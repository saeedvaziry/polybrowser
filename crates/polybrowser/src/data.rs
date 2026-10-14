//! Working with a profile's engine site data without opening a view.

use polybrowser_core::{Cookie, Engine, Error, Profile, Result};

/// Deletes the cookies, caches and storage of `profile` in `engine`, without a
/// view. An ephemeral profile is a no-op.
///
/// The callback runs on the main thread, asynchronously. Call this on the main
/// thread (otherwise [`Error::NotMainThread`]). Only the WebKit engine on macOS
/// is supported; other engines report [`Error::Unsupported`].
pub fn clear_site_data(
    engine: Engine,
    profile: &Profile,
    done: impl FnOnce(Result<()>) + 'static,
) -> Result<()> {
    if !crate::engines().contains(&engine) {
        return Err(Error::EngineUnavailable(engine));
    }
    #[cfg(system_engine)]
    if engine == polybrowser_wry::ENGINE {
        return polybrowser_wry::clear_site_data(profile, Box::new(done));
    }
    let _ = (profile, done);
    Err(Error::unsupported("clear_site_data"))
}

/// Writes `cookies` into the store of `profile` in `engine`, without a view,
/// and reports how many were written. An ephemeral profile writes nothing.
///
/// Same threading and engine support as [`clear_site_data`].
pub fn import_cookies(
    engine: Engine,
    profile: &Profile,
    cookies: Vec<Cookie>,
    done: impl FnOnce(Result<usize>) + 'static,
) -> Result<()> {
    if !crate::engines().contains(&engine) {
        return Err(Error::EngineUnavailable(engine));
    }
    #[cfg(system_engine)]
    if engine == polybrowser_wry::ENGINE {
        return polybrowser_wry::import_cookies(profile, cookies, Box::new(done));
    }
    let _ = (profile, cookies, done);
    Err(Error::unsupported("import_cookies"))
}
