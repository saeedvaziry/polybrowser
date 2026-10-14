//! Chromium in a winit window.
//!
//! Chromium is not part of the system: the `chromium` feature compiles it in,
//! and the application ships it. On macOS that means an application bundle,
//! so `cargo run` cannot start this example by itself. `polybrowser-bundle`
//! builds the bundle and starts it:
//!
//! ```text
//! cargo run -p polybrowser-bundler -- \
//!     --package polybrowser --example chromium --features chromium \
//!     --run -- [url] [--profile NAME] [--devtools] [--use-mock-keychain]
//! ```
//!
//! Without a URL it opens `chrome://version`, Chromium's account of itself.
//! Without `--profile` it browses privately and leaves nothing on disk; with
//! it, cookies and site data are kept in a profile of that name and are there
//! again on the next run. `--devtools` opens DevTools as well.
//!
//! Chromium reads the same command line, and switches this example does not
//! know are left to it. `--use-mock-keychain` is one worth knowing: Chromium
//! encrypts cookies with a key from the macOS keychain and asks for access to
//! it, and this switch makes it use a fixed key instead.

use std::process::ExitCode;

use polybrowser::{Browser, BrowserEvent, Engine, Profile, ProfileManager, Rect};
use winit::{
    application::ApplicationHandler,
    dpi::LogicalSize,
    event::WindowEvent,
    event_loop::{ActiveEventLoop, EventLoop},
    window::{Window, WindowId},
};

/// Names the directory the profiles are kept in.
const APP_ID: &str = "dev.polybrowser.examples";

struct Options {
    url: String,
    profile: Option<String>,
    devtools: bool,
}

impl Options {
    fn from_arguments() -> Self {
        let mut options = Options {
            url: "chrome://version".to_owned(),
            profile: None,
            devtools: false,
        };
        let mut arguments = std::env::args().skip(1);
        while let Some(argument) = arguments.next() {
            match argument.as_str() {
                "--profile" => options.profile = arguments.next(),
                "--devtools" => options.devtools = true,
                // For Chromium.
                switch if switch.starts_with("--") => {}
                url => options.url = url.to_owned(),
            }
        }
        options
    }
}

struct App {
    options: Options,
    /// `None` browses privately.
    profile: Option<Profile>,
    failure: Option<polybrowser::Error>,
    // The browser is declared first so that it is dropped before its window.
    browser: Option<Browser>,
    window: Option<Window>,
}

impl App {
    fn view_bounds(window: &Window) -> Rect {
        let size = window.inner_size().to_logical::<f64>(window.scale_factor());
        Rect::new(0.0, 0.0, size.width, size.height)
    }

    fn open_browser(&self, window: &Window) -> polybrowser::Result<Browser> {
        let mut builder = Browser::builder()
            .engine(Engine::Chromium)
            .url(&self.options.url)
            // On by default in debug builds only.
            .devtools(true)
            .on_event(|event| match event {
                BrowserEvent::UrlChanged { url } => println!("address  {url}"),
                BrowserEvent::TitleChanged { title } => println!("title    {title}"),
                BrowserEvent::LoadFailed { url, error } => println!("failed   {url}: {error}"),
                _ => {}
            });
        if let Some(profile) = &self.profile {
            builder = builder.profile(profile);
        }
        // The first Chromium view starts Chromium, which takes a moment.
        let browser = builder.build(window, Self::view_bounds(window))?;
        if self.options.devtools {
            browser.open_devtools()?;
        }
        Ok(browser)
    }
}

impl ApplicationHandler for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        let attributes = Window::default_attributes()
            .with_title("polybrowser (chromium)")
            .with_inner_size(LogicalSize::new(1100.0, 760.0));
        let window = event_loop.create_window(attributes).expect("create window");

        match self.open_browser(&window) {
            Ok(browser) => self.browser = Some(browser),
            Err(error) => {
                self.failure = Some(error);
                event_loop.exit();
            }
        }
        self.window = Some(window);
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::Resized(_) => {
                if let (Some(window), Some(browser)) = (&self.window, &self.browser) {
                    let _ = browser.set_bounds(Self::view_bounds(window));
                }
            }
            WindowEvent::CloseRequested => event_loop.exit(),
            _ => {}
        }
    }

    /// The application is ending: its window was closed, or Quit was chosen
    /// from the menu, which on macOS ends the process without returning from
    /// the event loop. Chromium is shut down here so that both ways do it.
    fn exiting(&mut self, _event_loop: &ActiveEventLoop) {
        self.browser = None;
        // Without this Chromium can lose the cookies of the last few seconds,
        // and a private session leaves its temporary files behind.
        polybrowser::shutdown();
    }
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let options = Options::from_arguments();
    let profile = match &options.profile {
        // Chromium keeps the profiles of a process under one directory, so
        // an application takes all of them from one `ProfileManager`.
        Some(name) => Some(ProfileManager::open(APP_ID)?.get_or_create(name)?),
        None => None,
    };

    let event_loop = EventLoop::new()?;
    let mut app = App {
        options,
        profile,
        failure: None,
        browser: None,
        window: None,
    };
    event_loop.run_app(&mut app)?;
    match app.failure.take() {
        Some(error) => Err(error.into()),
        None => Ok(()),
    }
}

fn main() -> ExitCode {
    // First thing in `main`, before arguments are read or windows created.
    polybrowser::init();

    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::FAILURE
        }
    }
}
