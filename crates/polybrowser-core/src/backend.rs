//! The interface between `polybrowser` and a web engine.
//!
//! Applications do not use this module; it is for crates that implement an
//! engine. A backend turns a [`ViewConfig`] into a native view parented to a
//! window, reports what happens in it through an [`EventSink`], and is driven
//! through [`ViewBackend`].

use std::{
    fs, io,
    path::{Path, PathBuf},
    rc::Rc,
};

use raw_window_handle::WindowHandle;

use crate::{BrowserEvent, Engine, NewWindowPolicy, ProfileId, Rect, Result};

/// Receives events from a view. Always called on the main thread.
pub type EventSink = Rc<dyn Fn(BrowserEvent)>;

/// Decides whether the main frame may navigate to a URL.
pub type NavigationFilter = Rc<dyn Fn(&str) -> bool>;

/// Where a view keeps cookies, caches and other site data.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProfileStorage {
    /// Nothing survives the view.
    Ephemeral,
    /// Site data persists under `data_dir`, or, for engines that cannot be
    /// told where to store data, in an engine-managed store keyed by `id`.
    Persistent { id: ProfileId, data_dir: PathBuf },
}

/// Everything a backend needs to create a view.
pub struct ViewConfig {
    pub url: Option<String>,
    pub html: Option<String>,
    pub bounds: Rect,
    pub visible: bool,
    pub user_agent: Option<String>,
    pub devtools: bool,
    pub storage: ProfileStorage,
    /// Directory holding this engine's data for every profile of the
    /// application, when the profile came from a
    /// [`ProfileManager`](crate::ProfileManager).
    pub engine_root: Option<PathBuf>,
    pub new_window_policy: NewWindowPolicy,
    pub navigation_filter: Option<NavigationFilter>,
}

/// A web engine that can create views.
pub trait EngineBackend {
    fn engine(&self) -> Engine;

    /// Creates a view as a child of `parent`. Called on the main thread.
    fn create_view(
        &self,
        parent: WindowHandle<'_>,
        config: ViewConfig,
        events: EventSink,
    ) -> Result<Box<dyn ViewBackend>>;
}

/// A live view. Dropping it removes the view from its window.
pub trait ViewBackend {
    fn load_url(&self, url: &str) -> Result<()>;
    fn load_html(&self, html: &str) -> Result<()>;
    fn url(&self) -> Result<String>;
    fn reload(&self) -> Result<()>;
    fn stop(&self) -> Result<()>;
    fn go_back(&self) -> Result<()>;
    fn go_forward(&self) -> Result<()>;
    fn can_go_back(&self) -> Result<bool>;
    fn can_go_forward(&self) -> Result<bool>;
    fn evaluate_script(&self, script: &str) -> Result<()>;
    fn set_bounds(&self, bounds: Rect) -> Result<()>;
    fn set_visible(&self, visible: bool) -> Result<()>;
    fn focus(&self) -> Result<()>;
    /// `1.0` is the natural size.
    fn set_zoom(&self, factor: f64) -> Result<()>;
    fn open_devtools(&self) -> Result<()>;
    fn close_devtools(&self) -> Result<()>;
    fn is_devtools_open(&self) -> bool;
    /// Deletes cookies, caches and site storage of the view's profile.
    fn clear_browsing_data(&self) -> Result<()>;
}

const PURGE_QUEUE_FILE: &str = "purge-queue.json";

/// Profiles that were deleted while the engine still held data for them
/// outside the profile directories (see [`ProfileStorage::Persistent`]).
/// The backend should erase that data and then call [`purge_finished`].
pub fn pending_purges(engine_root: &Path) -> Vec<ProfileId> {
    fs::read(engine_root.join(PURGE_QUEUE_FILE))
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Vec<String>>(&bytes).ok())
        .unwrap_or_default()
        .iter()
        .filter_map(|id| id.parse().ok())
        .collect()
}

/// Removes a profile from the queue returned by [`pending_purges`].
pub fn purge_finished(engine_root: &Path, id: ProfileId) -> io::Result<()> {
    let mut queue = pending_purges(engine_root);
    queue.retain(|queued| *queued != id);
    write_purge_queue(engine_root, &queue)
}

pub(crate) fn enqueue_purge(engine_root: &Path, id: ProfileId) -> io::Result<()> {
    let mut queue = pending_purges(engine_root);
    if !queue.contains(&id) {
        queue.push(id);
    }
    write_purge_queue(engine_root, &queue)
}

fn write_purge_queue(engine_root: &Path, queue: &[ProfileId]) -> io::Result<()> {
    let path = engine_root.join(PURGE_QUEUE_FILE);
    if queue.is_empty() {
        return match fs::remove_file(path) {
            Err(error) if error.kind() != io::ErrorKind::NotFound => Err(error),
            _ => Ok(()),
        };
    }
    let ids: Vec<String> = queue.iter().map(ProfileId::to_string).collect();
    fs::create_dir_all(engine_root)?;
    fs::write(path, serde_json::to_vec(&ids)?)
}
