//! The web view that comes with the operating system, as a `polybrowser`
//! engine: `WKWebView` on macOS, WebKitGTK on Linux and WebView2 on Windows.
//!
//! The heavy lifting is done by [`wry`]; this crate maps it onto the
//! `polybrowser` backend traits and adds what a browser needs on top:
//! per-profile site data and address tracking for single-page applications.
//!
//! Applications normally use this through the `polybrowser` crate.

use std::{
    cell::RefCell,
    rc::{Rc, Weak},
};

use polybrowser_core::{
    backend::{EngineBackend, EventSink, NavigationFilter, ViewBackend, ViewConfig},
    BrowserEvent, Engine, Error, NewWindowPolicy, Rect, Result,
};
use raw_window_handle::WindowHandle;
use wry::{
    dpi::{LogicalPosition, LogicalSize},
    NewWindowResponse, PageLoadEvent, WebView, WebViewBuilder,
};

/// The engine behind the system web view on the platform being compiled for.
pub const ENGINE: Engine = if cfg!(target_os = "windows") {
    Engine::WebView2
} else {
    Engine::WebKit
};

/// Message the injected script posts when a page changes its address without
/// loading a document. Nothing in it is trusted; it only prompts a look at
/// the view's real URL.
const ADDRESS_CHANGED: &str = "polybrowser:address-changed";

/// `history.pushState` and friends do not produce load events, so a page has
/// to say when it used them.
const WATCH_ADDRESS: &str = r#"(function () {
  var notify = function () {
    try { window.ipc.postMessage("polybrowser:address-changed"); } catch (error) {}
  };
  ["pushState", "replaceState"].forEach(function (name) {
    var original = history[name];
    history[name] = function () {
      var result = original.apply(this, arguments);
      notify();
      return result;
    };
  });
  window.addEventListener("popstate", notify);
  window.addEventListener("hashchange", notify);
})();"#;

/// Creates views backed by the system web view.
#[derive(Debug, Default, Clone, Copy)]
pub struct WryBackend;

impl WryBackend {
    pub fn new() -> Self {
        Self
    }
}

impl EngineBackend for WryBackend {
    fn engine(&self) -> Engine {
        ENGINE
    }

    fn create_view(
        &self,
        parent: WindowHandle<'_>,
        config: ViewConfig,
        events: EventSink,
    ) -> Result<Box<dyn ViewBackend>> {
        if let Some(engine_root) = &config.engine_root {
            platform::purge_deleted_profiles(engine_root);
        }

        let state = Rc::new(State {
            events,
            webview: RefCell::new(Weak::new()),
            address: RefCell::new(String::new()),
            new_window_policy: config.new_window_policy,
        });

        let context = platform::context_for(&config.storage);
        let mut context_guard = context.as_ref().map(|context| context.borrow_mut());
        let mut builder = match context_guard.as_deref_mut() {
            Some(context) => WebViewBuilder::new_with_web_context(context),
            None => WebViewBuilder::new(),
        };
        builder = platform::apply_storage(builder, &config.storage);

        builder = builder
            .with_bounds(to_wry_rect(config.bounds))
            .with_visible(config.visible)
            .with_devtools(config.devtools)
            .with_initialization_script_for_main_only(WATCH_ADDRESS, true)
            .with_ipc_handler({
                let state = state.clone();
                move |request| {
                    if request.body() == ADDRESS_CHANGED {
                        state.sync_address();
                    }
                }
            })
            .with_on_page_load_handler({
                let state = state.clone();
                move |event, url| match event {
                    // A new document first, then its address: the order
                    // `BrowserEvent` promises.
                    PageLoadEvent::Started => {
                        state.emit(BrowserEvent::LoadStarted { url: url.clone() });
                        state.set_address(&url);
                    }
                    PageLoadEvent::Finished => {
                        state.set_address(&url);
                        state.emit(BrowserEvent::LoadFinished { url });
                    }
                }
            })
            .with_document_title_changed_handler({
                let state = state.clone();
                move |title| {
                    // A page that changes address and title together must be
                    // seen to change address first, or the title would be
                    // credited to the address it just left. The script's
                    // message may still be on its way, so look for ourselves.
                    // An empty title is the engine resetting it for a document
                    // that is about to be announced; nothing to look at yet.
                    if !title.is_empty() {
                        state.sync_address();
                    }
                    state.emit(BrowserEvent::TitleChanged { title });
                }
            })
            .with_new_window_req_handler({
                let state = state.clone();
                move |url, _features| {
                    state.new_window_requested(url);
                    NewWindowResponse::Deny
                }
            });
        if let Some(filter) = config.navigation_filter {
            builder = builder.with_navigation_handler(navigation_handler(filter));
        }
        if let Some(user_agent) = config.user_agent {
            builder = builder.with_user_agent(user_agent);
        }
        builder = match (config.html, config.url) {
            (Some(html), _) => builder.with_html(html),
            (None, Some(url)) => builder.with_url(url),
            (None, None) => builder,
        };

        let webview = Rc::new(builder.build_as_child(&parent).map_err(error)?);
        drop(context_guard);
        *state.webview.borrow_mut() = Rc::downgrade(&webview);

        Ok(Box::new(WryView {
            webview,
            _context: context,
        }))
    }
}

