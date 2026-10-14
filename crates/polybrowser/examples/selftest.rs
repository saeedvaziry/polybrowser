//! Checks that an engine really works on this machine.
//!
//! ```text
//! cargo run -p polybrowser --example selftest -- [engine]
//! ```
//!
//! Opens a window for a few seconds and browses pages from a server it starts
//! on localhost, so it needs no network. It verifies that events arrive, that
//! history is recorded (including an address changed with `pushState`), that
//! a profile keeps its cookies between views and that another profile does
//! not see them, and that the page inspector opens and closes. The exit status
//! is zero if everything held.

use std::{
    io::{BufRead, BufReader, Write},
    net::{TcpListener, TcpStream},
    process::ExitCode,
    thread,
    time::{Duration, Instant},
};

use polybrowser::{
    Browser, BrowserEvent, Engine, HistoryFormat, HistoryQuery, Profile, ProfileManager, Rect,
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
}

struct App {
    engine: Engine,
    origin: String,
    proxy: EventLoopProxy<BrowserEvent>,
    main_profile: Profile,
    other_profile: Profile,
    step: Step,
    started: Instant,
    events: Vec<BrowserEvent>,
    failures: Vec<String>,
    completed: bool,
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
                self.completed = true;
                self.browser = None;
                event_loop.exit();
            }
            _ => {}
        }
        if !self.failures.is_empty() && self.browser.is_none() {
            // Creating a view failed; nothing more will happen.
            event_loop.exit();
        } else if self.started.elapsed() > TIMEOUT {
            self.failures
                .push(format!("timed out waiting in step {:?}", self.step));
            self.browser = None;
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
    let mut app = App {
        engine,
        origin,
        proxy: event_loop.create_proxy(),
        main_profile: main_profile.clone(),
        other_profile: other_profile.clone(),
        step: Step::FirstPage,
        started: Instant::now(),
        events: Vec::new(),
        failures: Vec::new(),
        completed: false,
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
