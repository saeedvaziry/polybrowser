//! Native `WKWebView` features that wry does not expose: scripts with a
//! result, snapshots that work while hidden, find-in-page, cookie access, the
//! extra navigation events, `window.close()` from the main frame, and
//! per-profile data-store operations without a view. macOS only.
//!
//! Everything here runs on the main thread. Asynchronous WebKit APIs take a
//! completion block that calls back on the main thread; the block owns the
//! Rust callback in a cell so it fires exactly once. A result known up front
//! is still reported on a later turn of the run loop, never re-entrantly.

use std::{
    cell::{Cell, RefCell},
    ffi::c_void,
    ptr::{self, NonNull},
    rc::Rc,
    time::{SystemTime, UNIX_EPOCH},
};

use block2::{DynBlock, RcBlock};
use objc2::{
    define_class, msg_send,
    rc::Retained,
    runtime::{AnyObject, NSObject, NSObjectProtocol, ProtocolObject, Sel},
    AllocAnyThread, DefinedClass, MainThreadOnly,
};
use objc2_app_kit::{
    NSBitmapImageRep, NSCompositingOperation, NSDeviceRGBColorSpace, NSGraphicsContext, NSImage,
};
use objc2_foundation::{
    MainThreadMarker, NSArray, NSData, NSDate, NSDictionary, NSError, NSHTTPCookie,
    NSKeyValueObservingOptions, NSObjectNSKeyValueObserverRegistration, NSPoint, NSRect, NSRunLoop,
    NSRunLoopCommonModes, NSSet, NSSize, NSString, NSTimer, NSURL, NSUUID,
};
use objc2_web_kit::{
    WKContentWorld, WKFindConfiguration, WKFindResult, WKHTTPCookieStore, WKNavigation,
    WKNavigationAction, WKNavigationActionPolicy, WKNavigationDelegate, WKNavigationType,
    WKScriptMessage, WKScriptMessageHandler, WKSnapshotConfiguration, WKUserContentController,
    WKWebView, WKWebsiteDataStore, WKWebsiteDataTypeCookies,
};
use polybrowser_core::{
    backend::{Done, EventSink},
    BrowserEvent, Cookie, Error, FindOptions, Image, Profile, Result, Timestamp,
};
use wry::{WebView, WebViewExtMacOS, WryWebView};

use crate::ENGINE;

/// Proof of being on the main thread, which every WebKit call here needs.
fn mtm() -> Result<MainThreadMarker> {
    MainThreadMarker::new().ok_or(Error::NotMainThread)
}

fn backend_error(message: impl std::fmt::Display) -> Error {
    Error::backend(ENGINE, message)
}

/// `&Rc<dyn Fn>` is awkward to call; clone the cheap handle and invoke it.
fn emit(events: &EventSink, event: BrowserEvent) {
    let events = events.clone();
    events(event);
}

/// Schedules a one-shot timer on the main run loop, in the common modes so it
/// also fires while a menu is open or a window is being resized.
fn schedule(seconds: f64, block: &DynBlock<dyn Fn(NonNull<NSTimer>)>) -> Retained<NSTimer> {
    // SAFETY: the timer is scheduled on, and fires on, the main run loop, so
    // its block is only ever touched on the main thread.
    unsafe {
        let timer = NSTimer::timerWithTimeInterval_repeats_block(seconds, false, block);
        NSRunLoop::mainRunLoop().addTimer_forMode(&timer, NSRunLoopCommonModes);
        timer
    }
}

/// Reports a result known up front on a later turn of the main run loop, as
/// callbacks never run before the call that takes them returns.
fn defer<T: 'static>(done: Done<T>, result: Result<T>) {
    let slot = RefCell::new(Some((done, result)));
    let block = RcBlock::new(move |_timer: NonNull<NSTimer>| {
        if let Some((done, result)) = slot.take() {
            done(result);
        }
    });
    schedule(0.0, &block);
}

/// The text of a JavaScript value that should be a string. Values that come
/// from a page can be anything, so the type is checked.
fn as_string(value: *mut AnyObject) -> Option<String> {
    let value = unsafe { value.as_ref() }?;
    value.downcast_ref::<NSString>().map(NSString::to_string)
}

// ------------------------------------------------------------------- scripts

