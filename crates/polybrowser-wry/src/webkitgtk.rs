use gtk::prelude::*;
use polybrowser_core::{
    backend::{Done, ProfileStorage},
    Cookie, Engine, Error, Profile, Result,
};
pub(super) use polybrowser_webkitgtk::NativeView;
use polybrowser_webkitgtk::SiteData;
use wry::WebViewExtUnix;
use wry::{WebViewBuilder, WebViewBuilderExtUnix};
pub(super) fn native(view: &wry::WebView) -> Result<NativeView> {
    unsafe { NativeView::from_raw(view.webview().as_ptr().cast()) }
}
struct Store {
    view: wry::WebView,
    host: gtk::OffscreenWindow,
    _context: super::platform::SharedContext,
}
impl Store {
    fn open(profile: &Profile) -> Result<Self> {
        if !gtk::is_initialized_main_thread() {
            return Err(Error::NotMainThread);
        }
        let data_dir = profile
            .engine_data_dir(Engine::WebKit)
            .ok_or_else(|| Error::unsupported("ephemeral profile"))?;
        std::fs::create_dir_all(&data_dir)?;
        let context = super::platform::context_for(&ProfileStorage::Persistent {
            id: profile.id(),
            data_dir,
        })
        .unwrap();
        let host = gtk::OffscreenWindow::new();
        let container = gtk::Box::new(gtk::Orientation::Vertical, 0);
        host.add(&container);
        let view = WebViewBuilder::new_with_web_context(&mut context.borrow_mut())
            .build_gtk(&container)
            .map_err(super::error)?;
        Ok(Self {
            view,
            host,
            _context: context,
        })
    }
    fn data(&self) -> Result<SiteData> {
        Ok(native(&self.view)?.site_data())
    }
}
impl Drop for Store {
    fn drop(&mut self) {
        unsafe {
            self.host.destroy();
        }
    }
}
pub(super) fn clear_site_data(profile: &Profile, done: Done<()>) -> Result<()> {
    if profile.engine_data_dir(Engine::WebKit).is_none() {
        done(Ok(()));
        return Ok(());
    }
    let store = Store::open(profile)?;
    store.data()?.clear(Box::new(move |result| {
        drop(store);
        done(result);
    }))
}
pub(super) fn import_cookies(
    profile: &Profile,
    cookies: Vec<Cookie>,
    done: Done<usize>,
) -> Result<()> {
    if profile.engine_data_dir(Engine::WebKit).is_none() {
        done(Ok(0));
        return Ok(());
    }
    let store = Store::open(profile)?;
    store.data()?.import(
        cookies,
        Box::new(move |result| {
            drop(store);
            done(result);
        }),
    )
}
