//! A Chromium browser embedded as a child view, and its event handlers.

use std::{
    cell::{Cell, RefCell},
    collections::HashMap,
    os::raw::c_int,
    rc::Rc,
};

use cef::*;
use objc2::rc::autoreleasepool;
use polybrowser_core::{
    backend::{EventSink, NavigationFilter, ViewBackend, ViewConfig},
    BrowserEvent, Engine, Error, NewWindowPolicy, Result,
};
use raw_window_handle::{RawWindowHandle, WindowHandle};

use crate::{
    appkit,
    runtime::{self, error},
};

/// What the handlers of one view share with the view itself.
struct ViewState {
    /// Cleared when the application drops the view, so that a browser which
    /// is still closing stays quiet.
    events: RefCell<Option<EventSink>>,
    /// The last address reported, to report each change once.
    address: RefCell<String>,
    /// The last title reported for the current document; Chromium repeats it.
    title: RefCell<Option<String>>,
    navigation_filter: Option<NavigationFilter>,
    new_window_policy: NewWindowPolicy,
    browser: RefCell<Option<Browser>>,
}

thread_local! {
    /// Every browser that exists, including those still closing.
    ///
    /// Chromium calls some handler methods on its own threads. Handlers hold
    /// only the key into this map, and the map belongs to the main thread, so
    /// a call from elsewhere finds nothing and touches no main-thread state.
    static VIEWS: RefCell<HashMap<u64, Rc<ViewState>>> = RefCell::new(HashMap::new());
    static NEXT_ID: Cell<u64> = const { Cell::new(1) };
}

fn view(id: u64) -> Option<Rc<ViewState>> {
    VIEWS
        .try_with(|views| views.borrow().get(&id).cloned())
        .ok()
        .flatten()
}

pub(crate) fn open_count() -> usize {
    VIEWS.with(|views| views.borrow().len())
}

/// Asks every browser to close; used at shutdown.
pub(crate) fn close_all() {
    let views: Vec<Rc<ViewState>> = VIEWS.with(|views| views.borrow().values().cloned().collect());
    for view in views {
        view.events.borrow_mut().take();
        view.close();
    }
}

