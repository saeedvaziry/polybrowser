use std::{fmt, str::FromStr};

/// A web engine a browser view can be rendered with.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Engine {
    /// WebKit as shipped by the operating system: `WKWebView` on macOS and
    /// WebKitGTK on Linux. Adds nothing to the size of the application.
    WebKit,
    /// Microsoft Edge WebView2 on Windows: Chromium-based and provided by the
    /// operating system.
    WebView2,
    /// Chromium, through the Chromium Embedded Framework. Behaves the same on
    /// every desktop platform, at the cost of shipping the engine with the
    /// application.
    Chromium,
}

impl Engine {
    /// Every engine this library knows about, whether or not it is compiled in.
    pub const ALL: [Engine; 3] = [Engine::WebKit, Engine::WebView2, Engine::Chromium];

    /// A stable lowercase identifier, also used for directory names.
    pub const fn id(self) -> &'static str {
        match self {
            Engine::WebKit => "webkit",
            Engine::WebView2 => "webview2",
            Engine::Chromium => "chromium",
        }
    }

    /// Whether the engine exists for the operating system being compiled for.
    ///
    /// This says nothing about Cargo features; `polybrowser::engines()` lists
    /// the engines that are both supported here and compiled in.
    pub const fn supports_current_platform(self) -> bool {
        match self {
            Engine::WebKit => cfg!(any(
                target_os = "macos",
                target_os = "linux",
                target_os = "dragonfly",
                target_os = "freebsd",
                target_os = "netbsd",
                target_os = "openbsd"
            )),
            Engine::WebView2 => cfg!(target_os = "windows"),
            Engine::Chromium => cfg!(any(
                target_os = "macos",
                target_os = "linux",
                target_os = "windows"
            )),
        }
    }
}

impl fmt::Display for Engine {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Engine::WebKit => "WebKit",
            Engine::WebView2 => "WebView2",
            Engine::Chromium => "Chromium",
        })
    }
}

impl FromStr for Engine {
    type Err = crate::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Engine::ALL
            .into_iter()
            .find(|engine| engine.id().eq_ignore_ascii_case(s.trim()))
            .ok_or_else(|| crate::Error::UnknownEngine(s.to_owned()))
    }
}