/// Runs the body of an async function and reports the JSON text of its result.
pub(crate) fn run_script(webview: &WebView, body: &str, done: Done<String>) -> Result<()> {
    let mtm = mtm()?;
    let wk = webview.webview();
    let world = unsafe { WKContentWorld::pageWorld(mtm) };
    // Wrap so the function returns the JSON of the body's value; `undefined`
    // becomes `null`. The body may use `await` and `return`.
    let wrapped = format!(
        "const __v = await (async () => {{\n{body}\n}})();\nreturn JSON.stringify(__v ?? null);"
    );
    let js = NSString::from_str(&wrapped);

    let slot = RefCell::new(Some(done));
    let block = RcBlock::new(move |result: *mut AnyObject, error: *mut NSError| {
        if let Some(done) = slot.take() {
            done(script_result(result, error));
        }
    });
    unsafe {
        wk.callAsyncJavaScript_arguments_inFrame_inContentWorld_completionHandler(
            &js,
            None,
            None,
            &world,
            Some(&block),
        );
    }
    Ok(())
}

fn script_result(result: *mut AnyObject, error: *mut NSError) -> Result<String> {
    if let Some(error) = unsafe { error.as_ref() } {
        return Err(Error::Script(error.localizedDescription().to_string()));
    }
    if result.is_null() {
        return Ok("null".to_owned());
    }
    // The wrapper resolves to a JSON string, unless the page replaced
    // `JSON.stringify`.
    as_string(result)
        .ok_or_else(|| Error::Script("the script's result could not be read as JSON".to_owned()))
}

/// After a main-frame load, reports the page's favicon URL, skipping a repeat
/// of `last`.
pub(crate) fn emit_favicon(
    webview: Retained<WryWebView>,
    events: EventSink,
    last: Rc<RefCell<String>>,
) {
    let Ok(mtm) = mtm() else {
        return;
    };
    // An isolated world, so the page cannot redefine the DOM functions the
    // script reads the address with.
    let world = unsafe { WKContentWorld::defaultClientWorld(mtm) };
    let js = NSString::from_str(FAVICON_JS);
    let block = RcBlock::new(move |result: *mut AnyObject, error: *mut NSError| {
        if !error.is_null() {
            return;
        }
        let Some(url) = as_string(result) else {
            return;
        };
        if !(url.starts_with("http://") || url.starts_with("https://")) {
            return;
        }
        if *last.borrow() == url {
            return;
        }
        *last.borrow_mut() = url.clone();
        emit(&events, BrowserEvent::FaviconChanged { url });
    });
    unsafe {
        webview.callAsyncJavaScript_arguments_inFrame_inContentWorld_completionHandler(
            &js,
            None,
            None,
            &world,
            Some(&block),
        );
    }
}

const FAVICON_JS: &str = r#"
const links = document.querySelectorAll("link[rel~='icon']");
if (links.length) return links[links.length - 1].href;
return location.origin + "/favicon.ico";
"#;

// ----------------------------------------------------------------- snapshots

/// Shared state of one snapshot request, so the capture and the timeout race
/// to finish it exactly once.
struct Snapshot {
    slot: RefCell<Option<Done<Image>>>,
    timer: RefCell<Option<Retained<NSTimer>>>,
    scale: f64,
}

impl Snapshot {
    fn finish(&self, result: Result<Image>) {
        let Some(done) = self.slot.take() else {
            return;
        };
        if let Some(timer) = self.timer.take() {
            timer.invalidate();
        }
        done(result);
    }

    fn is_finished(&self) -> bool {
        self.slot.borrow().is_none()
    }
}

/// Captures the view as RGBA. A view on screen uses `takeSnapshot`; one that
/// is not (hidden itself or through an ancestor, clipped away, or outside a
/// visible window), and any failure, falls back to rendering the page's PDF,
/// which needs no screen. An 8 s timeout reports an error. A view without a
/// size is an error straight away.
pub(crate) fn snapshot(webview: &WebView, done: Done<Image>) -> Result<()> {
    let mtm = mtm()?;
    let wk = webview.webview();
    let size = wk.bounds().size;
    if size.width <= 0.0 || size.height <= 0.0 {
        return Err(backend_error("the page has no size yet"));
    }
    let state = Rc::new(Snapshot {
        slot: RefCell::new(Some(done)),
        timer: RefCell::new(None),
        scale: backing_scale(&wk),
    });

    let timer_state = state.clone();
    let timer_block = RcBlock::new(move |_timer: NonNull<NSTimer>| {
        timer_state.finish(Err(backend_error("snapshot timed out")));
    });
    *state.timer.borrow_mut() = Some(schedule(8.0, &timer_block));

    if is_on_screen(&wk) {
        take_snapshot(wk, state, mtm);
    } else {
        render_pdf(&wk, state);
    }
    Ok(())
}