fn navigation_handler(filter: NavigationFilter) -> impl Fn(String) -> bool {
    move |url| filter(&url)
}

/// What the view's event handlers share.
struct State {
    events: EventSink,
    /// Set once the view exists; the handlers are created before it.
    webview: RefCell<Weak<WebView>>,
    /// The last address reported, to report each change once.
    address: RefCell<String>,
    new_window_policy: NewWindowPolicy,
}

impl State {
    fn emit(&self, event: BrowserEvent) {
        (self.events)(event);
    }

    fn set_address(&self, url: &str) {
        if url.is_empty() || *self.address.borrow() == url {
            return;
        }
        *self.address.borrow_mut() = url.to_owned();
        self.emit(BrowserEvent::UrlChanged {
            url: url.to_owned(),
        });
    }

    fn sync_address(&self) {
        let webview = self.webview.borrow().upgrade();
        if let Some(url) = webview.and_then(|webview| webview.url().ok()) {
            self.set_address(&url);
        }
    }

    fn new_window_requested(&self, url: String) {
        self.emit(BrowserEvent::NewWindowRequested { url: url.clone() });
        if self.new_window_policy == NewWindowPolicy::NavigateCurrent {
            let webview = self.webview.borrow().upgrade();
            if let Some(webview) = webview {
                let _ = webview.load_url(&url);
            }
        }
    }
}

struct WryView {
    webview: Rc<WebView>,
    /// Declared after the view so that it is dropped after it: wry wants a
    /// context to outlive the views created from it.
    _context: Option<platform::SharedContext>,
}

impl ViewBackend for WryView {
    fn load_url(&self, url: &str) -> Result<()> {
        self.webview.load_url(url).map_err(error)
    }

    fn load_html(&self, html: &str) -> Result<()> {
        self.webview.load_html(html).map_err(error)
    }

    fn url(&self) -> Result<String> {
        self.webview.url().map_err(error)
    }

    fn reload(&self) -> Result<()> {
        self.webview.reload().map_err(error)
    }

    fn stop(&self) -> Result<()> {
        self.webview.evaluate_script("window.stop()").map_err(error)
    }

    fn go_back(&self) -> Result<()> {
        self.webview.go_back().map_err(error)
    }

    fn go_forward(&self) -> Result<()> {
        self.webview.go_forward().map_err(error)
    }

    fn can_go_back(&self) -> Result<bool> {
        self.webview.can_go_back().map_err(error)
    }

    fn can_go_forward(&self) -> Result<bool> {
        self.webview.can_go_forward().map_err(error)
    }

    fn evaluate_script(&self, script: &str) -> Result<()> {
        self.webview.evaluate_script(script).map_err(error)
    }

    fn set_bounds(&self, bounds: Rect) -> Result<()> {
        self.webview.set_bounds(to_wry_rect(bounds)).map_err(error)
    }

    fn set_visible(&self, visible: bool) -> Result<()> {
        self.webview.set_visible(visible).map_err(error)
    }

    fn focus(&self) -> Result<()> {
        self.webview.focus().map_err(error)
    }

    fn set_zoom(&self, factor: f64) -> Result<()> {
        self.webview.zoom(factor).map_err(error)
    }

    fn open_devtools(&self) -> Result<()> {
        #[cfg(any(debug_assertions, feature = "devtools"))]
        {
            self.webview.open_devtools();
            Ok(())
        }
        #[cfg(not(any(debug_assertions, feature = "devtools")))]
        Err(devtools_disabled())
    }

    fn close_devtools(&self) -> Result<()> {
        #[cfg(any(debug_assertions, feature = "devtools"))]
        {
            self.webview.close_devtools();
            Ok(())
        }
        #[cfg(not(any(debug_assertions, feature = "devtools")))]
        Err(devtools_disabled())
    }

