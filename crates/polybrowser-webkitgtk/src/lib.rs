//! Browser operations for a native WebKitGTK 4.1 view (WebKit 2.42 or later).
//! Libraries are loaded on first use. The host owns GTK initialization and
//! its event loop; all objects and callbacks remain on that thread.
#![cfg(target_os = "linux")]

use ffi::{gpointer, Api, GError};
use polybrowser_core::{
    self as polybrowser, backend::Done, Cookie, Engine, Error, FindOptions, Image, Result,
};
use std::{
    cell::{Cell, RefCell},
    collections::HashMap,
    ffi::{c_int, CStr, CString},
    ptr,
    rc::{Rc, Weak},
};
mod callbacks;
mod capture;
mod cookies;
mod ffi;
mod pixels;

fn error(message: impl std::fmt::Display) -> Error {
    Error::backend(Engine::WebKit, message)
}
fn cstring(value: &str) -> CString {
    CString::new(value).unwrap_or_default()
}
unsafe fn text(value: *const std::ffi::c_char) -> Option<String> {
    if value.is_null() {
        None
    } else {
        Some(
            unsafe { CStr::from_ptr(value) }
                .to_string_lossy()
                .into_owned(),
        )
    }
}
unsafe fn owned(g: &Api, value: *mut std::ffi::c_char) -> Option<String> {
    let result = unsafe { text(value) };
    if !value.is_null() {
        unsafe {
            g.g_free(value.cast());
        }
    }
    result
}

