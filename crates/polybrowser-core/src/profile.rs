use std::{
    fmt, fs, io,
    path::{Path, PathBuf},
    str::FromStr,
    sync::{Arc, OnceLock},
};

use serde::{Deserialize, Serialize};

use crate::{
    backend, import, store::Store, Bookmarks, Engine, Error, ExternalProfile, History,
    ImportOptions, ImportReport, Result, Timestamp,
};

const PROFILES_DIR: &str = "profiles";
const ENGINES_DIR: &str = "engines";
const META_FILE: &str = "profile.json";
const LIBRARY_FILE: &str = "library.sqlite";

/// Identifies a profile. Unlike its name, it never changes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ProfileId([u8; 16]);

impl ProfileId {
    fn random() -> Self {
        ProfileId(uuid::Uuid::new_v4().into_bytes())
    }

    pub const fn from_bytes(bytes: [u8; 16]) -> Self {
        ProfileId(bytes)
    }

    pub const fn as_bytes(&self) -> &[u8; 16] {
        &self.0
    }
}

impl fmt::Display for ProfileId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        uuid::Uuid::from_bytes(self.0).hyphenated().fmt(f)
    }
}

impl FromStr for ProfileId {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self> {
        uuid::Uuid::parse_str(s)
            .map(|uuid| ProfileId(uuid.into_bytes()))
            .map_err(|_| Error::ProfileNotFound(s.to_owned()))
    }
}

#[derive(Serialize, Deserialize)]
struct Meta {
    id: String,
    name: String,
    created_at: i64,
}

/// A browsing identity: its own cookies and site data in every engine, plus
/// history and bookmarks that are shared between engines.
///
/// Cheap to clone. Get persistent profiles from a [`ProfileManager`], or use
/// [`Profile::ephemeral`] for private browsing.
#[derive(Clone)]
pub struct Profile {
    inner: Arc<Inner>,
}

struct Inner {
    id: ProfileId,
    name: String,
    created_at: Timestamp,
    /// The profile's own directory; `None` for ephemeral profiles.
    dir: Option<PathBuf>,
    /// The manager's root; `None` for a profile that belongs to no manager.
    root: Option<PathBuf>,
    store: OnceLock<Arc<Store>>,
}

impl Profile {
    fn new(
        meta_id: ProfileId,
        name: String,
        created_at: Timestamp,
        dir: Option<PathBuf>,
        root: Option<PathBuf>,
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                id: meta_id,
                name,
                created_at,
                dir,
                root,
                store: OnceLock::new(),
            }),
        }
    }

    /// A profile that leaves nothing on disk. Site data is dropped with the
    /// views using it; history and bookmarks live in memory until the last
    /// clone of the profile goes away.
    pub fn ephemeral() -> Self {
        Self::new(
            ProfileId::random(),
            "Private".to_owned(),
            Timestamp::now(),
            None,
            None,
        )
    }

    pub fn id(&self) -> ProfileId {
        self.inner.id
    }

    pub fn name(&self) -> &str {
        &self.inner.name
    }

    pub fn created_at(&self) -> Timestamp {
        self.inner.created_at
    }

    pub fn is_ephemeral(&self) -> bool {
        self.inner.dir.is_none()
    }

    /// Directory holding the profile's metadata, history and bookmarks.
    pub fn dir(&self) -> Option<&Path> {
        self.inner.dir.as_deref()
    }

    /// Directory in which `engine` keeps the data of all profiles managed
    /// together with this one.
    pub fn engine_root(&self, engine: Engine) -> Option<PathBuf> {
        Some(
            self.inner
                .root
                .as_ref()?
                .join(ENGINES_DIR)
                .join(engine.id()),
        )
    }

    /// Directory in which `engine` keeps this profile's cookies, cache and
    /// site storage. `None` for ephemeral profiles.
    pub fn engine_data_dir(&self, engine: Engine) -> Option<PathBuf> {
        self.inner.dir.as_ref()?;
        Some(self.engine_root(engine)?.join(self.inner.id.to_string()))
    }

    pub fn history(&self) -> Result<History> {
        Ok(History::new(self.store()?))
    }

    pub fn bookmarks(&self) -> Result<Bookmarks> {
        Ok(Bookmarks::new(self.store()?))
    }

    /// Copies history and bookmarks from a profile of another browser into
    /// this one. Safe to repeat: visits already imported are skipped and the
    /// imported bookmark folder is replaced.
    ///
    /// Cookies, saved passwords and open tabs are not imported.
    pub fn import_from(
        &self,
        source: &ExternalProfile,
        options: &ImportOptions,
    ) -> Result<ImportReport> {
        import::run(self, source, options)
    }

    fn store(&self) -> Result<Arc<Store>> {
        if let Some(store) = self.inner.store.get() {
            return Ok(store.clone());
        }
        let store = Arc::new(match &self.inner.dir {
            Some(dir) => Store::open(&dir.join(LIBRARY_FILE))?,
            None => Store::in_memory()?,
        });
        Ok(self.inner.store.get_or_init(|| store).clone())
    }
}