/// Whether the view is drawn on screen: in a visible window, with neither it
/// nor an ancestor hidden, and not clipped away by its ancestors.
fn is_on_screen(wk: &WryWebView) -> bool {
    let visible = wk.visibleRect().size;
    !wk.isHiddenOrHasHiddenAncestor()
        && visible.width > 0.0
        && visible.height > 0.0
        && wk.window().is_some_and(|window| window.isVisible())
}

fn take_snapshot(wk: Retained<WryWebView>, state: Rc<Snapshot>, mtm: MainThreadMarker) {
    let capture = state.clone();
    let fallback_view = wk.clone();
    let block = RcBlock::new(move |image: *mut NSImage, _error: *mut NSError| {
        let captured =
            unsafe { image.as_ref() }.and_then(|image| image_to_rgba(image, capture.scale));
        match captured {
            Some(image) => capture.finish(Ok(image)),
            None if !capture.is_finished() => render_pdf(&fallback_view, capture.clone()),
            None => {}
        }
    });
    // Capture the page as it is now rather than after the next screen update,
    // which does not come while the window is occluded.
    let config = unsafe { WKSnapshotConfiguration::new(mtm) };
    unsafe { config.setAfterScreenUpdates(false) };
    unsafe { wk.takeSnapshotWithConfiguration_completionHandler(Some(&config), &block) };
}

fn render_pdf(wk: &WryWebView, state: Rc<Snapshot>) {
    let block = RcBlock::new(move |data: *mut NSData, error: *mut NSError| {
        let image = unsafe { data.as_ref() }
            .and_then(|data| NSImage::initWithData(NSImage::alloc(), data))
            .and_then(|image| image_to_rgba(&image, state.scale));
        match image {
            Some(image) => state.finish(Ok(image)),
            None => {
                let message = match unsafe { error.as_ref() } {
                    Some(error) => error.localizedDescription().to_string(),
                    None => "snapshot produced no image".to_owned(),
                };
                state.finish(Err(backend_error(message)));
            }
        }
    });
    unsafe { wk.createPDFWithConfiguration_completionHandler(None, &block) };
}

/// The backing scale of the view's window, defaulting to 2 when it has none.
fn backing_scale(wk: &WryWebView) -> f64 {
    wk.window()
        .map(|window| window.backingScaleFactor())
        .filter(|scale| *scale > 0.0)
        .unwrap_or(2.0)
}

/// Draws an image onto white at `scale`, returning straight RGBA8, top row
/// first.
fn image_to_rgba(image: &NSImage, scale: f64) -> Option<Image> {
    let size = image.size();
    if size.width <= 0.0 || size.height <= 0.0 {
        return None;
    }
    let pixels_wide = (size.width * scale).round() as usize;
    let pixels_high = (size.height * scale).round() as usize;
    if pixels_wide == 0 || pixels_high == 0 {
        return None;
    }

    // A premultiplied RGBA8 bitmap: it is what Core Graphics bitmap contexts
    // accept, and because the page is drawn onto opaque white every pixel ends
    // up fully opaque, so premultiplied and straight values are identical.
    let rep = unsafe {
        NSBitmapImageRep::initWithBitmapDataPlanes_pixelsWide_pixelsHigh_bitsPerSample_samplesPerPixel_hasAlpha_isPlanar_colorSpaceName_bytesPerRow_bitsPerPixel(
            NSBitmapImageRep::alloc(),
            ptr::null_mut(),
            pixels_wide as isize,
            pixels_high as isize,
            8,
            4,
            true,
            false,
            NSDeviceRGBColorSpace,
            (pixels_wide * 4) as isize,
            32,
        )
    }?;
    rep.setSize(NSSize::new(size.width, size.height));

    let context = NSGraphicsContext::graphicsContextWithBitmapImageRep(&rep)?;
    let bytes_per_row = rep.bytesPerRow() as usize;
    // A fresh buffer is undefined; make it opaque white before drawing over it.
    unsafe { ptr::write_bytes(rep.bitmapData(), 0xFF, bytes_per_row * pixels_high) };

    let previous = NSGraphicsContext::currentContext();
    NSGraphicsContext::setCurrentContext(Some(&context));
    image.drawInRect_fromRect_operation_fraction(
        NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(size.width, size.height)),
        NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(0.0, 0.0)),
        NSCompositingOperation::SourceOver,
        1.0,
    );
    context.flushGraphics();
    NSGraphicsContext::setCurrentContext(previous.as_deref());

    let data = rep.bitmapData();
    if data.is_null() {
        return None;
    }
    let mut rgba = Vec::with_capacity(pixels_wide * pixels_high * 4);
    for row in 0..pixels_high {
        let start = unsafe { data.add(row * bytes_per_row) };
        rgba.extend_from_slice(unsafe { std::slice::from_raw_parts(start, pixels_wide * 4) });
    }
    Some(Image {
        width: pixels_wide as u32,
        height: pixels_high as u32,
        rgba,
    })
}

