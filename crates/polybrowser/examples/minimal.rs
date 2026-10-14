//! The smallest useful embedding: a winit window with a browser filling it.
//!
//! ```text
//! cargo run -p polybrowser --example minimal -- [engine] [url]
//! ```
//!
//! `engine` is `webkit`, `webview2` or `chromium`; it defaults to the first
//! one compiled in. The page is browsed privately, so nothing is left on disk.

use polybrowser::{Browser, BrowserEvent, Engine, Rect};
use winit::{
    application::ApplicationHandler,
    dpi::LogicalSize,
    event::WindowEvent,
    event_loop::{ActiveEventLoop, ControlFlow, EventLoop},
    window::{Window, WindowId},
};

struct App {
    engine: Engine,
    url: String,
    // The browser is declared first so that it is dropped before its window.
    browser: Option<Browser>,
    window: Option<Window>,
}

impl App {
    fn view_bounds(window: &Window) -> Rect {
        let size = window.inner_size().to_logical::<f64>(window.scale_factor());
        Rect::new(0.0, 0.0, size.width, size.height)
    }
}

impl ApplicationHandler for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        let attributes = Window::default_attributes()
            .with_title(format!("polybrowser ({})", self.engine))
            .with_inner_size(LogicalSize::new(1100.0, 760.0));
        let window = event_loop.create_window(attributes).expect("create window");

        let browser = Browser::builder()
            .engine(self.engine)
            .url(&self.url)
            .on_event(|event| match event {
                BrowserEvent::UrlChanged { url } => println!("address  {url}"),
                BrowserEvent::TitleChanged { title } => println!("title    {title}"),
                BrowserEvent::LoadFailed { url, error } => println!("failed   {url}: {error}"),
                _ => {}
            })
            .build(&window, Self::view_bounds(&window))
            .expect("create browser");

        self.browser = Some(browser);
        self.window = Some(window);
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::Resized(_) => {
                if let (Some(window), Some(browser)) = (&self.window, &self.browser) {
                    let _ = browser.set_bounds(Self::view_bounds(window));
                }
            }
            WindowEvent::CloseRequested => {
                self.browser = None;
                event_loop.exit();
            }
            _ => {}
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        // Chromium on Linux and Windows is driven from here; elsewhere this
        // costs nothing and the loop could simply wait.
        polybrowser::pump();
        let needs_pumping = self.engine == Engine::Chromium && !cfg!(target_os = "macos");
        event_loop.set_control_flow(if needs_pumping {
            ControlFlow::wait_duration(std::time::Duration::from_millis(10))
        } else {
            ControlFlow::Wait
        });
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Must come first: Chromium's helper processes re-enter `main`.
    polybrowser::init();

    let mut arguments = std::env::args().skip(1);
    let engine = match arguments.next() {
        Some(name) => name.parse()?,
        None => polybrowser::default_engine().ok_or("no engine compiled in")?,
    };
    let url = arguments
        .next()
        .unwrap_or_else(|| "https://www.rust-lang.org".to_owned());

    let event_loop = EventLoop::new()?;
    let mut app = App {
        engine,
        url,
        browser: None,
        window: None,
    };
    event_loop.run_app(&mut app)?;

    drop(app);
    polybrowser::shutdown();
    Ok(())
}
