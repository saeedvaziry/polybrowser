use std::{
    cell::{Cell, RefCell},
    fmt, fs,
    rc::Rc,
};

use polybrowser_core::{
    backend::{
        EngineBackend, EventSink, NavigationFilter, ProfileStorage, ViewBackend, ViewConfig,
    },
    is_recordable_url, BrowserEvent, Engine, Error, History, NewWindowPolicy, Profile, Rect,
    Result,
};
use raw_window_handle::HasWindowHandle;

use crate::engines;

/// A web browser view embedded in a window of the application.
///
/// A `Browser` renders pages with one [`Engine`] and stores its site data in
/// one [`Profile`]. An application that wants tabs creates one `Browser` per
/// tab and shows one at a time with [`set_visible`](Browser::set_visible).
///
/// Views belong to the main thread: create, use and drop a `Browser` there.
/// Dropping it removes the view from the window.
///
/// ```no_run
/// # fn demo(window: &impl raw_window_handle::HasWindowHandle) -> polybrowser::Result<()> {
/// use polybrowser::{Browser, ProfileManager, Rect};
///
/// let profile = ProfileManager::open("com.example.app")?.get_or_create("Default")?;
/// let browser = Browser::builder()
///     .profile(&profile)
///     .url("https://www.rust-lang.org")
///     .on_event(|event| println!("{event:?}"))
///     .build(window, Rect::new(0.0, 0.0, 1024.0, 768.0))?;
/// browser.load_url("https://docs.rs")?;
/// # Ok(())
/// # }
/// ```
pub struct Browser {
    view: Box<dyn ViewBackend>,
    engine: Engine,
    profile: Profile,
    devtools: bool,
    state: Rc<State>,
}

/// What the view last told us, kept so it can be read back synchronously.
struct State {
    url: RefCell<String>,
    title: RefCell<String>,
    loading: Cell<bool>,
    history: Option<History>,
    on_event: Option<Box<dyn Fn(BrowserEvent)>>,
}

impl State {
    fn handle(&self, event: BrowserEvent) {
        match &event {
            BrowserEvent::LoadStarted { .. } => {
                self.loading.set(true);
                // A new document: the previous one's title is not its title.
                self.title.borrow_mut().clear();
            }
            BrowserEvent::UrlChanged { url } => {
                let previous = self.url.replace(url.clone());
                if previous != *url {
                    // Still set here only if the document stayed the same and
                    // just changed its address, in which case it still applies.
                    let title = self.title.borrow().clone();
                    let title = Some(title.as_str()).filter(|title| !title.is_empty());
                    self.record(|history| history.record_visit(url, title).map(drop));
                }
            }
            BrowserEvent::TitleChanged { title } => {
                self.title.replace(title.clone());
                let url = self.url.borrow().clone();
                if !title.is_empty() && is_recordable_url(&url) {
                    self.record(|history| history.set_title(&url, title));
                }
            }
            BrowserEvent::LoadFinished { .. } | BrowserEvent::LoadFailed { .. } => {
                self.loading.set(false);
            }
            _ => {}
        }
        if let Some(on_event) = &self.on_event {
            on_event(event);
        }
    }

    fn record(&self, write: impl FnOnce(&History) -> Result<()>) {
        if let Some(history) = &self.history {
            // Browsing goes on even when the history cannot be written; there
            // is nobody to hand the error to from inside an engine callback.
            let _ = write(history);
        }
    }
}

impl Browser {
    pub fn builder() -> BrowserBuilder {
        BrowserBuilder::default()
    }

    /// The engine rendering this view.
    pub fn engine(&self) -> Engine {
        self.engine
    }

    /// The profile this view browses with.
    pub fn profile(&self) -> &Profile {
        &self.profile
    }

    /// The address of the page being shown, as last reported by the engine.
    /// Empty until the first navigation commits.
    pub fn url(&self) -> String {
        self.state.url.borrow().clone()
    }

    /// The title of the page being shown; empty if it has none yet.
    pub fn title(&self) -> String {
        self.state.title.borrow().clone()
    }

    pub fn is_loading(&self) -> bool {
        self.state.loading.get()
    }

    pub fn load_url(&self, url: &str) -> Result<()> {
        self.view.load_url(url)
    }

    /// Shows a document given as a string. It has no address of its own and
    /// does not enter the history.
    pub fn load_html(&self, html: &str) -> Result<()> {
        self.view.load_html(html)
    }