impl ViewState {
    fn emit(&self, event: BrowserEvent) {
        let events = self.events.borrow().clone();
        if let Some(events) = events {
            events(event);
        }
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

    fn browser(&self) -> Option<Browser> {
        self.browser.borrow().clone()
    }

    fn set_title(&self, title: String) {
        if self.title.borrow().as_deref() == Some(title.as_str()) {
            return;
        }
        *self.title.borrow_mut() = Some(title.clone());
        self.emit(BrowserEvent::TitleChanged { title });
    }

    /// Destroys the browser by taking its view out of the window.
    ///
    /// Asking Chromium to close a browser makes it ask the surrounding window
    /// to close, which is right for a browser that owns its window and wrong
    /// for one embedded in somebody else's. A child browser goes away when
    /// its view does.
    fn close(&self) {
        let Some(host) = self.browser().and_then(|browser| browser.host()) else {
            return;
        };
        // In an autorelease pool of its own: AppKit sets the view aside to
        // release it later, and if "later" is a pool the application drains
        // after `shutdown`, as winit does when its event loop ends, the view
        // outlives Chromium and brings the process down.
        autoreleasepool(|_| {
            // SAFETY: Chromium hands out its own live view, or null once closed.
            match unsafe { appkit::view_from_pointer(host.window_handle().cast()) } {
                Some(view) => appkit::remove_from_parent(&view),
                None => host.close_browser(1),
            }
        });
    }
}

fn is_main(frame: &Option<&mut Frame>) -> bool {
    frame.as_ref().is_some_and(|frame| frame.is_main() != 0)
}

fn text(string: Option<&CefString>) -> String {
    string.map(CefString::to_string).unwrap_or_default()
}

fn frame_url(frame: Option<&mut Frame>) -> String {
    frame
        .map(|frame| CefString::from(&frame.url()).to_string())
        .unwrap_or_default()
}

wrap_client! {
    struct ViewClient {
        id: u64,
    }

    impl Client {
        fn display_handler(&self) -> Option<DisplayHandler> {
            Some(ViewDisplayHandler::new(self.id))
        }

        fn load_handler(&self) -> Option<LoadHandler> {
            Some(ViewLoadHandler::new(self.id))
        }

        fn life_span_handler(&self) -> Option<LifeSpanHandler> {
            Some(ViewLifeSpanHandler::new(self.id))
        }

        fn request_handler(&self) -> Option<RequestHandler> {
            Some(ViewRequestHandler::new(self.id))
        }
    }
}

wrap_display_handler! {
    struct ViewDisplayHandler {
        id: u64,
    }

    impl DisplayHandler {
        fn on_address_change(
            &self,
            _browser: Option<&mut Browser>,
            frame: Option<&mut Frame>,
            url: Option<&CefString>,
        ) {
            if is_main(&frame) {
                if let Some(view) = view(self.id) {
                    view.set_address(&text(url));
                }
            }
        }

        fn on_title_change(&self, _browser: Option<&mut Browser>, title: Option<&CefString>) {
            if let Some(view) = view(self.id) {
                view.set_title(text(title));
            }
        }
    }
}

wrap_load_handler! {
    struct ViewLoadHandler {
        id: u64,
    }

    impl LoadHandler {
        fn on_load_start(
            &self,
            _browser: Option<&mut Browser>,
            frame: Option<&mut Frame>,
            _transition_type: TransitionType,
        ) {
            if !is_main(&frame) {
                return;
            }
            let url = frame_url(frame);
            if let Some(view) = view(self.id) {
                // A new document first, then its address: the order
                // `BrowserEvent` promises.
                view.title.borrow_mut().take();
                view.emit(BrowserEvent::LoadStarted { url: url.clone() });
                view.set_address(&url);
            }
        }

        fn on_load_end(
            &self,
            _browser: Option<&mut Browser>,
            frame: Option<&mut Frame>,
            _http_status_code: c_int,
        ) {
            if !is_main(&frame) {
                return;
            }
            let url = frame_url(frame);
            if let Some(view) = view(self.id) {
                view.emit(BrowserEvent::LoadFinished { url });
            }
        }

        fn on_load_error(
            &self,
            _browser: Option<&mut Browser>,
            frame: Option<&mut Frame>,
            error_code: Errorcode,
            error_text: Option<&CefString>,
            failed_url: Option<&CefString>,
        ) {
            // An aborted load is a navigation that was replaced or cancelled,
            // not a failure.
            let aborted = sys::cef_errorcode_t::from(error_code) == sys::cef_errorcode_t::ERR_ABORTED;
            if aborted || !is_main(&frame) {
                return;
            }
            if let Some(view) = view(self.id) {
                view.emit(BrowserEvent::LoadFailed {
                    url: text(failed_url),
                    error: text(error_text),
                });
            }
        }
    }
}

wrap_life_span_handler! {
    struct ViewLifeSpanHandler {
        id: u64,
    }

    impl LifeSpanHandler {
        fn on_before_popup(
            &self,
            _browser: Option<&mut Browser>,
            _frame: Option<&mut Frame>,
            _popup_id: c_int,
            target_url: Option<&CefString>,
            _target_frame_name: Option<&CefString>,
            _target_disposition: WindowOpenDisposition,
            _user_gesture: c_int,
            _popup_features: Option<&PopupFeatures>,
            _window_info: Option<&mut WindowInfo>,
            _client: Option<&mut Option<Client>>,
            _settings: Option<&mut BrowserSettings>,
            _extra_info: Option<&mut Option<DictionaryValue>>,
            _no_javascript_access: Option<&mut c_int>,
        ) -> c_int {
            let url = text(target_url);
            if let Some(view) = view(self.id) {
                view.emit(BrowserEvent::NewWindowRequested { url: url.clone() });
                if view.new_window_policy == NewWindowPolicy::NavigateCurrent {
                    if let Some(frame) = view.browser().and_then(|browser| browser.main_frame()) {
                        frame.load_url(Some(&CefString::from(url.as_str())));
                    }
                }
            }
            // Never let Chromium open a window of its own.
            1
        }

        fn do_close(&self, _browser: Option<&mut Browser>) -> c_int {
            // Non-zero: the close is ours to carry out. Otherwise Chromium
            // would close the window the view sits in, and a page could do
            // that to the application with `window.close()`.
            1
        }

        fn on_before_close(&self, _browser: Option<&mut Browser>) {
            let closed = VIEWS.try_with(|views| views.borrow_mut().remove(&self.id));
            if let Ok(Some(view)) = closed {
                view.browser.borrow_mut().take();
            }
        }
    }
}

wrap_request_handler! {
    struct ViewRequestHandler {
        id: u64,
    }

    impl RequestHandler {
        fn on_before_browse(
            &self,
            _browser: Option<&mut Browser>,
            frame: Option<&mut Frame>,
            request: Option<&mut Request>,
            _user_gesture: c_int,
            _is_redirect: c_int,
        ) -> c_int {
            if !is_main(&frame) {
                return 0;
            }
            let Some(filter) = view(self.id).and_then(|view| view.navigation_filter.clone()) else {
                return 0;
            };
            let url = request
                .map(|request| CefString::from(&request.url()).to_string())
                .unwrap_or_default();
            // Returning non-zero cancels the navigation.
            (!filter(&url)).into()
        }
    }
}

pub(crate) fn create(
    parent: WindowHandle<'_>,
    config: ViewConfig,
    events: EventSink,
) -> Result<Box<dyn ViewBackend>> {
    let RawWindowHandle::AppKit(handle) = parent.as_raw() else {
        return Err(Error::UnsupportedWindowHandle(Engine::Chromium));
    };
    runtime::ensure_started(config.engine_root.as_deref())?;
    let mut context = runtime::request_context(&config.storage)?;

    // SAFETY: the handle is the caller's promise of a live NSView, and
    // starting Chromium has established that this is the main thread.
    let parent_view = unsafe { appkit::view_from_pointer(handle.ns_view.as_ptr()) }
        .ok_or(Error::UnsupportedWindowHandle(Engine::Chromium))?;
    let frame = appkit::frame_in(&parent_view, config.bounds);
    let window_info = WindowInfo {
        // "Alloy" is the bare web view; the default style brings Chrome's own
        // toolbar and tab strip along.
        runtime_style: RuntimeStyle::ALLOY,
        ..Default::default()
    }
    .set_as_child(
        handle.ns_view.as_ptr(),
        &cef::Rect {
            x: frame.origin.x.round() as c_int,
            y: frame.origin.y.round() as c_int,
            width: frame.size.width.round() as c_int,
            height: frame.size.height.round() as c_int,
        },
    );
    let window_info = WindowInfo {
        hidden: (!config.visible).into(),
        ..window_info
    };

    let url = match (&config.html, &config.url) {
        (Some(html), _) => data_url(html),
        (None, Some(url)) => url.clone(),
        (None, None) => "about:blank".to_owned(),
    };

    let id = NEXT_ID.with(|next| next.replace(next.get() + 1));
    let state = Rc::new(ViewState {
        events: RefCell::new(Some(events)),
        address: RefCell::new(String::new()),
        title: RefCell::new(None),
        navigation_filter: config.navigation_filter,
        new_window_policy: config.new_window_policy,
        browser: RefCell::new(None),
    });
    VIEWS.with(|views| views.borrow_mut().insert(id, state.clone()));

    let mut client = ViewClient::new(id);
    let browser = browser_host_create_browser_sync(
        Some(&window_info),
        Some(&mut client),
        Some(&CefString::from(url.as_str())),
        Some(&BrowserSettings::default()),
        None,
        Some(&mut context),
    );
    let Some(browser) = browser else {
        VIEWS.with(|views| views.borrow_mut().remove(&id));
        return Err(error("Chromium did not create the view"));
    };
    *state.browser.borrow_mut() = Some(browser.clone());

    Ok(Box::new(CefView { browser, state }))
}

/// A document given as a string, as a URL Chromium can navigate to.
fn data_url(html: &str) -> String {
    let encoded = CefString::from(&base64_encode(Some(html.as_bytes())));
    let encoded = CefString::from(&uriencode(Some(&encoded), 0)).to_string();
    format!("data:text/html;charset=utf-8;base64,{encoded}")
}

struct CefView {
    browser: Browser,
    state: Rc<ViewState>,
}

impl CefView {
    fn host(&self) -> Result<BrowserHost> {
        self.browser
            .host()
            .ok_or_else(|| error("the view is closed"))
    }