impl fmt::Debug for Profile {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Profile")
            .field("id", &self.inner.id.to_string())
            .field("name", &self.inner.name)
            .field("ephemeral", &self.is_ephemeral())
            .finish()
    }
}

/// Creates, lists and deletes the profiles of an application.
///
/// Everything lives under one root directory:
///
/// ```text
/// <root>/profiles/<profile id>/profile.json      name and creation time
/// <root>/profiles/<profile id>/library.sqlite    history and bookmarks
/// <root>/engines/<engine>/<profile id>/          that engine's site data
/// ```
#[derive(Debug, Clone)]
pub struct ProfileManager {
    root: PathBuf,
}

impl ProfileManager {
    /// Opens the profiles of the application called `app_id`, in the
    /// platform's per-user data directory (for example
    /// `~/Library/Application Support/<app_id>/polybrowser` on macOS).
    ///
    /// `app_id` becomes a directory name; a reverse-DNS identifier such as
    /// `com.example.myapp` works well.
    pub fn open(app_id: &str) -> Result<Self> {
        let is_plain_name = !app_id.is_empty()
            && app_id != "."
            && app_id != ".."
            && !app_id.contains(['/', '\\', '\0']);
        if !is_plain_name {
            return Err(Error::InvalidProfileName(
                "the application id must be a plain directory name",
            ));
        }
        let base = dirs::data_local_dir().ok_or(Error::NoDataDirectory)?;
        Self::open_at(base.join(app_id).join("polybrowser"))
    }

