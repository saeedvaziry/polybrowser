//! View-level value types shared between the facade and the engine backends.

/// A bitmap captured from a view by [`snapshot`].
///
/// Pixels are straight (non-premultiplied) RGBA8, row-major, in physical
/// pixels at the window's backing scale, so `rgba.len() == width * height * 4`.
///
/// [`snapshot`]: crate::backend::ViewBackend::snapshot
#[derive(Clone, PartialEq, Eq)]
pub struct Image {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

impl std::fmt::Debug for Image {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Image")
            .field("width", &self.width)
            .field("height", &self.height)
            .field("rgba", &format_args!("[{} bytes]", self.rgba.len()))
            .finish()
    }
}

/// How [`find`](crate::backend::ViewBackend::find) looks for text in a page.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FindOptions {
    /// Search towards the start of the document instead of the end.
    pub backwards: bool,
    /// Match letter case exactly.
    pub case_sensitive: bool,
    /// Continue from the other end when the end is reached.
    pub wraps: bool,
}

impl Default for FindOptions {
    fn default() -> Self {
        Self {
            backwards: false,
            case_sensitive: false,
            wraps: true,
        }
    }
}