    pub fn reload(&self) -> Result<()> {
        self.view.reload()
    }

    pub fn stop(&self) -> Result<()> {
        self.view.stop()
    }

    pub fn go_back(&self) -> Result<()> {
        self.view.go_back()
    }

    pub fn go_forward(&self) -> Result<()> {
        self.view.go_forward()
    }

    pub fn can_go_back(&self) -> Result<bool> {
        self.view.can_go_back()
    }

    pub fn can_go_forward(&self) -> Result<bool> {
        self.view.can_go_forward()
    }

    /// Runs JavaScript in the page's main frame, without waiting for a result.
    pub fn evaluate_script(&self, script: &str) -> Result<()> {
        self.view.evaluate_script(script)
    }

    /// Moves and resizes the view inside its window. Call this when the
    /// window is resized; views do not follow it on their own.
    pub fn set_bounds(&self, bounds: Rect) -> Result<()> {
        self.view.set_bounds(bounds)
    }

    pub fn set_visible(&self, visible: bool) -> Result<()> {
        self.view.set_visible(visible)
    }

    /// Gives the view keyboard focus.
    pub fn focus(&self) -> Result<()> {
        self.view.focus()
    }

    /// Scales the page; `1.0` is its natural size.
    pub fn set_zoom(&self, factor: f64) -> Result<()> {
        self.view.set_zoom(factor)
    }

    /// Opens the engine's page inspector for this view: Web Inspector in
    /// WebKit, DevTools in Chromium and WebView2.
    ///
    /// Fails unless the view was built with
    /// [`devtools(true)`](BrowserBuilder::devtools), which is the default in
    /// debug builds only.
    pub fn open_devtools(&self) -> Result<()> {
        if !self.devtools {
            return Err(Error::backend(
                self.engine,
                "the page inspector is disabled for this view; build it with `devtools(true)`",
            ));
        }
        self.view.open_devtools()
    }

    pub fn close_devtools(&self) -> Result<()> {
        self.view.close_devtools()
    }

    pub fn is_devtools_open(&self) -> bool {
        self.view.is_devtools_open()
    }

    /// Deletes the cookies, caches and site storage of this view's profile in
    /// this view's engine. History and bookmarks are kept; see
    /// [`History::clear`].
    pub fn clear_browsing_data(&self) -> Result<()> {
        self.view.clear_browsing_data()
    }
}

impl fmt::Debug for Browser {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Browser")
            .field("engine", &self.engine)
            .field("profile", &self.profile)
            .field("url", &self.url())
            .finish_non_exhaustive()
    }
}

/// Configures a [`Browser`]. Start with [`Browser::builder`].
pub struct BrowserBuilder {
    engine: Option<Engine>,
    backend: Option<Box<dyn EngineBackend>>,
    profile: Option<Profile>,
    url: Option<String>,
    html: Option<String>,
    visible: bool,
    user_agent: Option<String>,
    devtools: bool,
    record_history: bool,
    new_window_policy: NewWindowPolicy,
    navigation_filter: Option<NavigationFilter>,
    on_event: Option<Box<dyn Fn(BrowserEvent)>>,
}

impl Default for BrowserBuilder {
    fn default() -> Self {
        Self {
            engine: None,
            backend: None,
            profile: None,
            url: None,
            html: None,
            visible: true,
            user_agent: None,
            devtools: cfg!(debug_assertions),
            record_history: true,
            new_window_policy: NewWindowPolicy::default(),
            navigation_filter: None,
            on_event: None,
        }
    }
}

impl BrowserBuilder {
    /// Chooses the engine. Without this, [`default_engine`](crate::default_engine)
    /// is used. Building fails with [`Error::EngineUnavailable`] if the engine
    /// is not in [`engines`](crate::engines).
    pub fn engine(mut self, engine: Engine) -> Self {
        self.engine = Some(engine);
        self
    }

    /// Uses an engine implemented outside this crate. Overrides
    /// [`engine`](Self::engine).
    pub fn backend(mut self, backend: impl EngineBackend + 'static) -> Self {
        self.backend = Some(Box::new(backend));
        self
    }

    /// Chooses the profile. Without this, the view browses privately, with a
    /// fresh [`Profile::ephemeral`].
    pub fn profile(mut self, profile: &Profile) -> Self {
        self.profile = Some(profile.clone());
        self
    }