    fn frame(&self) -> Result<Frame> {
        self.browser
            .main_frame()
            .ok_or_else(|| error("the view is closed"))
    }

    /// The `NSView` Chromium created inside the parent.
    fn native_view(&self) -> Result<objc2::rc::Retained<objc2_app_kit::NSView>> {
        let pointer = self.host()?.window_handle();
        // SAFETY: Chromium hands out its own live view, or null once closed;
        // views are only used on the main thread.
        unsafe { appkit::view_from_pointer(pointer.cast()) }
            .ok_or_else(|| error("the view is closed"))
    }
}

impl ViewBackend for CefView {
    fn load_url(&self, url: &str) -> Result<()> {
        self.frame()?.load_url(Some(&CefString::from(url)));
        Ok(())
    }

    fn load_html(&self, html: &str) -> Result<()> {
        self.load_url(&data_url(html))
    }

    fn url(&self) -> Result<String> {
        Ok(CefString::from(&self.frame()?.url()).to_string())
    }

    fn reload(&self) -> Result<()> {
        self.browser.reload();
        Ok(())
    }

    fn stop(&self) -> Result<()> {
        self.browser.stop_load();
        Ok(())
    }

    fn go_back(&self) -> Result<()> {
        self.browser.go_back();
        Ok(())
    }

