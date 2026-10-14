//! Checks that an engine really works on this machine.
//!
//! ```text
//! cargo run -p polybrowser --example selftest -- [engine]
//! ```
//!
//! Opens a window for a few seconds and browses pages from a server it starts
//! on localhost, so it needs no network. It verifies that events arrive, that
//! history is recorded (including an address changed with `pushState`), that a
//! profile keeps its cookies between views and that another profile does not
//! see them, that the page inspector opens and closes, and that the extra view
//! features work: running a script with a result, snapshotting a hidden view
//! (hidden itself or through its parent), find-in-page, the cookie store
//! (host-only and domain cookies, and importing into a profile), and the
//! `LoadFailed`, `ProgressChanged`, `NavigationStateChanged`, `FaviconChanged`
//! and `CloseRequested` events, the last one from the main frame only. The
//! exit status is zero if everything held.

use std::{
    cell::Cell,
    io::{BufRead, BufReader, Write},
    net::{TcpListener, TcpStream},
    process::ExitCode,
    rc::Rc,
    sync::mpsc::{Receiver, Sender},
    thread,
    time::{Duration, Instant},
};

use polybrowser::{
    Browser, BrowserEvent, Cookie, Engine, FindOptions, HistoryFormat, HistoryQuery, Profile,
    ProfileManager, Rect,
};
use winit::{
    application::ApplicationHandler,
    dpi::LogicalSize,
    event::WindowEvent,
    event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy},
    window::{Window, WindowId},
};

const TIMEOUT: Duration = Duration::from_secs(45);

const FIRST_PAGE: &str = r#"<!doctype html><title>One</title><p>First page
<script>
  addEventListener("load", function () {
    setTimeout(function () {
      history.pushState({}, "", "/pushed");
      document.title = "Pushed";
    }, 300);
  });
</script>"#;

const FEATURE_PAGE: &str = r#"<!doctype html><title>Feature</title>
<link rel="icon" href="/favicon.ico">
<style>body { background: rgb(0, 160, 0); }</style>
<p>a needle hidden in the haystack
<iframe src="/frame" style="width: 1px; height: 1px; border: 0"></iframe>"#;

/// A frame inside the feature page that tries every way to close the view;
/// none may work.
const FRAME_PAGE: &str = r#"<!doctype html><script>
  try { window.close(); } catch (error) {}
  try { window.webkit.messageHandlers.polybrowserClose.postMessage(null); } catch (error) {}
  try { window.webkit.messageHandlers.ipc.postMessage("polybrowser:close-requested"); } catch (error) {}
</script>"#;

/// A script whose page has replaced `JSON.stringify`, so its result comes back
/// as a number instead of JSON text.
const TAMPERED_SCRIPT: &str = r#"
const original = JSON.stringify;
JSON.stringify = function (value) {
  if (value !== "not json") return original.apply(JSON, arguments);
  JSON.stringify = original;
  return 5;
};
return "not json";
"#;

/// A page that is green all over, so a snapshot of it shows whether it was
/// really painted.
#[cfg(target_os = "macos")]
const GREEN_PAGE: &str = "<!doctype html><body style=\"background: rgb(0, 160, 0)\">";

/// A host the test's cookies are set for but never sent to.
const COOKIE_HOST: &str = "example.test";

/// Starts the server the test browses and returns its origin.
fn serve() -> std::io::Result<String> {
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let address = listener.local_addr()?;
    thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            thread::spawn(move || {
                let _ = respond(stream);
            });
        }
    });
    Ok(format!("http://{address}"))
}

fn respond(mut stream: TcpStream) -> std::io::Result<()> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut request_line = String::new();
    reader.read_line(&mut request_line)?;
    let path = request_line
        .split_whitespace()
        .nth(1)
        .unwrap_or("/")
        .to_owned();

    let mut cookies = String::new();
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 || line == "\r\n" {
            break;
        }
        if let Some((name, value)) = line.split_once(':') {
            if name.eq_ignore_ascii_case("cookie") {
                cookies = value.trim().to_owned();
            }
        }
    }

    let (status, headers, body) = match path.as_str() {
        "/one" => (
            "200 OK",
            "Set-Cookie: seen=1; Max-Age=3600; Path=/\r\n",
            FIRST_PAGE.to_owned(),
        ),
        "/two" => (
            "200 OK",
            "",
            "<!doctype html><title>Two</title><p>Second page".to_owned(),
        ),
        "/feature" => ("200 OK", "", FEATURE_PAGE.to_owned()),
        "/frame" => ("200 OK", "", FRAME_PAGE.to_owned()),
        // Reports the cookies the browser sent, where the test can read them.
        "/whoami" => (
            "200 OK",
            "",
            format!("<!doctype html><title>cookies:{cookies}</title><p>{cookies}"),
        ),
        _ => (
            "404 Not Found",
            "",
            "<!doctype html><title>Not found</title>".to_owned(),
        ),
    };
    write!(
        stream,
        "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\n\
         Cache-Control: no-store\r\nConnection: close\r\n{headers}\r\n{body}",
        body.len()
    )
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Step {
    FirstPage,
    SecondPage,
    SameProfile,
    OtherProfile,
    InspectorOpening,
    InspectorClosing,
    Features,
}