    /// The first page to load.
    pub fn url(mut self, url: impl Into<String>) -> Self {
        self.url = Some(url.into());
        self
    }

    /// A document to show first, instead of [`url`](Self::url).
    pub fn html(mut self, html: impl Into<String>) -> Self {
        self.html = Some(html.into());
        self
    }

    /// Whether the view starts out visible. Defaults to `true`.
    pub fn visible(mut self, visible: bool) -> Self {
        self.visible = visible;
        self
    }

    /// Overrides the `User-Agent` the view sends. Not applied by the
    /// Chromium engine yet.
    pub fn user_agent(mut self, user_agent: impl Into<String>) -> Self {
        self.user_agent = Some(user_agent.into());
        self
    }

    /// Whether the page inspector may be opened, with
    /// [`Browser::open_devtools`] or, in the system web views, from the
    /// page's context menu. Defaults to on in debug builds and off in release
    /// builds.
    ///
    /// The system web views also need the `devtools` Cargo feature to keep
    /// the inspector in a release build. On macOS that feature uses a private
    /// WebKit API, which the App Store does not accept.
    pub fn devtools(mut self, devtools: bool) -> Self {
        self.devtools = devtools;
        self
    }

    /// Whether visited pages are written to the profile's
    /// [history](Profile::history). Defaults to `true`.
    pub fn record_history(mut self, record_history: bool) -> Self {
        self.record_history = record_history;
        self
    }

    /// What to do when a page wants to open a new window. Defaults to
    /// loading it in this view.
    pub fn new_window_policy(mut self, policy: NewWindowPolicy) -> Self {
        self.new_window_policy = policy;
        self
    }

    /// Lets the application veto navigations: return `false` to cancel.
    /// Chromium asks about the main frame only; the system web views may ask
    /// about frames inside the page as well.
    pub fn on_navigation(mut self, filter: impl Fn(&str) -> bool + 'static) -> Self {
        self.navigation_filter = Some(Rc::new(filter));
        self
    }

    /// Receives the view's events, on the main thread, while the application's
    /// event loop runs. The usual thing to do is to forward them into that
    /// event loop; calling back into the [`Browser`] from here is not supported.
    pub fn on_event(mut self, handler: impl Fn(BrowserEvent) + 'static) -> Self {
        self.on_event = Some(Box::new(handler));
        self
    }

    /// Creates the view inside `window`, at `bounds` within its content area.
    ///
    /// Anything that implements [`HasWindowHandle`] will do, which covers
    /// winit, tao, eframe, iced, Slint, SDL, GLFW, FLTK and more. The window
    /// must outlive the returned [`Browser`].
    pub fn build<W: HasWindowHandle + ?Sized>(self, window: &W, bounds: Rect) -> Result<Browser> {
        let backend = match self.backend {
            Some(backend) => backend,
            None => {
                let engine = match self.engine {
                    Some(engine) => engine,
                    None => engines::default_engine().ok_or(Error::NoEngine)?,
                };
                engines::backend(engine)?
            }
        };
        let engine = backend.engine();
        let profile = self.profile.unwrap_or_else(Profile::ephemeral);

        let storage = match profile.engine_data_dir(engine) {
            Some(data_dir) => {
                // Also marks the profile as having data in this engine, which
                // deleting the profile relies on.
                fs::create_dir_all(&data_dir)?;
                ProfileStorage::Persistent {
                    id: profile.id(),
                    data_dir,
                }
            }
            None => ProfileStorage::Ephemeral,
        };

        let state = Rc::new(State {
            url: RefCell::new(String::new()),
            title: RefCell::new(String::new()),
            loading: Cell::new(false),
            history: if self.record_history {
                Some(profile.history()?)
            } else {
                None
            },
            on_event: self.on_event,
        });
        let events: EventSink = {
            let state = state.clone();
            Rc::new(move |event| state.handle(event))
        };

        let config = ViewConfig {
            url: self.url,
            html: self.html,
            bounds,
            visible: self.visible,
            user_agent: self.user_agent,
            devtools: self.devtools,
            storage,
            engine_root: profile.engine_root(engine),
            new_window_policy: self.new_window_policy,
            navigation_filter: self.navigation_filter,
        };
        let devtools = config.devtools;
        let view = backend.create_view(window.window_handle()?, config, events)?;

        Ok(Browser {
            view,
            engine,
            profile,
            devtools,
            state,
        })
    }
}