    fn go_forward(&self) -> Result<()> {
        self.browser.go_forward();
        Ok(())
    }

    fn can_go_back(&self) -> Result<bool> {
        Ok(self.browser.can_go_back() != 0)
    }

    fn can_go_forward(&self) -> Result<bool> {
        Ok(self.browser.can_go_forward() != 0)
    }

    fn evaluate_script(&self, script: &str) -> Result<()> {
        self.frame()?
            .execute_java_script(Some(&CefString::from(script)), None, 0);
        Ok(())
    }

    fn set_bounds(&self, bounds: polybrowser_core::Rect) -> Result<()> {
        let view = self.native_view()?;
        appkit::set_frame(&view, bounds);
        Ok(())
    }

    fn set_visible(&self, visible: bool) -> Result<()> {
        let view = self.native_view()?;
        appkit::set_hidden(&view, !visible);
        // Lets Chromium stop painting a view nobody can see.
        self.host()?.was_hidden((!visible).into());
        Ok(())
    }

    fn focus(&self) -> Result<()> {
        let view = self.native_view()?;
        appkit::make_first_responder(&view);
        self.host()?.set_focus(1);
        Ok(())
    }

    fn set_zoom(&self, factor: f64) -> Result<()> {
        // Chromium's zoom level is the exponent of 1.2 that gives the factor.
        self.host()?
            .set_zoom_level(factor.max(0.01).ln() / 1.2_f64.ln());
        Ok(())
    }

    fn open_devtools(&self) -> Result<()> {
        self.host()?.show_dev_tools(
            Some(&WindowInfo::default()),
            None,
            Some(&BrowserSettings::default()),
            None,
        );
        Ok(())
    }

    fn close_devtools(&self) -> Result<()> {
        self.host()?.close_dev_tools();
        Ok(())
    }

    fn is_devtools_open(&self) -> bool {
        self.host().is_ok_and(|host| host.has_dev_tools() != 0)
    }

    fn clear_browsing_data(&self) -> Result<()> {
        let context = self
            .host()?
            .request_context()
            .ok_or_else(|| error("the view has no profile"))?;
        if let Some(cookies) = context.cookie_manager(None) {
            cookies.delete_cookies(None, None, None);
        }
        context.clear_http_cache(None);
        Ok(())
    }
}

impl Drop for CefView {
    fn drop(&mut self) {
        // Chromium may take a few turns of the message loop to let go of the
        // browser; `on_before_close` forgets it then. It is silent meanwhile.
        self.state.events.borrow_mut().take();
        self.state.close();
    }
}