/// The width, height and byte count of a snapshot, and whether its middle
/// pixel has the green of the pages it is taken of.
type Shape = (u32, u32, usize, bool);

fn shape(image: polybrowser::Image) -> Shape {
    let middle = ((image.height / 2 * image.width + image.width / 2) * 4) as usize;
    let green = image
        .rgba
        .get(middle..middle + 4)
        .is_some_and(|pixel| pixel[0] < 40 && pixel[1] > 120 && pixel[2] < 40);
    (image.width, image.height, image.rgba.len(), green)
}

/// Results of the asynchronous view operations, delivered from their callbacks.
enum Probe {
    Script(polybrowser::Result<String>),
    NumberScript(polybrowser::Result<String>),
    TamperedScript(polybrowser::Result<String>),
    Snapshot(polybrowser::Result<Shape>),
    #[cfg(target_os = "macos")]
    NestedLoaded,
    NestedSnapshot(polybrowser::Result<Shape>),
    Find(polybrowser::Result<bool>),
    CookieSet(polybrowser::Result<()>),
    CookiesGot(polybrowser::Result<Vec<Cookie>>),
    SubdomainCookiesGot(polybrowser::Result<Vec<Cookie>>),
    CookiesDeleted(polybrowser::Result<usize>),
    CookiesCleared(polybrowser::Result<()>),
    /// The result of a profile-level operation, and whether the call had
    /// returned before its callback ran.
    Imported(polybrowser::Result<usize>, bool),
    ImportedEphemeral(polybrowser::Result<usize>, bool),
    ClearedEphemeral(polybrowser::Result<()>, bool),
}

/// The cookies the probe sets: one host-only for the test server, which the
/// view must send there, and a host-only and a domain cookie for
/// [`COOKIE_HOST`], whose scope must survive the store.
fn probe_cookies() -> [Cookie; 3] {
    let cookie = |name: &str, domain: &str, http_only: bool| Cookie {
        name: name.into(),
        value: "1".into(),
        domain: domain.into(),
        path: "/".into(),
        secure: false,
        http_only,
        expires: None,
    };
    [
        cookie("probe", "127.0.0.1", true),
        cookie("hostonly", COOKIE_HOST, false),
        cookie("dotted", &format!(".{COOKIE_HOST}"), false),
    ]
}

/// Tracks the feature-checking phase.
#[derive(Default)]
struct Features {
    /// The hidden view the checks run against, created hidden and never shown.
    probe: Option<Browser>,
    /// A visible view inside a hidden parent view, the way an application
    /// keeps a tab it is not showing.
    nested: Option<Browser>,
    /// A view pointed at an unreachable port, to force `LoadFailed`.
    unreachable: Option<Browser>,
    started: bool,
    kicked_off: bool,
    whoami_sent: bool,
    get_sent: bool,
    delete_sent: bool,
    clear_sent: bool,
    finished_ops: bool,
    nested_loaded: bool,
    nested_snapshot_sent: bool,
    empty_snapshot_refused: bool,
    script: Option<String>,
    number_script: Option<polybrowser::Result<String>>,
    tampered_script: Option<polybrowser::Result<String>>,
    snapshot: Option<Shape>,
    nested_snapshot: Option<polybrowser::Result<Shape>>,
    find: Option<bool>,
    cookies_set: usize,
    cookie_set_failed: bool,
    /// The title of `/whoami` in the probe, listing the cookies it sent.
    sent_cookies: Option<String>,
    cookies_got: Option<Vec<Cookie>>,
    subdomain_cookies: Option<Vec<Cookie>>,
    cookies_deleted: Option<usize>,
    cookies_cleared: bool,
    imported: Option<(polybrowser::Result<usize>, bool)>,
    imported_ephemeral: Option<(polybrowser::Result<usize>, bool)>,
    cleared_ephemeral: Option<(polybrowser::Result<()>, bool)>,
}

/// A plain `NSView` added to the window and hidden.
#[cfg(target_os = "macos")]
struct HiddenParent(objc2::rc::Retained<objc2_app_kit::NSView>);