// ---------------------------------------------------------------------- find

pub(crate) fn find(
    webview: &WebView,
    query: &str,
    options: FindOptions,
    done: Done<bool>,
) -> Result<()> {
    let mtm = mtm()?;
    let wk = webview.webview();
    let config = unsafe { WKFindConfiguration::new(mtm) };
    unsafe {
        config.setBackwards(options.backwards);
        config.setCaseSensitive(options.case_sensitive);
        config.setWraps(options.wraps);
    }
    let query = NSString::from_str(query);
    let slot = Rc::new(RefCell::new(Some(done)));
    let block = RcBlock::new(move |result: NonNull<WKFindResult>| {
        if let Some(done) = slot.borrow_mut().take() {
            done(Ok(unsafe { result.as_ref().matchFound() }));
        }
    });
    unsafe { wk.findString_withConfiguration_completionHandler(&query, Some(&config), &block) };
    Ok(())
}

// ------------------------------------------------------------------- cookies

fn cookie_store(wk: &WryWebView) -> Retained<WKHTTPCookieStore> {
    unsafe { wk.configuration().websiteDataStore().httpCookieStore() }
}

fn to_cookie(cookie: &NSHTTPCookie) -> Cookie {
    Cookie {
        name: cookie.name().to_string(),
        value: cookie.value().to_string(),
        domain: cookie.domain().to_string(),
        path: cookie.path().to_string(),
        secure: cookie.isSecure(),
        http_only: cookie.isHTTPOnly(),
        expires: cookie.expiresDate().map(|date| {
            Timestamp::from_unix_micros((date.timeIntervalSince1970() * 1_000_000.0) as i64)
        }),
    }
}

pub(crate) fn cookies(webview: &WebView, url: Option<&str>, done: Done<Vec<Cookie>>) -> Result<()> {
    let _ = mtm()?;
    let store = cookie_store(&webview.webview());
    let url = url.map(str::to_owned);
    let slot = Rc::new(RefCell::new(Some(done)));
    let block = RcBlock::new(move |array: NonNull<NSArray<NSHTTPCookie>>| {
        if let Some(done) = slot.borrow_mut().take() {
            let array = unsafe { array.as_ref() };
            let mut cookies = Vec::new();
            for index in 0..array.count() {
                let cookie = to_cookie(&array.objectAtIndex(index));
                if url.as_deref().is_none_or(|url| cookie.matches_url(url)) {
                    cookies.push(cookie);
                }
            }
            done(Ok(cookies));
        }
    });
    unsafe { store.getAllCookies(&block) };
    Ok(())
}

pub(crate) fn set_cookie(webview: &WebView, cookie: Cookie, done: Done<()>) -> Result<()> {
    mtm()?;
    let store = cookie_store(&webview.webview());
    let ns_cookie = build_cookie(&cookie).ok_or_else(|| backend_error("invalid cookie"))?;
    let slot = Rc::new(RefCell::new(Some(done)));
    let block = RcBlock::new(move || {
        if let Some(done) = slot.borrow_mut().take() {
            done(Ok(()));
        }
    });
    unsafe { store.setCookie_completionHandler(&ns_cookie, Some(&block)) };
    Ok(())
}