/// A retained native view. Dropping it cancels all pending page operations.
pub struct NativeView {
    native: gpointer,
    requests: Rc<Requests>,
    signals: Vec<std::ffi::c_ulong>,
}
#[derive(Default)]
struct Requests {
    next: Cell<u64>,
    pending: RefCell<HashMap<u64, Cancellation>>,
    capturing: Cell<bool>,
    visible: Cell<bool>,
}
struct Cancellation {
    native: gpointer,
    timer: u32,
}
impl Cancellation {
    fn release(self, g: &Api, cancel: bool) {
        unsafe {
            g.g_source_remove(self.timer);
            if cancel {
                g.g_cancellable_cancel(self.native);
            }
            g.g_object_unref(self.native);
        }
    }
}
impl Requests {
    fn cancel(&self) {
        if let Some(g) = ffi::loaded() {
            for pending in self.pending.take().into_values() {
                pending.release(g, true);
            }
        }
    }
    fn begin<T: 'static>(
        self: &Rc<Self>,
        g: &Api,
        done: Done<T>,
    ) -> Result<(Box<Pending<T>>, gpointer)> {
        if self.pending.borrow().len() >= 128 {
            return Err(error("too many pending browser requests"));
        }
        let id = self.next.get();
        self.next.set(id.wrapping_add(1));
        let native = unsafe { g.g_cancellable_new() };
        let timer = unsafe {
            g.g_timeout_add_full(
                0,
                60_000,
                on_timeout,
                g.g_object_ref(native),
                Some(release_timeout),
            )
        };
        self.pending
            .borrow_mut()
            .insert(id, Cancellation { native, timer });
        Ok((
            Box::new(Pending {
                id,
                requests: Rc::downgrade(self),
                done,
            }),
            native,
        ))
    }
}
struct Pending<T> {
    id: u64,
    requests: Weak<Requests>,
    done: Done<T>,
}
impl<T> Pending<T> {
    fn finish(self, result: Result<T>) {
        let live = self
            .requests
            .upgrade()
            .and_then(|requests| requests.pending.borrow_mut().remove(&self.id));
        let valid = if let (Some(pending), Some(g)) = (live, ffi::loaded()) {
            let valid = unsafe { g.g_cancellable_is_cancelled(pending.native) == 0 };
            pending.release(g, false);
            valid
        } else {
            false
        };
        (self.done)(if valid {
            result
        } else {
            Err(error("browser document retired or operation timed out"))
        });
    }
}
unsafe extern "C" fn on_timeout(data: gpointer) -> c_int {
    if let Some(g) = ffi::loaded() {
        unsafe {
            g.g_cancellable_cancel(data);
        }
    }
    1
}
unsafe extern "C" fn release_timeout(data: gpointer) {
    if let Some(g) = ffi::loaded() {
        unsafe {
            g.g_object_unref(data);
        }
    }
}
unsafe extern "C" fn on_load(_view: gpointer, event: c_int, data: gpointer) {
    if event == 0 {
        if let Some(requests) = unsafe { &*data.cast::<Weak<Requests>>() }.upgrade() {
            requests.cancel();
        }
    }
}
unsafe extern "C" fn on_destroy(_view: gpointer, data: gpointer) {
    if let Some(requests) = unsafe { &*data.cast::<Weak<Requests>>() }.upgrade() {
        requests.cancel();
    }
}
unsafe extern "C" fn free_signal(data: gpointer, _closure: gpointer) {
    drop(unsafe { Box::from_raw(data.cast::<Weak<Requests>>()) });
}
impl NativeView {
    /// Retains a WebKitWebView owned by the calling GTK thread.
    ///
    /// # Safety
    /// `view` must be a live WebKitWebView. The host must initialize GTK and
    /// drive its event loop on this thread; all use and drop stay here.
    pub unsafe fn from_raw(view: gpointer) -> Result<Self> {
        if view.is_null() {
            return Err(error("null browser view"));
        }
        let g = ffi::load()?;
        let requests = Rc::new(Requests {
            visible: Cell::new(unsafe { g.gtk_widget_get_visible(view) != 0 }),
            ..Requests::default()
        });
        let load: ffi::Callback = unsafe {
            std::mem::transmute(on_load as unsafe extern "C" fn(gpointer, c_int, gpointer))
        };
        let destroy: ffi::Callback =
            unsafe { std::mem::transmute(on_destroy as unsafe extern "C" fn(gpointer, gpointer)) };
        let signals = [(c"load-changed", load), (c"destroy", destroy)]
            .into_iter()
            .map(|(name, handler)| unsafe {
                g.g_signal_connect_data(
                    view,
                    name.as_ptr(),
                    handler,
                    Box::into_raw(Box::new(Rc::downgrade(&requests))).cast(),
                    free_signal as *const () as gpointer,
                    0,
                )
            })
            .collect();
        Ok(Self {
            native: unsafe { g.g_object_ref(view) },
            requests,
            signals,
        })
    }
    /// Cancels page requests, for example when the host retires a document.
    pub fn cancel(&self) {
        self.requests.cancel();
    }
    /// Sets the desired visibility, including while a hidden capture is pending.
    pub fn set_visible(&self, visible: bool) {
        self.requests.visible.set(visible);
        if !self.requests.capturing.get() {
            if let Some(g) = ffi::loaded() {
                unsafe {
                    if visible {
                        g.gtk_widget_show(self.native);
                    } else {
                        g.gtk_widget_hide(self.native);
                    }
                }
            }
        }
    }
    /// Runs an async function body and reports JSON, awaiting promises.
    pub fn run_script(&self, body: &str, done: Done<String>) -> Result<()> {
        let g = ffi::load()?;
        let source =
            format!("const __v = await (async () => {{\n{body}\n}})();\nreturn JSON.stringify(__v ?? null);");
        let (pending, cancel) = self.requests.begin(g, done)?;
        unsafe {
            g.webkit_web_view_call_async_javascript_function(
                self.native,
                source.as_ptr().cast(),
                source.len() as isize,
                ptr::null_mut(),
                ptr::null(),
                ptr::null(),
                cancel,
                callbacks::on_script,
                Box::into_raw(pending).cast(),
            );
        }
        Ok(())
    }
    /// Finds and selects text in the page.
    pub fn find(&self, query: &str, options: FindOptions, done: Done<bool>) -> Result<()> {
        let query = serde_json::to_string(query).map_err(error)?;
        self.run_script(
            &format!(
                "return window.find({query}, {}, {}, {}, false, true, false);",
                options.case_sensitive, options.backwards, options.wraps
            ),
            Box::new(move |result| {
                done(result.and_then(|json| serde_json::from_str(&json).map_err(error)))
            }),
        )
    }
    /// Captures straight RGBA pixels, temporarily mapping hidden views offscreen.
    pub fn snapshot(&self, done: Done<Image>) -> Result<()> {
        let g = ffi::load()?;
        if self.requests.capturing.replace(true) {
            return Err(error("a snapshot is already pending"));
        }
        let capture =
            match unsafe { capture::Capture::new(g, self.native, Rc::downgrade(&self.requests)) } {
                Ok(capture) => capture,
                Err(failure) => {
                    self.requests.capturing.set(false);
                    return Err(failure);
                }
            };
        let requests = Rc::downgrade(&self.requests);
        let done = Box::new(move |result| {
            drop(capture);
            if let Some(requests) = requests.upgrade() {
                requests.capturing.set(false);
            }
            done(result);
        });
        let (pending, cancel) = match self.requests.begin(g, done) {
            Ok(pending) => pending,
            Err(failure) => {
                self.requests.capturing.set(false);
                return Err(failure);
            }
        };
        let request = Box::new(capture::Request {
            view: self.native,
            cancel,
            pending,
        });
        unsafe {
            g.g_object_ref(self.native);
            g.g_object_ref(cancel);
            g.g_timeout_add_full(0, 50, capture::start, Box::into_raw(request).cast(), None);
        }
        Ok(())
    }
    /// The site's profile store, retained independently of this view.
    pub fn site_data(&self) -> SiteData {
        unsafe {
            SiteData::from_raw(
                ffi::loaded()
                    .unwrap()
                    .webkit_web_view_get_context(self.native),
            )
            .expect("loaded WebKit")
        }
    }
    /// Reads all cookies or only those sent to a URL.
    pub fn cookies(&self, url: Option<&str>, done: Done<Vec<Cookie>>) -> Result<()> {
        self.site_data().cookies(url, done)
    }
    /// Writes one cookie.
    pub fn set_cookie(&self, cookie: Cookie, done: Done<()>) -> Result<()> {
        self.site_data().set_cookie(cookie, done)
    }
    /// Deletes matching cookies and reports the count.
    pub fn delete_cookies(
        &self,
        name: &str,
        domain: Option<&str>,
        done: Done<usize>,
    ) -> Result<()> {
        self.site_data().delete_cookies(name, domain, done)
    }
    /// Removes all cookies in the view's profile.
    pub fn clear_cookies(&self, done: Done<()>) -> Result<()> {
        self.site_data().clear_cookies(done)
    }
}
impl Drop for NativeView {
    fn drop(&mut self) {
        self.requests.cancel();
        if let Some(g) = ffi::loaded() {
            unsafe {
                for signal in self.signals.drain(..) {
                    if g.g_signal_handler_is_connected(self.native, signal) != 0 {
                        g.g_signal_handler_disconnect(self.native, signal);
                    }
                }
                g.g_object_unref(self.native);
            }
        }
    }
}

