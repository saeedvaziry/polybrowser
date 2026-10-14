/// Something that happened in a browser view.
///
/// Events are delivered on the thread that owns the view (the main thread).
///
/// When the main frame moves to a new document, [`LoadStarted`] comes before
/// the [`UrlChanged`] for its address, and the document's [`TitleChanged`]
/// after both. A page that only changes its address (a single-page
/// application) produces [`UrlChanged`] alone.
///
/// [`LoadStarted`]: BrowserEvent::LoadStarted
/// [`UrlChanged`]: BrowserEvent::UrlChanged
/// [`TitleChanged`]: BrowserEvent::TitleChanged
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum BrowserEvent {
    /// The main frame began loading a new document.
    LoadStarted { url: String },
    /// The main frame finished loading.
    LoadFinished { url: String },
    /// The main frame failed to load. Only the Chromium engine reports this.
    LoadFailed { url: String, error: String },
    /// The address of the main frame changed. Unlike [`LoadStarted`], this
    /// also covers same-document navigations (`history.pushState`, fragment
    /// changes), which is what single-page applications do.
    ///
    /// [`LoadStarted`]: BrowserEvent::LoadStarted
    UrlChanged { url: String },
    /// The document title changed.
    TitleChanged { title: String },
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
