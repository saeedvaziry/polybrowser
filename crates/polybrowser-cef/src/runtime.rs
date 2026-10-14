//! Starting and stopping Chromium, and its per-profile request contexts.

use std::{
    cell::RefCell,
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
    ptr,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    thread,
    time::{Duration, Instant},
};

use cef::{args::Args, *};
use objc2::MainThreadMarker;
use polybrowser_core::{backend::ProfileStorage, Engine, Error, ProfileId, Result};

use crate::{appkit, pump, view};

/// How long to wait for Chromium to start or to open a profile.
const READY_TIMEOUT: Duration = Duration::from_secs(20);
/// How long to wait for views to close at shutdown.
const CLOSE_TIMEOUT: Duration = Duration::from_secs(5);

struct Runtime {
    /// Chromium's `root_cache_path`: every profile directory must be a child.
    root: PathBuf,
    /// The root was made up for a process that only browses privately.
    temporary_root: bool,
    contexts: HashMap<ProfileId, RequestContext>,
    /// Chromium keeps pointers into the command line it was started with.
    _args: Args,
}

thread_local! {
    static RUNTIME: RefCell<Option<Runtime>> = const { RefCell::new(None) };
}

/// Chromium cannot be initialised twice in one process.
static SHUT_DOWN: AtomicBool = AtomicBool::new(false);

pub(crate) fn error(message: impl std::fmt::Display) -> Error {
    Error::backend(Engine::Chromium, message)
}

fn path_string(path: &Path) -> Result<CefString> {
    path.to_str()
        .map(CefString::from)
        .ok_or_else(|| error(format!("{} is not valid UTF-8", path.display())))
}

fn other_root(used: &Path, wanted: &Path) -> Error {
    error(format!(
        "Chromium keeps all profiles of a process under one directory and is already using {}; \
         a profile stored under {} cannot be used in the same process. Take all profiles, \
         private ones included, from the same ProfileManager.",
        used.display(),
        wanted.display()
    ))
}

wrap_app! {
    struct ChromiumApp {
        ready: Arc<AtomicBool>,
        root: CefString,
    }

    impl App {
        fn on_before_command_line_processing(
            &self,
            _process_type: Option<&CefString>,
            command_line: Option<&mut CommandLine>,
        ) {
            let Some(command_line) = command_line else {
                return;
            };
            // At every start Chrome copies its whole application bundle to a
            // temporary directory, so that replacing the application during
            // an update does not invalidate the running code's signature,
            // and starts a helper to remove the copy afterwards. That helper
            // knows nothing of our data root and creates a directory shared
            // by all CEF applications, ~/Library/Application Support/CEF.
            // An embedded browser is updated with its host; turn this off,
            // keeping whatever features the application disabled itself.
            let name = CefString::from("disable-features");
            let mut disabled = CefString::from(&command_line.switch_value(Some(&name))).to_string();
            if !disabled.is_empty() {
                disabled.push(',');
            }
            disabled.push_str("MacAppCodeSignClone");
            command_line.append_switch_with_value(Some(&name), Some(&CefString::from(disabled.as_str())));
        }

        fn browser_process_handler(&self) -> Option<BrowserProcessHandler> {
            Some(ProcessHandler::new(self.ready.clone(), self.root.clone()))
        }
    }
}

wrap_browser_process_handler! {
    struct ProcessHandler {
        ready: Arc<AtomicBool>,
        root: CefString,
    }

    impl BrowserProcessHandler {
        fn on_context_initialized(&self) {
            self.ready.store(true, Ordering::Release);
        }

        fn on_before_child_process_launch(&self, command_line: Option<&mut CommandLine>) {
            // Helpers are not told about `root_cache_path` in the settings;
            // they find the data root on their command line.
            if let Some(command_line) = command_line {
                command_line.append_switch_with_value(
                    Some(&CefString::from("user-data-dir")),
                    Some(&self.root),
                );
            }
        }

        fn on_schedule_message_pump_work(&self, delay_ms: i64) {
            pump::schedule(delay_ms);
        }
    }
}

wrap_request_context_handler! {
    struct ContextHandler {
        ready: Arc<AtomicBool>,
    }

    impl RequestContextHandler {
        fn on_request_context_initialized(&self, _request_context: Option<&mut RequestContext>) {
            self.ready.store(true, Ordering::Release);
        }
    }
}

