//! Which engines this build contains.

use polybrowser_core::{backend::EngineBackend, Engine, Error, Result};

/// The engines usable in this build: enabled through Cargo features and
/// supported on the platform. The system's own engine comes first.
pub fn engines() -> Vec<Engine> {
    vec![
        #[cfg(system_engine)]
        polybrowser_wry::ENGINE,
        #[cfg(chromium_engine)]
        Engine::Chromium,
    ]
}

/// The engine a [`Browser`](crate::Browser) uses when none is asked for: the
/// first of [`engines`].
pub fn default_engine() -> Option<Engine> {
    engines().into_iter().next()
}

pub(crate) fn backend(engine: Engine) -> Result<Box<dyn EngineBackend>> {
    #[cfg(system_engine)]
    {
        if engine == polybrowser_wry::ENGINE {
            return Ok(Box::new(polybrowser_wry::WryBackend::new()));
        }
    }
    #[cfg(chromium_engine)]
    {
        if engine == Engine::Chromium {
            return Ok(Box::new(polybrowser_cef::CefBackend::new()));
        }
    }
    Err(Error::EngineUnavailable(engine))
}