pub(crate) fn delete_cookies(
    webview: &WebView,
    name: &str,
    domain: Option<&str>,
    done: Done<usize>,
) -> Result<()> {
    let _ = mtm()?;
    let store = cookie_store(&webview.webview());
    let name = name.to_owned();
    let domain = domain.map(str::to_owned);
    let slot = RefCell::new(Some(done));
    let delete_store = store.clone();
    let block = RcBlock::new(move |array: NonNull<NSArray<NSHTTPCookie>>| {
        let Some(done) = slot.take() else {
            return;
        };
        let array = unsafe { array.as_ref() };
        let matching: Vec<_> = (0..array.count())
            .map(|index| array.objectAtIndex(index))
            .filter(|cookie| {
                cookie.name().to_string() == name
                    && domain
                        .as_deref()
                        .is_none_or(|domain| cookie.domain().to_string() == domain)
            })
            .collect();
        if matching.is_empty() {
            done(Ok(0));
            return;
        }
        let deleted = count_completions(matching.len(), done);
        for cookie in &matching {
            unsafe { delete_store.deleteCookie_completionHandler(cookie, Some(&deleted)) };
        }
    });
    unsafe { store.getAllCookies(&block) };
    Ok(())
}

/// One completion handler for `count` cookie-store operations: once every one
/// of them has finished, reports `count`.
fn count_completions(count: usize, done: Done<usize>) -> RcBlock<dyn Fn()> {
    let finished = Cell::new(0);
    let slot = RefCell::new(Some(done));
    RcBlock::new(move || {
        finished.set(finished.get() + 1);
        if finished.get() == count {
            if let Some(done) = slot.take() {
                done(Ok(count));
            }
        }
    })
}

pub(crate) fn clear_cookies(webview: &WebView, done: Done<()>) -> Result<()> {
    let mtm = mtm()?;
    let data_store = unsafe { webview.webview().configuration().websiteDataStore() };
    remove_cookie_data(&data_store, mtm, done);
    Ok(())
}

fn remove_cookie_data(data_store: &WKWebsiteDataStore, mtm: MainThreadMarker, done: Done<()>) {
    let types = NSSet::from_slice(&[unsafe { WKWebsiteDataTypeCookies }]);
    let date = NSDate::distantPast();
    let slot = Rc::new(RefCell::new(Some(done)));
    let block = RcBlock::new(move || {
        if let Some(done) = slot.borrow_mut().take() {
            done(Ok(()));
        }
    });
    let _ = mtm;
    unsafe { data_store.removeDataOfTypes_modifiedSince_completionHandler(&types, &date, &block) };
}

/// Builds an `NSHTTPCookie` from a `Set-Cookie` header, the one path that
/// honors `HttpOnly` (which `NSHTTPCookie`'s property keys do not), as if the
/// cookie's host had sent it. Only a domain with a leading dot becomes a
/// `Domain` attribute; without one the cookie stays host-only.
fn build_cookie(cookie: &Cookie) -> Option<Retained<NSHTTPCookie>> {
    let scheme = if cookie.secure { "https" } else { "http" };
    let host = cookie.domain.trim_start_matches('.');
    let path = if cookie.path.is_empty() {
        "/"
    } else {
        &cookie.path
    };
    let url = NSURL::URLWithString(&NSString::from_str(&format!("{scheme}://{host}{path}")))?;

    let mut header = format!("{}={}; Path={}", cookie.name, cookie.value, path);
    if cookie.domain.starts_with('.') {
        header.push_str(&format!("; Domain={}", cookie.domain));
    }
    if cookie.secure {
        header.push_str("; Secure");
    }
    if cookie.http_only {
        header.push_str("; HttpOnly");
    }
    if let Some(expires) = cookie.expires {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|since| since.as_secs() as i64)
            .unwrap_or(0);
        header.push_str(&format!(
            "; Max-Age={}",
            (expires.as_unix_secs() - now).max(0)
        ));
    }

    let fields = NSDictionary::<NSString, NSString>::from_slices(
        &[&*NSString::from_str("Set-Cookie")],
        &[&*NSString::from_str(&header)],
    );
    let cookies = NSHTTPCookie::cookiesWithResponseHeaderFields_forURL(&fields, &url);
    (cookies.count() > 0).then(|| cookies.objectAtIndex(0))
}

// -------------------------------------------------- data stores without a view