/// Retained WebKit profile data. Operations finish on the GTK thread.
pub struct SiteData {
    native: gpointer,
    _thread: std::marker::PhantomData<Rc<()>>,
}
impl SiteData {
    /// Retains a native WebKitWebContext on the GTK thread.
    ///
    /// # Safety
    /// The context must be live and owned by this thread. The host must drive
    /// GTK's event loop here until all callbacks complete.
    pub unsafe fn from_raw(context: gpointer) -> Result<Self> {
        if context.is_null() {
            return Err(error("null browser context"));
        }
        Ok(Self {
            native: unsafe { ffi::load()?.g_object_ref(context) },
            _thread: std::marker::PhantomData,
        })
    }
    fn retained(&self) -> Rc<Self> {
        Rc::new(unsafe { Self::from_raw(self.native).expect("loaded WebKit") })
    }
    /// Lists cookies, optionally filtered by the URL they would be sent to.
    pub fn cookies(&self, url: Option<&str>, done: Done<Vec<Cookie>>) -> Result<()> {
        cookies::list(ffi::load()?, self.retained(), url, done);
        Ok(())
    }
    /// Stores one cookie.
    pub fn set_cookie(&self, cookie: Cookie, done: Done<()>) -> Result<()> {
        cookies::change(ffi::load()?, self.retained(), &cookie, false, done);
        Ok(())
    }
    /// Removes matching cookies and reports the count.
    pub fn delete_cookies(
        &self,
        name: &str,
        domain: Option<&str>,
        done: Done<usize>,
    ) -> Result<()> {
        let context = self.retained();
        let (name, domain) = (name.to_owned(), domain.map(str::to_owned));
        let g = ffi::load()?;
        cookies::list(
            g,
            context.clone(),
            None,
            Box::new(move |result| match result {
                Err(failure) => done(Err(failure)),
                Ok(cookies) => {
                    let cookies = cookies
                        .into_iter()
                        .filter(|cookie| {
                            cookie.name == name
                                && domain
                                    .as_ref()
                                    .is_none_or(|domain| domain == &cookie.domain)
                        })
                        .collect::<Vec<_>>();
                    cookies::sequence(g, context, cookies.into_iter(), true, 0, done);
                }
            }),
        );
        Ok(())
    }
    /// Removes all cookies.
    pub fn clear_cookies(&self, done: Done<()>) -> Result<()> {
        cookies::clear(ffi::load()?, self.retained(), 256, done);
        Ok(())
    }
    /// Removes all website data, including cookies, caches and local storage.
    pub fn clear(&self, done: Done<()>) -> Result<()> {
        cookies::clear(ffi::load()?, self.retained(), 16383, done);
        Ok(())
    }
    /// Imports cookies in sequence and reports the number written.
    pub fn import(&self, cookies: Vec<Cookie>, done: Done<usize>) -> Result<()> {
        cookies::sequence(
            ffi::load()?,
            self.retained(),
            cookies.into_iter(),
            false,
            0,
            done,
        );
        Ok(())
    }
}
impl Drop for SiteData {
    fn drop(&mut self) {
        if let Some(g) = ffi::loaded() {
            unsafe {
                g.g_object_unref(self.native);
            }
        }
    }
}

/// Checks that the runtime provides the browser APIs, without initializing GTK.
pub fn probe() -> Result<()> {
    ffi::load().map(|_| ())
}
