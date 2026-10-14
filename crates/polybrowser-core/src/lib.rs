//! Engine-independent half of `polybrowser`.
//!
//! This crate has no web engine in it. It defines the types every engine
//! backend shares ([`Engine`], [`Rect`], [`BrowserEvent`], the [`backend`]
//! traits) and owns the data that should outlive any particular engine:
//! [profiles](ProfileManager), [browsing history](History),
//! [bookmarks](Bookmarks), [importing](import) from installed browsers and
//! exporting.
//!
//! Most applications depend on the `polybrowser` crate instead, which
//! re-exports everything here.

pub mod backend;
mod bookmarks;
mod engine;
mod error;
mod event;
mod export;
mod geometry;
mod history;
pub mod import;
mod profile;
mod store;
mod time;

pub use bookmarks::{Bookmark, BookmarkNode, Bookmarks};
pub use engine::Engine;
pub use error::{Error, Result};
pub use event::{BrowserEvent, NewWindowPolicy};
pub use export::HistoryFormat;
pub use geometry::Rect;
pub use history::{is_recordable_url, History, HistoryEntry, HistoryQuery, Visit};
pub use import::{ExternalBrowser, ExternalProfile, ImportOptions, ImportReport};
pub use profile::{Profile, ProfileId, ProfileManager};
pub use time::Timestamp;