/// The WebKit data store of a persistent profile.
fn profile_data_store(profile: &Profile, mtm: MainThreadMarker) -> Retained<WKWebsiteDataStore> {
    let uuid = NSUUID::from_bytes(*profile.id().as_bytes());
    unsafe { WKWebsiteDataStore::dataStoreForIdentifier(&uuid, mtm) }
}

/// Keeps `data_store` alive until `done` runs: no view may be holding it.
fn holding<T: 'static>(data_store: Retained<WKWebsiteDataStore>, done: Done<T>) -> Done<T> {
    Box::new(move |result| {
        drop(data_store);
        done(result);
    })
}

/// Clears cookies, caches and storage of a profile's WebKit data store.
pub(crate) fn clear_site_data(profile: &Profile, done: Done<()>) -> Result<()> {
    let mtm = mtm()?;
    if profile.is_ephemeral() {
        defer(done, Ok(()));
        return Ok(());
    }
    let data_store = profile_data_store(profile, mtm);
    let types = unsafe { WKWebsiteDataStore::allWebsiteDataTypes(mtm) };
    let date = NSDate::distantPast();
    let slot = RefCell::new(Some(holding(data_store.clone(), done)));
    let block = RcBlock::new(move || {
        if let Some(done) = slot.take() {
            done(Ok(()));
        }
    });
    unsafe { data_store.removeDataOfTypes_modifiedSince_completionHandler(&types, &date, &block) };
    Ok(())
}

/// Writes cookies into a profile's WebKit data store and, once WebKit has
/// stored them, reports how many.
pub(crate) fn import_cookies(
    profile: &Profile,
    cookies: Vec<Cookie>,
    done: Done<usize>,
) -> Result<()> {
    let mtm = mtm()?;
    let ns_cookies: Vec<_> = if profile.is_ephemeral() {
        Vec::new()
    } else {
        cookies.iter().filter_map(build_cookie).collect()
    };
    if ns_cookies.is_empty() {
        defer(done, Ok(0));
        return Ok(());
    }
    let data_store = profile_data_store(profile, mtm);
    let store = unsafe { data_store.httpCookieStore() };
    let stored = count_completions(ns_cookies.len(), holding(data_store, done));
    for ns_cookie in &ns_cookies {
        unsafe { store.setCookie_completionHandler(ns_cookie, Some(&stored)) };
    }
    Ok(())
}

// ---------------------------------- navigation events, KVO, window.close()

const KVO_KEYS: [&str; 3] = ["estimatedProgress", "canGoBack", "canGoForward"];

/// The message handler the `window.close()` shim posts to.
const CLOSE_HANDLER: &str = "polybrowserClose";

/// Turns `window.close()` into a message to [`CLOSE_HANDLER`]. It is injected
/// into the main frame only, but any frame can post to the handler, so the
/// handler checks the sender as well.
pub(crate) const CLOSE_SHIM: &str = r#"(function () {
  if (window !== window.top) return;
  window.close = function () {
    try { window.webkit.messageHandlers.polybrowserClose.postMessage(null); } catch (error) {}
  };
})();"#;

/// Native observers that live for as long as the view. Dropping it detaches
/// the KVO observer and the close handler before the web view goes away.
pub(crate) struct Observers {
    webview: Retained<WryWebView>,
    _navigation: Retained<NavigationProxy>,
    kvo: Retained<KvoObserver>,
    _close: Retained<CloseHandler>,
}

impl Drop for Observers {
    fn drop(&mut self) {
        for key in KVO_KEYS {
            unsafe {
                self.webview
                    .removeObserver_forKeyPath(&self.kvo, &NSString::from_str(key));
            }
        }
        unsafe {
            self.webview
                .configuration()
                .userContentController()
                .removeScriptMessageHandlerForName(&NSString::from_str(CLOSE_HANDLER));
        }
    }
}