#[cfg(target_os = "macos")]
impl HiddenParent {
    fn new(window: &Window, width: f64, height: f64) -> Option<Self> {
        use objc2::MainThreadOnly;
        use objc2_app_kit::NSView;
        use objc2_foundation::{NSPoint, NSRect, NSSize};
        use raw_window_handle::{HasWindowHandle, RawWindowHandle};

        let RawWindowHandle::AppKit(handle) = window.window_handle().ok()?.as_raw() else {
            return None;
        };
        let mtm = objc2::MainThreadMarker::new()?;
        // SAFETY: winit's handle points at the window's live content view.
        let content = unsafe { handle.ns_view.cast::<NSView>().as_ref() };
        let frame = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(width, height));
        let view = NSView::initWithFrame(NSView::alloc(mtm), frame);
        content.addSubview(&view);
        view.setHidden(true);
        Some(Self(view))
    }
}

#[cfg(target_os = "macos")]
impl raw_window_handle::HasWindowHandle for HiddenParent {
    fn window_handle(
        &self,
    ) -> Result<raw_window_handle::WindowHandle<'_>, raw_window_handle::HandleError> {
        use raw_window_handle::{AppKitWindowHandle, RawWindowHandle, WindowHandle};
        let view = std::ptr::NonNull::from(&*self.0).cast();
        let raw = RawWindowHandle::AppKit(AppKitWindowHandle::new(view));
        // SAFETY: the view lives as long as `self`.
        Ok(unsafe { WindowHandle::borrow_raw(raw) })
    }
}

struct App {
    engine: Engine,
    origin: String,
    proxy: EventLoopProxy<BrowserEvent>,
    probe_tx: Sender<Probe>,
    probe_rx: Receiver<Probe>,
    main_profile: Profile,
    other_profile: Profile,
    step: Step,
    started: Instant,
    events: Vec<BrowserEvent>,
    failures: Vec<String>,
    completed: bool,
    features: Features,
    // Declared before the window so that it is dropped first.
    browser: Option<Browser>,
    window: Option<Window>,
}