/// Starts Chromium if it is not running yet.
///
/// `engine_root` is where the profiles of the application live, if the view
/// being created belongs to a `ProfileManager`.
pub(crate) fn ensure_started(engine_root: Option<&Path>) -> Result<()> {
    let mtm = MainThreadMarker::new().ok_or(Error::NotMainThread)?;

    let running_root = RUNTIME.with(|runtime| runtime.borrow().as_ref().map(|r| r.root.clone()));
    if let Some(root) = running_root {
        return match engine_root {
            Some(wanted) if wanted != root => Err(other_root(&root, wanted)),
            _ => Ok(()),
        };
    }
    if SHUT_DOWN.load(Ordering::Acquire) {
        return Err(error(
            "Chromium was shut down and cannot be started again in this process",
        ));
    }

    let (root, temporary_root) = match engine_root {
        Some(root) => (root.to_owned(), false),
        None => (
            std::env::temp_dir().join(format!("polybrowser-chromium-{}", std::process::id())),
            true,
        ),
    };
    fs::create_dir_all(&root)?;
    let root = root.canonicalize()?;

    appkit::load_framework(appkit::Role::Main).map_err(error)?;
    let _ = api_hash(sys::CEF_API_VERSION_LAST, 0);
    appkit::adopt_cef_app_protocol(mtm).map_err(error)?;

    let args = Args::new();
    let root_string = path_string(&root)?;
    let settings = Settings {
        no_sandbox: (!cfg!(feature = "sandbox")).into(),
        external_message_pump: 1,
        root_cache_path: root_string.clone(),
        ..Default::default()
    };
    let ready = Arc::new(AtomicBool::new(false));
    let mut app = ChromiumApp::new(ready.clone(), root_string);
    if initialize(
        Some(args.as_main_args()),
        Some(&settings),
        Some(&mut app),
        ptr::null_mut(),
    ) != 1
    {
        // Typically another process is using the same data directory.
        return Err(error(format!(
            "Chromium failed to start (exit code {}); is another instance of the application \
             running with the same profiles?",
            get_exit_code()
        )));
    }

    RUNTIME.with(|runtime| {
        *runtime.borrow_mut() = Some(Runtime {
            root,
            temporary_root,
            contexts: HashMap::new(),
            _args: args,
        });
    });
    pump::start();
    wait_for(&ready, "Chromium to start")
}

/// The request context (cookie jar, cache, storage) for a profile.
pub(crate) fn request_context(storage: &ProfileStorage) -> Result<RequestContext> {
    let ProfileStorage::Persistent { id, data_dir } = storage else {
        // An empty cache path keeps everything in memory.
        return new_context(None);
    };

    let (cached, root) = RUNTIME.with(|runtime| {
        let runtime = runtime.borrow();
        let runtime = runtime
            .as_ref()
            .expect("Chromium is started before contexts are made");
        (runtime.contexts.get(id).cloned(), runtime.root.clone())
    });
    if let Some(context) = cached {
        return Ok(context);
    }

    let data_dir = data_dir.canonicalize()?;
    if data_dir.parent() != Some(root.as_path()) {
        return Err(other_root(&root, &data_dir));
    }
    let context = new_context(Some(&data_dir))?;
    RUNTIME.with(|runtime| {
        if let Some(runtime) = runtime.borrow_mut().as_mut() {
            runtime.contexts.insert(*id, context.clone());
        }
    });
    Ok(context)
}

fn new_context(cache_path: Option<&Path>) -> Result<RequestContext> {
    let mut settings = RequestContextSettings::default();
    if let Some(cache_path) = cache_path {
        settings.cache_path = path_string(cache_path)?;
    }
    let ready = Arc::new(AtomicBool::new(false));
    let mut handler = ContextHandler::new(ready.clone());
    let context = request_context_create_context(Some(&settings), Some(&mut handler))
        .ok_or_else(|| error("Chromium could not open the profile"))?;
    // A view can only be created synchronously in a context that is ready.
    wait_for(&ready, "Chromium to open the profile")?;
    Ok(context)
}

/// Runs Chromium's message loop until `flag` is set.
fn wait_for(flag: &AtomicBool, what: &str) -> Result<()> {
    let deadline = Instant::now() + READY_TIMEOUT;
    while !flag.load(Ordering::Acquire) {
        if Instant::now() > deadline {
            return Err(error(format!("timed out waiting for {what}")));
        }
        pump::work_now();
        thread::sleep(Duration::from_millis(2));
    }
    Ok(())
}

pub(crate) fn shutdown() {
    let Some(runtime) = RUNTIME.with(|runtime| runtime.borrow_mut().take()) else {
        return;
    };
    SHUT_DOWN.store(true, Ordering::Release);

    // Closing is asynchronous: ask, then keep Chromium running until done.
    view::close_all();
    let deadline = Instant::now() + CLOSE_TIMEOUT;
    while view::open_count() > 0 && Instant::now() < deadline {
        pump::work_now();
        thread::sleep(Duration::from_millis(2));
    }

    let Runtime {
        root,
        temporary_root,
        contexts,
        _args,
    } = runtime;
    // Chromium wants every reference to its objects gone before it stops.
    drop(contexts);
    pump::work_now();
    pump::stop();
    cef::shutdown();
    if temporary_root {
        let _ = fs::remove_dir_all(root);
    }
}