/// Wraps wry's navigation delegate to add `LoadFailed` and
/// `ExternalLinkRequested`, starts observing load progress and navigation
/// state, and receives the `window.close()` shim's messages.
pub(crate) fn install(webview: &WebView, events: EventSink) -> Observers {
    let mtm = MainThreadMarker::new().expect("views are created on the main thread");
    let wk = webview.webview();

    // wry always installs a navigation delegate; wrap it so unhandled
    // selectors forward to it and the two failure callbacks reach us.
    let inner = unsafe { wk.navigationDelegate() }.expect("wry sets a navigation delegate");
    let navigation = NavigationProxy::new(mtm, inner, events.clone());
    unsafe { wk.setNavigationDelegate(Some(ProtocolObject::from_ref(&*navigation))) };

    let close = CloseHandler::new(mtm, events.clone());
    unsafe {
        wk.configuration()
            .userContentController()
            .addScriptMessageHandler_name(
                ProtocolObject::from_ref(&*close),
                &NSString::from_str(CLOSE_HANDLER),
            );
    }

    let kvo = KvoObserver::new(mtm, events, wk.clone());
    for key in KVO_KEYS {
        unsafe {
            wk.addObserver_forKeyPath_options_context(
                &kvo,
                &NSString::from_str(key),
                NSKeyValueObservingOptions::New,
                ptr::null_mut(),
            );
        }
    }

    Observers {
        webview: wk,
        _navigation: navigation,
        kvo,
        _close: close,
    }
}

struct CloseHandlerIvars {
    events: EventSink,
}

define_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[ivars = CloseHandlerIvars]
    struct CloseHandler;

    unsafe impl NSObjectProtocol for CloseHandler {}

    unsafe impl WKScriptMessageHandler for CloseHandler {
        #[unsafe(method(userContentController:didReceiveScriptMessage:))]
        fn did_receive(&self, _controller: &WKUserContentController, message: &WKScriptMessage) {
            // A frame inside the page must not close the view.
            if unsafe { message.frameInfo().isMainFrame() } {
                emit(&self.ivars().events, BrowserEvent::CloseRequested);
            }
        }
    }
);

impl CloseHandler {
    fn new(mtm: MainThreadMarker, events: EventSink) -> Retained<Self> {
        let this = mtm
            .alloc::<CloseHandler>()
            .set_ivars(CloseHandlerIvars { events });
        unsafe { msg_send![super(this), init] }
    }
}

fn is_external_scheme(url: &str) -> bool {
    let scheme = url.split(':').next().unwrap_or("").to_ascii_lowercase();
    !scheme.is_empty()
        && !matches!(
            scheme.as_str(),
            "http" | "https" | "about" | "data" | "blob" | "file" | "javascript"
        )
}

struct NavigationProxyIvars {
    inner: Retained<ProtocolObject<dyn WKNavigationDelegate>>,
    events: EventSink,
}

define_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[ivars = NavigationProxyIvars]
    struct NavigationProxy;

    unsafe impl NSObjectProtocol for NavigationProxy {}

    // Transparent forwarding: claim the selectors wry's delegate handles, and
    // hand those messages to it.
    impl NavigationProxy {
        #[unsafe(method(respondsToSelector:))]
        fn responds_to_selector(&self, selector: Sel) -> bool {
            let here: bool = unsafe { msg_send![super(self), respondsToSelector: selector] };
            here || unsafe { msg_send![&*self.ivars().inner, respondsToSelector: selector] }
        }

        #[unsafe(method(forwardingTargetForSelector:))]
        fn forwarding_target(&self, selector: Sel) -> *mut AnyObject {
            let inner = &*self.ivars().inner;
            let responds: bool = unsafe { msg_send![inner, respondsToSelector: selector] };
            if responds {
                (inner as *const ProtocolObject<dyn WKNavigationDelegate>) as *mut AnyObject
            } else {
                ptr::null_mut()
            }
        }
    }

    unsafe impl WKNavigationDelegate for NavigationProxy {
        #[unsafe(method(webView:decidePolicyForNavigationAction:decisionHandler:))]
        fn decide_policy(
            &self,
            web_view: &WKWebView,
            action: &WKNavigationAction,
            handler: &block2::DynBlock<dyn Fn(WKNavigationActionPolicy)>,
        ) {
            let user_click = unsafe { action.navigationType() } == WKNavigationType::LinkActivated;
            let url = unsafe { action.request() }
                .URL()
                .and_then(|url| url.absoluteString())
                .map(|url| url.to_string())
                .unwrap_or_default();
            if user_click && is_external_scheme(&url) {
                // Cancel the engine's own navigation and hand the link up.
                (*handler).call((WKNavigationActionPolicy::Cancel,));
                emit(&self.ivars().events, BrowserEvent::ExternalLinkRequested { url });
                return;
            }
            let inner = &*self.ivars().inner;
            unsafe {
                let _: () = msg_send![
                    inner,
                    webView: web_view,
                    decidePolicyForNavigationAction: action,
                    decisionHandler: handler,
                ];
            }
        }

        #[unsafe(method(webView:didFailNavigation:withError:))]
        fn did_fail(&self, web_view: &WKWebView, _navigation: Option<&WKNavigation>, error: &NSError) {
            self.report_failure(web_view, error);
        }

        #[unsafe(method(webView:didFailProvisionalNavigation:withError:))]
        fn did_fail_provisional(
            &self,
            web_view: &WKWebView,
            _navigation: Option<&WKNavigation>,
            error: &NSError,
        ) {
            self.report_failure(web_view, error);
        }
    }
);