impl App {
    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.origin)
    }

    fn expect(&mut self, holds: bool, what: &str) {
        println!("  {} {what}", if holds { "ok  " } else { "FAIL" });
        if !holds {
            self.failures.push(what.to_owned());
        }
    }

    fn saw(&self, event: &BrowserEvent) -> bool {
        self.events.contains(event)
    }

    /// Whether an event of the given shape was seen, ignoring its payload.
    fn saw_kind(&self, matches: impl Fn(&BrowserEvent) -> bool) -> bool {
        self.events.iter().any(matches)
    }

    /// Replaces the current view with a new one on `profile`.
    fn open(&mut self, profile: &Profile, path: &str) {
        self.browser = None;
        let window = self.window.as_ref().expect("the window exists");
        let size = window.inner_size().to_logical::<f64>(window.scale_factor());
        let proxy = self.proxy.clone();
        let browser = Browser::builder()
            .engine(self.engine)
            .profile(profile)
            .url(self.url(path))
            .on_event(move |event| {
                let _ = proxy.send_event(event);
            })
            .build(window, Rect::new(0.0, 0.0, size.width, size.height));
        match browser {
            Ok(browser) => self.browser = Some(browser),
            Err(error) => self
                .failures
                .push(format!("creating a view failed: {error}")),
        }
    }

    fn check_first_page(&mut self) {
        let browser = self.browser.as_ref().expect("a view is open");
        let (url, title) = (browser.url(), browser.title());
        let one = self.url("/one");
        let pushed = self.url("/pushed");
        self.expect(
            self.saw(&BrowserEvent::LoadStarted { url: one.clone() }),
            "the first page reported that it started loading",
        );
        self.expect(
            self.saw(&BrowserEvent::LoadFinished { url: one }),
            "the first page reported that it finished loading",
        );
        self.expect(
            self.saw(&BrowserEvent::UrlChanged {
                url: pushed.clone(),
            }),
            "an address changed with pushState was reported",
        );
        self.expect(url == pushed, "Browser::url follows the page");
        self.expect(title == "Pushed", "Browser::title follows the page");
    }

    fn check_second_page(&mut self) {
        let browser = self.browser.as_ref().expect("a view is open");
        let url = browser.url();
        let can_go_back = browser.can_go_back().unwrap_or(false);
        self.expect(url == self.url("/two"), "load_url navigated the view");
        self.expect(can_go_back, "the view has somewhere to go back to");

        let history = self.main_profile.history().expect("open the history");
        let entries = history.entries(&HistoryQuery::new()).unwrap_or_default();
        let title_of = |path: &str| {
            let url = self.url(path);
            entries
                .iter()
                .find(|entry| entry.url == url)
                .map(|entry| entry.title.clone().unwrap_or_default())
        };
        let titles = [title_of("/one"), title_of("/pushed"), title_of("/two")];
        self.expect(
            titles == ["One", "Pushed", "Two"].map(|title| Some(title.to_owned())),
            "history has the three pages with their titles",
        );
        self.expect(entries.len() == 3, "history has nothing else");

        let mut exported = Vec::new();
        let records = history
            .export(HistoryFormat::Json, &mut exported)
            .unwrap_or(0);
        self.expect(
            records == 3 && String::from_utf8_lossy(&exported).contains("/pushed"),
            "the history can be exported",
        );
    }

    /// Opens a hidden view and starts the asynchronous feature checks.
    fn start_features(&mut self) {
        self.step = Step::Features;
        self.features.started = true;
        let window = self.window.as_ref().expect("the window exists");
        let proxy = self.proxy.clone();
        let probe = Browser::builder()
            .engine(self.engine)
            .profile(&self.main_profile.clone())
            .url(self.url("/feature"))
            .visible(false) // created hidden and never shown
            .on_event(move |event| {
                let _ = proxy.send_event(event);
            })
            .build(window, Rect::new(0.0, 0.0, 480.0, 320.0));
        match probe {
            Ok(probe) => self.features.probe = Some(probe),
            Err(error) => self.failures.push(format!("hidden view failed: {error}")),
        }

        // A view with no size has nothing to capture, and must say so.
        let empty = Browser::builder()
            .engine(self.engine)
            .html("<p>empty")
            .visible(false)
            .build(window, Rect::new(0.0, 0.0, 0.0, 0.0));
        self.features.empty_snapshot_refused =
            empty.is_ok_and(|empty| empty.snapshot(|_| {}).is_err());

        self.open_nested();
    }

    /// Opens a visible view inside a hidden parent view, reporting its load
    /// through the probe channel.
    #[cfg(target_os = "macos")]
    fn open_nested(&mut self) {
        let window = self.window.as_ref().expect("the window exists");
        let Some(parent) = HiddenParent::new(window, 480.0, 320.0) else {
            self.failures.push("no hidden parent view".to_owned());
            return;
        };
        let tx = self.probe_tx.clone();
        let nested = Browser::builder()
            .engine(self.engine)
            .html(GREEN_PAGE)
            .on_event(move |event| {
                if matches!(event, BrowserEvent::LoadFinished { .. }) {
                    let _ = tx.send(Probe::NestedLoaded);
                }
            })
            .build(&parent, Rect::new(0.0, 0.0, 480.0, 320.0));
        match nested {
            Ok(nested) => self.features.nested = Some(nested),
            Err(error) => self.failures.push(format!("nested view failed: {error}")),
        }
    }

    /// Only macOS has a parent view to hide here.
    #[cfg(not(target_os = "macos"))]
    fn open_nested(&mut self) {
        self.features.nested_snapshot = Some(Ok((1, 1, 4, true)));
    }

    /// Fires the view operations once the hidden page has loaded.
    fn kick_off_features(&mut self) {
        self.features.kicked_off = true;
        let Some(probe) = self.features.probe.as_ref() else {
            return;
        };

        let tx = self.probe_tx.clone();
        let _ = probe.run_script(
            "return await new Promise(function (r) { setTimeout(function () { r(6 * 7); }, 10); });",
            move |result| drop(tx.send(Probe::Script(result))),
        );
        let tx = self.probe_tx.clone();
        let _ = probe.run_script("return 5;", move |result| {
            drop(tx.send(Probe::NumberScript(result)))
        });
        let tx = self.probe_tx.clone();
        let _ = probe.run_script(TAMPERED_SCRIPT, move |result| {
            drop(tx.send(Probe::TamperedScript(result)))
        });
        let tx = self.probe_tx.clone();
        let _ = probe.snapshot(move |result| {
            let _ = tx.send(Probe::Snapshot(result.map(shape)));
        });
        let tx = self.probe_tx.clone();
        let _ = probe.find("needle", FindOptions::default(), move |result| {
            drop(tx.send(Probe::Find(result)))
        });

        for cookie in probe_cookies() {
            let tx = self.probe_tx.clone();
            let _ = probe.set_cookie(cookie, move |result| {
                drop(tx.send(Probe::CookieSet(result)))
            });
        }
        self.start_profile_operations();
    }

    /// Imports a cookie into the probe's profile, and runs the operations
    /// that have nothing to do for an ephemeral profile.
    fn start_profile_operations(&mut self) {
        let returned = Rc::new(Cell::new(false));
        let imported = Cookie {
            name: "imported".into(),
            ..probe_cookies()[0].clone()
        };
        let (tx, seen) = (self.probe_tx.clone(), returned.clone());
        let main = polybrowser::import_cookies(
            self.engine,
            &self.main_profile,
            vec![imported],
            move |r| drop(tx.send(Probe::Imported(r, seen.get()))),
        );
        let (tx, seen) = (self.probe_tx.clone(), returned.clone());
        let ephemeral = polybrowser::import_cookies(
            self.engine,
            &Profile::ephemeral(),
            probe_cookies().to_vec(),
            move |r| drop(tx.send(Probe::ImportedEphemeral(r, seen.get()))),
        );
        let (tx, seen) = (self.probe_tx.clone(), returned.clone());
        let cleared = polybrowser::clear_site_data(self.engine, &Profile::ephemeral(), move |r| {
            drop(tx.send(Probe::ClearedEphemeral(r, seen.get())))
        });
        returned.set(true);

        // An engine without these says so at once, and never calls back.
        if let Err(error) = main {
            self.features.imported = Some((Err(error), true));
        }
        if let Err(error) = ephemeral {
            self.features.imported_ephemeral = Some((Err(error), true));
        }
        if let Err(error) = cleared {
            self.features.cleared_ephemeral = Some((Err(error), true));
        }
    }

    /// Drives the cookie operations in order as their results arrive, then the
    /// `CloseRequested` and `LoadFailed` triggers.
    fn advance_features(&mut self) {
        while let Ok(result) = self.probe_rx.try_recv() {
            match result {
                Probe::Script(r) => self.features.script = r.ok(),
                Probe::NumberScript(r) => self.features.number_script = Some(r),
                Probe::TamperedScript(r) => self.features.tampered_script = Some(r),
                Probe::Snapshot(r) => self.features.snapshot = r.ok(),
                #[cfg(target_os = "macos")]
                Probe::NestedLoaded => self.features.nested_loaded = true,
                Probe::NestedSnapshot(r) => self.features.nested_snapshot = Some(r),
                Probe::Find(r) => self.features.find = r.ok(),
                Probe::CookieSet(r) => {
                    self.features.cookies_set += 1;
                    self.features.cookie_set_failed |= r.is_err();
                }
                Probe::CookiesGot(r) => self.features.cookies_got = r.ok(),
                Probe::SubdomainCookiesGot(r) => self.features.subdomain_cookies = r.ok(),
                Probe::CookiesDeleted(r) => self.features.cookies_deleted = r.ok(),
                Probe::CookiesCleared(r) => self.features.cookies_cleared = r.is_ok(),
                Probe::Imported(r, returned) => self.features.imported = Some((r, returned)),
                Probe::ImportedEphemeral(r, returned) => {
                    self.features.imported_ephemeral = Some((r, returned))
                }
                Probe::ClearedEphemeral(r, returned) => {
                    self.features.cleared_ephemeral = Some((r, returned))
                }
            }
        }

        if self.features.nested_loaded && !self.features.nested_snapshot_sent {
            self.features.nested_snapshot_sent = true;
            if let Some(nested) = self.features.nested.as_ref() {
                let tx = self.probe_tx.clone();
                let sent = nested.snapshot(move |result| {
                    let _ = tx.send(Probe::NestedSnapshot(result.map(shape)));
                });
                if let Err(error) = sent {
                    self.features.nested_snapshot = Some(Err(error));
                }
            }
        }

        let Some(probe) = self.features.probe.as_ref() else {
            return;
        };

        // The server lists the cookies it was sent in the page title. Leaving
        // the page waits for the checks that run in it.
        let page_checked = self.features.script.is_some()
            && self.features.number_script.is_some()
            && self.features.tampered_script.is_some()
            && self.features.snapshot.is_some()
            && self.features.find.is_some();
        if page_checked
            && self.features.cookies_set == probe_cookies().len()
            && self.features.imported.is_some()
            && !self.features.whoami_sent
        {
            self.features.whoami_sent = true;
            let _ = probe.load_url(&self.url("/whoami"));
        }
        if self.features.sent_cookies.is_some() && !self.features.get_sent {
            self.features.get_sent = true;
            let tx = self.probe_tx.clone();
            let _ = probe.cookies(None, move |r| drop(tx.send(Probe::CookiesGot(r))));
            let tx = self.probe_tx.clone();
            let subdomain = format!("http://sub.{COOKIE_HOST}/");
            let _ = probe.cookies(Some(&subdomain), move |r| {
                drop(tx.send(Probe::SubdomainCookiesGot(r)))
            });
        }
        if self.features.cookies_got.is_some()
            && self.features.subdomain_cookies.is_some()
            && !self.features.delete_sent
        {
            self.features.delete_sent = true;
            let tx = self.probe_tx.clone();
            let _ = probe.delete_cookies("probe", None, move |r| {
                drop(tx.send(Probe::CookiesDeleted(r)))
            });
        }
        if self.features.cookies_deleted.is_some() && !self.features.clear_sent {
            self.features.clear_sent = true;
            let tx = self.probe_tx.clone();
            let _ = probe.clear_cookies(move |r| drop(tx.send(Probe::CookiesCleared(r))));
        }
        if self.features.cookies_cleared && !self.features.finished_ops {
            self.features.finished_ops = true;
            // Ask the page to close itself, forcing `CloseRequested`.
            let _ = probe.run_script("window.close(); return null;", |_| {});
            // Navigate to a second page so the back list, and thus the
            // navigation state, changes.
            let _ = probe.load_url(&self.url("/two"));
            // A view pointed at a closed port forces `LoadFailed`.
            self.open_unreachable();
        }
    }

    fn open_unreachable(&mut self) {
        let window = self.window.as_ref().expect("the window exists");
        let proxy = self.proxy.clone();
        let failing = Browser::builder()
            .engine(self.engine)
            .profile(&self.main_profile.clone())
            .url("http://127.0.0.1:1/")
            .visible(false)
            .on_event(move |event| {
                let _ = proxy.send_event(event);
            })
            .build(window, Rect::new(0.0, 0.0, 480.0, 320.0));
        if let Ok(failing) = failing {
            self.features.unreachable = Some(failing);
        }
    }

    /// Whether every feature result and event has arrived.
    fn features_complete(&self) -> bool {
        self.features.finished_ops
            && self.features.script.is_some()
            && self.features.number_script.is_some()
            && self.features.tampered_script.is_some()
            && self.features.snapshot.is_some()
            && self.features.nested_snapshot.is_some()
            && self.features.imported_ephemeral.is_some()
            && self.features.cleared_ephemeral.is_some()
            && self.features.find.is_some()
            && self.features.cookies_got.is_some()
            && self.features.cookies_deleted.is_some()
            && self.features.cookies_cleared
            && self.saw_kind(|event| matches!(event, BrowserEvent::LoadFailed { .. }))
            && self.saw_kind(|event| matches!(event, BrowserEvent::CloseRequested))
            && self.saw_kind(|event| matches!(event, BrowserEvent::ProgressChanged { .. }))
            && self.saw_kind(|event| matches!(event, BrowserEvent::FaviconChanged { .. }))
            && self.saw_kind(|event| matches!(event, BrowserEvent::NavigationStateChanged { .. }))
    }

    fn check_features(&mut self) {
        let good = |(width, height, len, green): Shape| {
            width > 0 && height > 0 && len == (width * height * 4) as usize && green
        };
        self.expect(
            self.features.script.as_deref() == Some("42"),
            "run_script returned the awaited promise's value",
        );
        self.expect(
            matches!(&self.features.number_script, Some(Ok(json)) if json == "5"),
            "run_script returned a number as JSON",
        );
        self.expect(
            matches!(self.features.tampered_script, Some(Err(_))),
            "a page whose JSON.stringify returns a number gets an error",
        );
        self.expect(
            self.features.snapshot.is_some_and(good),
            "a hidden view was snapshotted",
        );
        self.expect(
            matches!(self.features.nested_snapshot, Some(Ok(shape)) if good(shape)),
            "a view in a hidden parent was snapshotted",
        );
        self.expect(
            self.features.empty_snapshot_refused,
            "a view with no size refused to snapshot",
        );
        self.expect(self.features.find == Some(true), "find matched in the page");
        self.expect(!self.features.cookie_set_failed, "the cookies were set");
        let cookies = self.features.cookies_got.clone().unwrap_or_default();
        let has = |cookies: &[Cookie], name: &str, domain: &str| {
            cookies.iter().any(|c| c.name == name && c.domain == domain)
        };
        self.expect(
            cookies.iter().any(|c| c.name == "probe" && c.http_only),
            "a cookie set with http_only was read back",
        );
        let sent = self.features.sent_cookies.clone().unwrap_or_default();
        let sent = sent.strip_prefix("cookies:").unwrap_or_default();
        self.expect(
            sent.split("; ").any(|c| c == "probe=1"),
            "a host-only cookie was sent to its host",
        );
        self.expect(
            has(&cookies, "hostonly", COOKIE_HOST),
            "a host-only cookie stayed host-only",
        );
        self.expect(
            has(&cookies, "dotted", &format!(".{COOKIE_HOST}")),
            "a domain cookie kept its domain",
        );
        self.expect(
            matches!(self.features.imported, Some((Ok(1), true)))
                && sent.split("; ").any(|c| c == "imported=1"),
            "import_cookies stored the cookie, then reported it",
        );
        self.expect(
            matches!(self.features.imported_ephemeral, Some((Ok(0), true))),
            "import_cookies into an ephemeral profile reported nothing, later",
        );
        self.expect(
            matches!(self.features.cleared_ephemeral, Some((Ok(()), true))),
            "clear_site_data of an ephemeral profile reported later",
        );
        let subdomain = self.features.subdomain_cookies.clone().unwrap_or_default();
        self.expect(
            subdomain.iter().any(|c| c.name == "dotted")
                && !subdomain.iter().any(|c| c.name == "hostonly"),
            "only the domain cookie applies to a subdomain",
        );
        self.expect(
            self.features.cookies_deleted == Some(1),
            "the cookie was deleted",
        );
        self.expect(
            self.features.cookies_cleared,
            "the cookie store was cleared",
        );
        self.expect(
            self.saw_kind(|event| matches!(event, BrowserEvent::LoadFailed { .. })),
            "LoadFailed was reported for an unreachable page",
        );
        let closes = self
            .events
            .iter()
            .filter(|event| matches!(event, BrowserEvent::CloseRequested))
            .count();
        self.expect(
            closes == 1,
            "CloseRequested was reported for the main frame's window.close() only",
        );
        self.expect(
            self.saw_kind(|event| matches!(event, BrowserEvent::ProgressChanged { .. })),
            "ProgressChanged was reported",
        );
        self.expect(
            self.saw_kind(|event| matches!(event, BrowserEvent::FaviconChanged { .. })),
            "FaviconChanged was reported",
        );
        self.expect(
            self.saw_kind(|event| matches!(event, BrowserEvent::NavigationStateChanged { .. })),
            "NavigationStateChanged was reported",
        );
        self.completed = true;
        self.features.probe = None;
        self.features.nested = None;
        self.features.unreachable = None;
    }
}

