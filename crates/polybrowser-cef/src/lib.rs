//! Chromium as a `polybrowser` engine, through the
//! [Chromium Embedded Framework](https://github.com/chromiumembedded/cef).
//!
//! Applications normally use this through the `polybrowser` crate with its
//! `chromium` feature. What that brings with it:
//!
//! - **A build dependency.** CEF is downloaded when the crate is first built
//!   (about 1 GB unpacked) and its C++ glue is compiled, which needs CMake
//!   and Ninja.
//! - **Helper processes.** Chromium renders pages in separate processes. On
//!   macOS these are separate executables inside the application bundle; the
//!   `polybrowser-cef-helper` crate is that executable, and
//!   `polybrowser-bundler` puts a bundle together.
//! - **One data root per process.** All profiles used with Chromium in one
//!   process must come from the same `ProfileManager`.
//!
//! Only macOS is implemented so far.

#[cfg(target_os = "macos")]
mod appkit;
#[cfg(target_os = "macos")]
mod pump;
#[cfg(target_os = "macos")]
mod runtime;
#[cfg(target_os = "macos")]
mod view;

#[cfg(target_os = "macos")]
pub use imp::{run_helper, CefBackend};

/// Does the work of a Chromium child process if this process is one.
///
/// Does nothing on macOS, where child processes are separate executables (see
/// [`run_helper`]). It exists so that applications are already written the
/// way Windows and Linux will need.
pub fn init() {}

/// Lets Chromium do pending work. Does nothing on macOS, where Chromium is
/// driven from the main run loop without the application's help.
pub fn pump() {}

/// Closes every Chromium view and shuts Chromium down, flushing cookies and
/// site data to disk. Chromium cannot be started again in the same process.
pub fn shutdown() {
    #[cfg(target_os = "macos")]
    runtime::shutdown();
}

#[cfg(target_os = "macos")]
mod imp {
    use polybrowser_core::{
        backend::{EngineBackend, EventSink, ViewBackend, ViewConfig},
        Engine, Result,
    };
    use raw_window_handle::WindowHandle;

    use crate::{appkit, view};

    /// Creates views rendered by Chromium.
    #[derive(Debug, Default, Clone, Copy)]
    pub struct CefBackend;

    impl CefBackend {
        pub fn new() -> Self {
            Self
        }
    }

    impl EngineBackend for CefBackend {
        fn engine(&self) -> Engine {
            Engine::Chromium
        }

        fn create_view(
            &self,
            parent: WindowHandle<'_>,
            config: ViewConfig,
            events: EventSink,
        ) -> Result<Box<dyn ViewBackend>> {
            view::create(parent, config, events)
        }
    }

    /// The whole `main` of a Chromium helper executable. Returns the exit
    /// status for the process.
    ///
    /// Chromium starts helpers for rendering, GPU work and networking; they
    /// are never started by hand.
    pub fn run_helper() -> i32 {
        use cef::{args::Args, *};

        let args = Args::new();

        // The sandbox has to be entered before Chromium itself is loaded.
        #[cfg(feature = "sandbox")]
        let _sandbox = {
            let mut sandbox = cef::sandbox::Sandbox::new();
            sandbox.initialize(args.as_main_args());
            sandbox
        };

        if let Err(error) = appkit::load_framework(appkit::Role::Helper) {
            eprintln!("polybrowser: {error}");
            return 1;
        }
        let _ = api_hash(sys::CEF_API_VERSION_LAST, 0);
        execute_process(
            Some(args.as_main_args()),
            None::<&mut App>,
            std::ptr::null_mut(),
        )
    }
}