impl NavigationProxy {
    fn new(
        mtm: MainThreadMarker,
        inner: Retained<ProtocolObject<dyn WKNavigationDelegate>>,
        events: EventSink,
    ) -> Retained<Self> {
        let this = mtm
            .alloc::<NavigationProxy>()
            .set_ivars(NavigationProxyIvars { inner, events });
        unsafe { msg_send![super(this), init] }
    }

    fn report_failure(&self, web_view: &WKWebView, error: &NSError) {
        // Cancellations (-999) and "frame load interrupted by policy change"
        // (102) are not failures.
        let code = error.code();
        if code == -999 || code == 102 {
            return;
        }
        emit(
            &self.ivars().events,
            BrowserEvent::LoadFailed {
                url: failing_url(web_view, error),
                error: error.localizedDescription().to_string(),
            },
        );
    }
}

/// The address that failed to load, from the error when it says, else the
/// view's address. The error's values are checked for type before use.
fn failing_url(web_view: &WKWebView, error: &NSError) -> String {
    let info = error.userInfo();
    let value = |key: &str| info.objectForKey(&NSString::from_str(key));
    let from_string = value("NSErrorFailingURLStringKey")
        .and_then(|value| value.downcast_ref::<NSString>().map(NSString::to_string));
    let from_url = || {
        value("NSErrorFailingURLKey").and_then(|value| {
            let url = value.downcast_ref::<NSURL>()?.absoluteString()?;
            Some(url.to_string())
        })
    };
    let from_view = || {
        unsafe { web_view.URL() }
            .and_then(|url| url.absoluteString())
            .map(|url| url.to_string())
    };
    from_string
        .or_else(from_url)
        .or_else(from_view)
        .unwrap_or_default()
}

struct KvoObserverIvars {
    events: EventSink,
    webview: Retained<WryWebView>,
    last_navigation_state: Cell<(bool, bool)>,
}

define_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[ivars = KvoObserverIvars]
    struct KvoObserver;

    unsafe impl NSObjectProtocol for KvoObserver {}

    impl KvoObserver {
        #[unsafe(method(observeValueForKeyPath:ofObject:change:context:))]
        fn observe(
            &self,
            key_path: *mut NSString,
            _object: *mut AnyObject,
            _change: *mut AnyObject,
            _context: *mut c_void,
        ) {
            if key_path.is_null() {
                return;
            }
            let key = unsafe { &*key_path }.to_string();
            let webview = &self.ivars().webview;
            if key == "estimatedProgress" {
                let progress = unsafe { webview.estimatedProgress() };
                emit(&self.ivars().events, BrowserEvent::ProgressChanged { progress });
            } else {
                let state = (unsafe { webview.canGoBack() }, unsafe { webview.canGoForward() });
                if self.ivars().last_navigation_state.get() != state {
                    self.ivars().last_navigation_state.set(state);
                    emit(
                        &self.ivars().events,
                        BrowserEvent::NavigationStateChanged {
                            can_go_back: state.0,
                            can_go_forward: state.1,
                        },
                    );
                }
            }
        }
    }
);

impl KvoObserver {
    fn new(
        mtm: MainThreadMarker,
        events: EventSink,
        webview: Retained<WryWebView>,
    ) -> Retained<Self> {
        let this = mtm.alloc::<KvoObserver>().set_ivars(KvoObserverIvars {
            events,
            webview,
            last_navigation_state: Cell::new((false, false)),
        });
        unsafe { msg_send![super(this), init] }
    }
}
