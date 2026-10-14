/// Something that happened in a browser view.
///
/// Events are delivered on the thread that owns the view (the main thread).
///
/// When the main frame moves to a new document, [`LoadStarted`] comes before
/// the [`UrlChanged`] for its address, and the document's [`TitleChanged`]
/// after both. A page that only changes its address (a single-page
/// application) produces [`UrlChanged`] alone. [`LoadFinished`] or
/// [`LoadFailed`] ends a load; [`FaviconChanged`] follows a finished main-frame
/// load. [`ProgressChanged`] and [`NavigationStateChanged`] may arrive at any
/// time while a load runs.
///
/// [`LoadStarted`]: BrowserEvent::LoadStarted
/// [`UrlChanged`]: BrowserEvent::UrlChanged
/// [`TitleChanged`]: BrowserEvent::TitleChanged
/// [`LoadFinished`]: BrowserEvent::LoadFinished
/// [`LoadFailed`]: BrowserEvent::LoadFailed
/// [`FaviconChanged`]: BrowserEvent::FaviconChanged
/// [`ProgressChanged`]: BrowserEvent::ProgressChanged
/// [`NavigationStateChanged`]: BrowserEvent::NavigationStateChanged
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum BrowserEvent {
    /// The main frame began loading a new document.
    LoadStarted { url: String },
    /// The main frame finished loading.
    LoadFinished { url: String },
    /// The main frame failed to load, for a provisional or committed
    /// navigation. Cancellations and loads stopped by a navigation policy are
    /// not failures and are not reported.
    LoadFailed { url: String, error: String },
    /// The address of the main frame changed. Unlike [`LoadStarted`], this
    /// also covers same-document navigations (`history.pushState`, fragment
    /// changes), which is what single-page applications do.
    ///
    /// [`LoadStarted`]: BrowserEvent::LoadStarted
    UrlChanged { url: String },
    /// The document title changed.
    TitleChanged { title: String },
    /// The load progress of the main frame, from `0.0` to `1.0`.
    ProgressChanged { progress: f64 },
    /// Whether the view can go back or forward changed. Emitted whenever
    /// either value changes.
    NavigationStateChanged {
        can_go_back: bool,
        can_go_forward: bool,
    },
    /// The page's favicon, as an absolute `http`/`https` URL: the last
    /// `link[rel~="icon"]` of the document, or `origin + "/favicon.ico"` when
    /// it declares none. The same URL is not reported twice in a row for one
    /// document.
    FaviconChanged { url: String },
    /// The page asked for its window to be closed (`window.close()` in the
    /// main frame; frames inside the page cannot). The view is not closed
    /// automatically; the application decides what to do.
    CloseRequested,
    /// The user activated a link whose scheme the engine cannot display
    /// (anything other than `http`, `https`, `about`, `data`, `blob`, `file`
    /// and `javascript`), such as `mailto:` or `tel:`. The engine does not
    /// navigate; the application can hand the URL to the operating system.
    /// Only user activations produce this, so a page cannot launch another
    /// application on its own.
    ExternalLinkRequested { url: String },
    /// The page asked for a new window (`window.open`, `target="_blank"`).
    /// What happens next is decided by the view's [`NewWindowPolicy`].
    NewWindowRequested { url: String },
}

/// What a view does when its page asks to open a new window.
///
/// [`BrowserEvent::NewWindowRequested`] is emitted either way.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum NewWindowPolicy {
    /// Load the requested URL in the same view. This keeps links working in
    /// an application that shows a single view.
    #[default]
    NavigateCurrent,
    /// Do nothing. Use this when the application opens its own tab or window
    /// in response to the event.
    Block,
}