impl ApplicationHandler<BrowserEvent> for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        let attributes = Window::default_attributes()
            .with_title(format!("polybrowser self-test ({})", self.engine))
            .with_inner_size(LogicalSize::new(640.0, 420.0));
        self.window = Some(event_loop.create_window(attributes).expect("create window"));
        let profile = self.main_profile.clone();
        self.open(&profile, "/one");
    }

    fn user_event(&mut self, _event_loop: &ActiveEventLoop, event: BrowserEvent) {
        println!("  {:?}: {event:?}", self.step);
        self.events.push(event.clone());

        // The hidden page finishing its load starts the feature operations.
        if self.step == Step::Features && !self.features.kicked_off {
            if let BrowserEvent::LoadFinished { url } = &event {
                if url == &self.url("/feature") {
                    self.kick_off_features();
                }
            }
        }

        let BrowserEvent::TitleChanged { title } = event else {
            return;
        };

        match self.step {
            Step::FirstPage if title == "Pushed" => {
                self.check_first_page();
                let next = self.url("/two");
                let navigated = self.browser.as_ref().map(|browser| browser.load_url(&next));
                self.expect(matches!(navigated, Some(Ok(()))), "load_url was accepted");
                self.step = Step::SecondPage;
            }
            Step::SecondPage if title == "Two" => {
                self.check_second_page();
                let profile = self.main_profile.clone();
                self.open(&profile, "/whoami");
                self.step = Step::SameProfile;
            }
            Step::SameProfile if title.starts_with("cookies:") => {
                self.expect(
                    title == "cookies:seen=1",
                    "a new view on the same profile has its cookies",
                );
                let profile = self.other_profile.clone();
                self.open(&profile, "/whoami");
                self.step = Step::OtherProfile;
            }
            Step::Features if title.starts_with("cookies:") => {
                self.features.sent_cookies = Some(title);
            }
            Step::OtherProfile if title.starts_with("cookies:") => {
                self.expect(title == "cookies:", "a view on another profile does not");
                let opened = self.browser.as_ref().map(Browser::open_devtools);
                self.expect(matches!(opened, Some(Ok(()))), "open_devtools was accepted");
                self.step = Step::InspectorOpening;
            }
            _ => {}
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        if matches!(event, WindowEvent::CloseRequested) {
            self.failures
                .push("the window was closed before the test finished".to_owned());
            self.browser = None;
            event_loop.exit();
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        polybrowser::pump();
        // The inspector reports nothing, so its two steps are polled.
        let inspecting = self.browser.as_ref().map(Browser::is_devtools_open);
        match (self.step, inspecting) {
            (Step::InspectorOpening, Some(true)) => {
                self.expect(true, "the inspector opened");
                let closed = self.browser.as_ref().map(Browser::close_devtools);
                self.expect(
                    matches!(closed, Some(Ok(()))),
                    "close_devtools was accepted",
                );
                self.step = Step::InspectorClosing;
            }
            (Step::InspectorClosing, Some(false)) => {
                self.expect(true, "the inspector closed");
                // The main view's inspector is done; move to the feature phase.
                self.browser = None;
                self.start_features();
            }
            _ => {}
        }

        if self.step == Step::Features && self.features.kicked_off {
            self.advance_features();
            if !self.completed && self.features_complete() {
                self.check_features();
                event_loop.exit();
            }
        }

        if !self.failures.is_empty() && self.browser.is_none() && !self.features.started {
            // Creating a view failed; nothing more will happen.
            event_loop.exit();
        } else if self.started.elapsed() > TIMEOUT {
            self.failures
                .push(format!("timed out waiting in step {:?}", self.step));
            self.browser = None;
            self.features.probe = None;
            self.features.nested = None;
            self.features.unreachable = None;
            event_loop.exit();
        }
        event_loop.set_control_flow(ControlFlow::wait_duration(Duration::from_millis(20)));
    }
}

fn run() -> Result<Vec<String>, Box<dyn std::error::Error>> {
    let engine: Engine = match std::env::args().nth(1) {
        Some(name) => name.parse()?,
        None => polybrowser::default_engine().ok_or("no engine compiled in")?,
    };
    println!(
        "Testing the {engine} engine (compiled in: {:?})",
        polybrowser::engines()
    );

    let origin = serve()?;
    let data = tempfile::tempdir()?;
    let profiles = ProfileManager::open_at(data.path().join("profiles"))?;
    let main_profile = profiles.create("Main")?;
    let other_profile = profiles.create("Other")?;

    let event_loop = EventLoop::<BrowserEvent>::with_user_event().build()?;
    let (probe_tx, probe_rx) = std::sync::mpsc::channel();
    let mut app = App {
        engine,
        origin,
        proxy: event_loop.create_proxy(),
        probe_tx,
        probe_rx,
        main_profile: main_profile.clone(),
        other_profile: other_profile.clone(),
        step: Step::FirstPage,
        started: Instant::now(),
        events: Vec::new(),
        failures: Vec::new(),
        completed: false,
        features: Features::default(),
        browser: None,
        window: None,
    };
    event_loop.run_app(&mut app)?;

    let (completed, mut failures) = (app.completed, std::mem::take(&mut app.failures));
    drop(app);
    polybrowser::shutdown();
    if !completed && failures.is_empty() {
        failures.push("the event loop ended before the test finished".to_owned());
    }

    // WKWebView keeps site data in a store of its own; the other engines must
    // have written into the directory the profile gave them.
    let stores_in_profile_dir = !(engine == Engine::WebKit && cfg!(target_os = "macos"));
    if completed && stores_in_profile_dir {
        let data_dir = main_profile
            .engine_data_dir(engine)
            .expect("a persistent profile");
        let wrote_there =
            std::fs::read_dir(&data_dir).is_ok_and(|mut entries| entries.next().is_some());
        println!(
            "  {} the engine stored site data in the profile's directory",
            if wrote_there { "ok  " } else { "FAIL" }
        );
        if !wrote_there {
            failures.push(format!("nothing was written to {}", data_dir.display()));
        }
    }

    let other_id = other_profile.id();
    drop((main_profile, other_profile));
    let deleted = profiles.delete(other_id).is_ok() && profiles.list()?.len() == 1;
    println!(
        "  {} a profile can be deleted",
        if deleted { "ok  " } else { "FAIL" }
    );
    if !deleted {
        failures.push("deleting a profile failed".to_owned());
    }
    Ok(failures)
}

fn main() -> ExitCode {
    // Must come first: Chromium's helper processes re-enter `main`.
    polybrowser::init();

    match run() {
        Ok(failures) if failures.is_empty() => {
            println!("PASS");
            ExitCode::SUCCESS
        }
        Ok(failures) => {
            println!("FAIL");
            for failure in failures {
                println!("  - {failure}");
            }
            ExitCode::FAILURE
        }
        Err(error) => {
            println!("FAIL: {error}");
            ExitCode::FAILURE
        }
    }
}