    /// Opens the profiles stored under `root`, creating it if needed.
    pub fn open_at(root: impl Into<PathBuf>) -> Result<Self> {
        let root = root.into();
        fs::create_dir_all(root.join(PROFILES_DIR))?;
        // Engines compare and nest paths, so hand out an absolute one.
        let root = fs::canonicalize(&root)?;
        Ok(Self { root })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// All profiles, oldest first.
    pub fn list(&self) -> Result<Vec<Profile>> {
        let mut profiles = Vec::new();
        for entry in fs::read_dir(self.root.join(PROFILES_DIR))? {
            let dir = entry?.path();
            // Anything without readable metadata is not a profile of ours.
            if let Some(profile) = self.load(&dir) {
                profiles.push(profile);
            }
        }
        profiles.sort_by(|a, b| (a.created_at(), a.name()).cmp(&(b.created_at(), b.name())));
        Ok(profiles)
    }

    /// Creates a profile. Names are unique, ignoring case.
    pub fn create(&self, name: &str) -> Result<Profile> {
        let name = validate_name(name)?;
        if self.find(name)?.is_some() {
            return Err(Error::ProfileExists(name.to_owned()));
        }
        let id = ProfileId::random();
        let created_at = Timestamp::now();
        let dir = self.profile_dir(id);
        fs::create_dir_all(&dir)?;
        write_meta(&dir, id, name, created_at)?;
        Ok(Profile::new(
            id,
            name.to_owned(),
            created_at,
            Some(dir),
            Some(self.root.clone()),
        ))
    }

    /// Looks a profile up by id.
    pub fn get(&self, id: ProfileId) -> Result<Profile> {
        self.load(&self.profile_dir(id))
            .ok_or_else(|| Error::ProfileNotFound(id.to_string()))
    }

    /// Looks a profile up by name, ignoring case.
    pub fn find(&self, name: &str) -> Result<Option<Profile>> {
        let wanted = name.trim().to_lowercase();
        Ok(self
            .list()?
            .into_iter()
            .find(|profile| profile.name().to_lowercase() == wanted))
    }

    /// The profile called `name`, created if it does not exist yet.
    pub fn get_or_create(&self, name: &str) -> Result<Profile> {
        match self.find(name)? {
            Some(profile) => Ok(profile),
            None => self.create(name),
        }
    }

    /// Renames a profile. Existing [`Profile`] values keep the old name.
    pub fn rename(&self, id: ProfileId, new_name: &str) -> Result<Profile> {
        let new_name = validate_name(new_name)?;
        let profile = self.get(id)?;
        if let Some(other) = self.find(new_name)? {
            if other.id() != id {
                return Err(Error::ProfileExists(new_name.to_owned()));
            }
        }
        write_meta(&self.profile_dir(id), id, new_name, profile.created_at())?;
        self.get(id)
    }

    /// Deletes a profile with its history, bookmarks and site data.
    ///
    /// Close the browser views using the profile first. Data that an engine
    /// keeps outside the directories above (WebKit on macOS does) is erased
    /// the next time that engine creates a view.
    pub fn delete(&self, id: ProfileId) -> Result<()> {
        let dir = self.profile_dir(id);
        if !dir.join(META_FILE).is_file() {
            return Err(Error::ProfileNotFound(id.to_string()));
        }
        for engine in Engine::ALL {
            let engine_root = self.root.join(ENGINES_DIR).join(engine.id());
            let data_dir = engine_root.join(id.to_string());
            if !data_dir.exists() {
                continue;
            }
            if keeps_data_outside_data_dir(engine) {
                backend::enqueue_purge(&engine_root, id)?;
            }
            fs::remove_dir_all(&data_dir)?;
        }
        fs::remove_dir_all(&dir)?;
        Ok(())
    }

    /// A private profile. Like [`Profile::ephemeral`], but engines that need
    /// one data root per process (Chromium) will share this manager's root,
    /// so it can be mixed with the manager's persistent profiles.
    pub fn ephemeral(&self) -> Profile {
        Profile::new(
            ProfileId::random(),
            "Private".to_owned(),
            Timestamp::now(),
            None,
            Some(self.root.clone()),
        )
    }

    fn profile_dir(&self, id: ProfileId) -> PathBuf {
        self.root.join(PROFILES_DIR).join(id.to_string())
    }

    fn load(&self, dir: &Path) -> Option<Profile> {
        let meta: Meta = serde_json::from_slice(&fs::read(dir.join(META_FILE)).ok()?).ok()?;
        let id: ProfileId = meta.id.parse().ok()?;
        // The directory name is what every other path is derived from.
        if dir.file_name()?.to_str()? != id.to_string() {
            return None;
        }
        Some(Profile::new(
            id,
            meta.name,
            Timestamp::from_unix_micros(meta.created_at),
            Some(dir.to_owned()),
            Some(self.root.clone()),
        ))
    }
}

/// `WKWebView` cannot be pointed at a directory; it keys its data stores by
/// identifier and keeps them under `~/Library/WebKit`.
fn keeps_data_outside_data_dir(engine: Engine) -> bool {
    cfg!(target_os = "macos") && engine == Engine::WebKit
}

fn validate_name(name: &str) -> Result<&str> {
    let name = name.trim();
    if name.is_empty() {
        return Err(Error::InvalidProfileName("the name is empty"));
    }
    if name.chars().count() > 128 {
        return Err(Error::InvalidProfileName(
            "the name is longer than 128 characters",
        ));
    }
    if name.chars().any(char::is_control) {
        return Err(Error::InvalidProfileName(
            "the name contains control characters",
        ));
    }
    Ok(name)
}

fn write_meta(dir: &Path, id: ProfileId, name: &str, created_at: Timestamp) -> io::Result<()> {
    let meta = Meta {
        id: id.to_string(),
        name: name.to_owned(),
        created_at: created_at.as_unix_micros(),
    };
    // Write then rename, so a crash cannot leave half a file behind.
    let temporary = dir.join(format!("{META_FILE}.tmp"));
    fs::write(&temporary, serde_json::to_vec_pretty(&meta)?)?;
    fs::rename(temporary, dir.join(META_FILE))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{HistoryQuery, Visit};

    fn manager() -> (tempfile::TempDir, ProfileManager) {
        let dir = tempfile::tempdir().unwrap();
        let manager = ProfileManager::open_at(dir.path().join("data")).unwrap();
        (dir, manager)
    }

    #[test]
    fn creates_lists_and_finds_profiles() {
        let (_dir, manager) = manager();
        let work = manager.create("Work").unwrap();
        let home = manager.create("  Home ").unwrap();
        assert_eq!(home.name(), "Home");
        assert!(!work.is_ephemeral());

        let names: Vec<_> = manager
            .list()
            .unwrap()
            .iter()
            .map(|p| p.name().to_owned())
            .collect();
        assert_eq!(names.len(), 2);
        assert!(names.contains(&"Work".to_owned()) && names.contains(&"Home".to_owned()));

        assert_eq!(manager.find("work").unwrap().unwrap().id(), work.id());
        assert_eq!(manager.get(work.id()).unwrap().name(), "Work");
        assert_eq!(manager.get_or_create("WORK").unwrap().id(), work.id());
        assert!(matches!(
            manager.create("work"),
            Err(Error::ProfileExists(_))
        ));
        assert!(matches!(
            manager.create("  "),
            Err(Error::InvalidProfileName(_))
        ));
    }

    #[test]
    fn history_persists_across_handles() {
        let (_dir, manager) = manager();
        let profile = manager.create("Main").unwrap();
        profile
            .history()
            .unwrap()
            .add_visit(&Visit::new(
                "https://example.com/",
                Some("Example".into()),
                Timestamp::from_unix_secs(10),
            ))
            .unwrap();
        drop(profile);

        let reopened = manager.find("Main").unwrap().unwrap();
        let entries = reopened
            .history()
            .unwrap()
            .entries(&HistoryQuery::new())
            .unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].title.as_deref(), Some("Example"));
    }

    #[test]
    fn renames_without_changing_identity() {
        let (_dir, manager) = manager();
        let profile = manager.create("Old").unwrap();
        manager.create("Taken").unwrap();
        assert!(matches!(
            manager.rename(profile.id(), "taken"),
            Err(Error::ProfileExists(_))
        ));
        let renamed = manager.rename(profile.id(), "New").unwrap();
        assert_eq!(renamed.id(), profile.id());
        assert_eq!(renamed.name(), "New");
        assert_eq!(renamed.dir(), profile.dir());
        // Changing only the case of a profile's own name is allowed.
        assert_eq!(manager.rename(profile.id(), "NEW").unwrap().name(), "NEW");
    }

    #[test]
    fn delete_removes_profile_and_engine_data() {
        let (_dir, manager) = manager();
        let profile = manager.create("Doomed").unwrap();
        profile
            .history()
            .unwrap()
            .record_visit("https://example.com/", None)
            .unwrap();
        let chromium = profile.engine_data_dir(Engine::Chromium).unwrap();
        let webkit = profile.engine_data_dir(Engine::WebKit).unwrap();
        fs::create_dir_all(&chromium).unwrap();
        fs::create_dir_all(&webkit).unwrap();
        let id = profile.id();
        let dir = profile.dir().unwrap().to_owned();
        drop(profile);

        manager.delete(id).unwrap();
        assert!(!dir.exists() && !chromium.exists() && !webkit.exists());
        assert!(manager.list().unwrap().is_empty());
        assert!(matches!(manager.delete(id), Err(Error::ProfileNotFound(_))));

        // WebKit on macOS holds site data elsewhere, so it has to be told.
        let webkit_root = manager.root().join(ENGINES_DIR).join("webkit");
        let queued = backend::pending_purges(&webkit_root);
        assert_eq!(queued.contains(&id), cfg!(target_os = "macos"));
        backend::purge_finished(&webkit_root, id).unwrap();
        assert!(backend::pending_purges(&webkit_root).is_empty());
        let chromium_root = manager.root().join(ENGINES_DIR).join("chromium");
        assert!(backend::pending_purges(&chromium_root).is_empty());
    }

    #[test]
    fn ephemeral_profiles_touch_no_files() {
        let (_dir, manager) = manager();
        let private = manager.ephemeral();
        assert!(private.is_ephemeral());
        assert!(private.dir().is_none());
        assert!(private.engine_data_dir(Engine::Chromium).is_none());
        assert!(private.engine_root(Engine::Chromium).is_some());
        assert!(Profile::ephemeral().engine_root(Engine::Chromium).is_none());

        private
            .history()
            .unwrap()
            .record_visit("https://example.com/", None)
            .unwrap();
        assert_eq!(private.clone().history().unwrap().page_count().unwrap(), 1);
        assert!(manager.list().unwrap().is_empty());
    }

    #[test]
    fn rejects_app_ids_that_are_paths() {
        for bad in ["", ".", "..", "a/b", "a\\b"] {
            assert!(ProfileManager::open(bad).is_err(), "{bad:?}");
        }
    }
}
