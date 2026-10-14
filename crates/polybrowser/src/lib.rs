//! A web browser for any desktop application.
//!
//! `polybrowser` does not implement a browser engine. It embeds the ones that
//! exist behind one API, and adds the parts of a browser that sit above the
//! engine:
//!
//! - **Engines.** [`Engine::WebKit`] and [`Engine::WebView2`] come with the
//!   operating system; [`Engine::Chromium`] is bundled with the application.
//!   Cargo features decide which are compiled in, and each [`Browser`] picks
//!   one of those at run time, so an application can offer one engine or all.
//! - **Profiles.** A [`Profile`] has its own cookies and site data in every
//!   engine. [`ProfileManager`] creates, lists, renames and deletes them.
//! - **History and bookmarks.** Stored by the library, per profile, in one
//!   format whatever engine rendered the page. See [`History`] and
//!   [`Bookmarks`].
//! - **Import and export.** [`import`] reads history and bookmarks from the
//!   browsers installed on the machine; [`History::export`] writes JSON, CSV
//!   or HTML.
//!
//! # Embedding a browser
//!
//! A [`Browser`] is a child view of a window the application already has. The
//! window can come from any toolkit that implements
//! [`raw_window_handle::HasWindowHandle`].
//!
//! ```no_run
//! # fn demo(window: &impl raw_window_handle::HasWindowHandle) -> polybrowser::Result<()> {
//! use polybrowser::{Browser, Engine, ProfileManager, Rect};
//!
//! let profiles = ProfileManager::open("com.example.app")?;
//! let profile = profiles.get_or_create("Default")?;
//!
//! let browser = Browser::builder()
//!     .engine(Engine::WebKit)
//!     .profile(&profile)
//!     .url("https://www.rust-lang.org")
//!     .build(window, Rect::new(0.0, 0.0, 1024.0, 768.0))?;
//! # Ok(())
//! # }
//! ```
//!
//! Call [`init`] first thing in `main`, and [`shutdown`] before leaving it.
//! Both do nothing for the system web views, but Chromium needs them, and
//! having them in place means switching engines later is a feature flag.
//!
//! # Cargo features
//!
//! | Feature    | Engine                                   | Default |
//! |------------|------------------------------------------|---------|
//! | `webkit`   | [`Engine::WebKit`] on macOS and Linux    | yes     |
//! | `webview2` | [`Engine::WebView2`] on Windows          | yes     |
//! | `chromium` | [`Engine::Chromium`], macOS only so far  | no      |
//!
//! With `default-features = false` and no engine, the crate is just the data
//! layer: profiles, history, bookmarks, import and export.

mod browser;
mod engines;

pub use browser::{Browser, BrowserBuilder};
pub use engines::{default_engine, engines};
pub use polybrowser_core::{
    backend, import, is_recordable_url, Bookmark, BookmarkNode, Bookmarks, BrowserEvent, Engine,
    Error, ExternalBrowser, ExternalProfile, History, HistoryEntry, HistoryFormat, HistoryQuery,
    ImportOptions, ImportReport, NewWindowPolicy, Profile, ProfileId, ProfileManager, Rect, Result,
    Timestamp, Visit,
};
pub use raw_window_handle;

/// Prepares the engines. Call this first thing in `main`, before reading
/// arguments or creating windows.
///
/// Chromium runs its renderers in child processes, and on Windows and Linux
/// those are started from the application's own executable. In such a child
/// this function does the child's work and exits the process when it is done;
/// in the application itself it returns immediately.
pub fn init() {
    #[cfg(chromium_engine)]
    polybrowser_cef::init();
}

/// Lets engines with their own message loop do pending work.
///
/// Only Chromium on Linux and Windows needs this: call it from the
/// application's event loop, about once per iteration. Everywhere else the
/// engines hook into the platform's event loop themselves and this does
/// nothing.
pub fn pump() {
    #[cfg(chromium_engine)]
    polybrowser_cef::pump();
}

/// Shuts the engines down. Call this on the main thread after the last
/// [`Browser`] is dropped, before `main` returns.
///
/// Chromium writes cookies and site data lazily; without a clean shutdown the
/// last few seconds of them can be lost.
pub fn shutdown() {
    #[cfg(chromium_engine)]
    polybrowser_cef::shutdown();
}