    fn is_devtools_open(&self) -> bool {
        #[cfg(any(debug_assertions, feature = "devtools"))]
        {
            self.webview.is_devtools_open()
        }
        #[cfg(not(any(debug_assertions, feature = "devtools")))]
        false
    }

    fn clear_browsing_data(&self) -> Result<()> {
        self.webview.clear_all_browsing_data().map_err(error)
    }
}

#[cfg(not(any(debug_assertions, feature = "devtools")))]
fn devtools_disabled() -> Error {
    Error::backend(
        ENGINE,
        "the web inspector is not compiled into release builds; enable the `devtools` feature",
    )
}

fn to_wry_rect(rect: Rect) -> wry::Rect {
    wry::Rect {
        position: LogicalPosition::new(rect.x, rect.y).into(),
        size: LogicalSize::new(rect.width.max(0.0), rect.height.max(0.0)).into(),
    }
}

fn error(error: wry::Error) -> Error {
    match error {
        wry::Error::NotMainThread => Error::NotMainThread,
        wry::Error::UnsupportedWindowHandle => Error::UnsupportedWindowHandle(ENGINE),
        wry::Error::WindowHandleError(error) => Error::WindowHandle(error),
        other => Error::backend(ENGINE, other),
    }
}

/// `WKWebView` takes no data directory. Each profile gets a data store of its
/// own, keyed by the profile id and kept by WebKit under `~/Library/WebKit`.
#[cfg(target_os = "macos")]
mod platform {
    use std::path::Path;

    use polybrowser_core::backend::{self, ProfileStorage};
    use wry::{WebView, WebViewBuilder, WebViewBuilderExtDarwin, WebViewExtDarwin};

    /// Never constructed here: there is no context to share on macOS.
    pub(super) type SharedContext = std::rc::Rc<std::cell::RefCell<wry::WebContext>>;

    pub(super) fn context_for(_storage: &ProfileStorage) -> Option<SharedContext> {
        None
    }

    pub(super) fn apply_storage<'a>(
        builder: WebViewBuilder<'a>,
        storage: &ProfileStorage,
    ) -> WebViewBuilder<'a> {
        match storage {
            ProfileStorage::Ephemeral => builder.with_incognito(true),
            // Needs macOS 14. Older systems fall back to the one data store
            // every profile then shares.
            ProfileStorage::Persistent { id, .. } => {
                builder.with_data_store_identifier(*id.as_bytes())
            }
        }
    }

    /// Erases the data stores of profiles deleted since WebKit last ran.
    pub(super) fn purge_deleted_profiles(engine_root: &Path) {
        for id in backend::pending_purges(engine_root) {
            let engine_root = engine_root.to_owned();
            WebView::remove_data_store(id.as_bytes(), move |result| {
                // A store still open in some view stays queued for next time.
                if result.is_ok() {
                    let _ = backend::purge_finished(&engine_root, id);
                }
            });
        }
    }
}

/// WebKitGTK and WebView2 keep site data in a directory of our choosing.
/// Views of one profile share one context, and with it one cookie jar.
#[cfg(not(target_os = "macos"))]
mod platform {
    use std::{cell::RefCell, collections::HashMap, path::Path, rc::Rc};

    use polybrowser_core::{backend::ProfileStorage, ProfileId};
    use wry::{WebContext, WebViewBuilder};

    pub(super) type SharedContext = Rc<RefCell<WebContext>>;

    thread_local! {
        static CONTEXTS: RefCell<HashMap<ProfileId, SharedContext>> = RefCell::new(HashMap::new());
    }

    pub(super) fn context_for(storage: &ProfileStorage) -> Option<SharedContext> {
        let ProfileStorage::Persistent { id, data_dir } = storage else {
            return None;
        };
        Some(CONTEXTS.with(|contexts| {
            contexts
                .borrow_mut()
                .entry(*id)
                .or_insert_with(|| Rc::new(RefCell::new(WebContext::new(Some(data_dir.clone())))))
                .clone()
        }))
    }

    pub(super) fn apply_storage<'a>(
        builder: WebViewBuilder<'a>,
        storage: &ProfileStorage,
    ) -> WebViewBuilder<'a> {
        match storage {
            ProfileStorage::Ephemeral => builder.with_incognito(true),
            ProfileStorage::Persistent { .. } => builder,
        }
    }

    pub(super) fn purge_deleted_profiles(_engine_root: &Path) {}
}
