//! Reading history and bookmarks out of browsers installed on this machine.
//!
//! ```no_run
//! # fn main() -> polybrowser_core::Result<()> {
//! use polybrowser_core::{import, ImportOptions, ProfileManager};
//!
//! let profile = ProfileManager::open("com.example.app")?.get_or_create("Default")?;
//! for source in import::detect() {
//!     let report = profile.import_from(&source, &ImportOptions::default())?;
//!     println!("{} ({}): {} visits", source.browser(), source.name(), report.visits_added);
//! }
//! # Ok(())
//! # }
//! ```
//!
//! Browsers are only ever read from, and through a temporary copy of their
//! databases, so importing works while the browser is running and cannot
//! disturb it.

use std::{
    fmt, fs, io,
    path::{Path, PathBuf},
};

use rusqlite::Connection;

use crate::{BookmarkNode, Cookie, Error, Profile, Result, Timestamp, Visit};

mod chromium;
#[cfg(target_os = "macos")]
mod cookie_crypto;
mod firefox;
mod safari;

/// A browser whose data can be imported.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ExternalBrowser {
    Chrome,
    Chromium,
    Edge,
    Brave,
    Vivaldi,
    Opera,
    Arc,
    Firefox,
    Safari,
}

impl ExternalBrowser {
    pub const ALL: [ExternalBrowser; 9] = [
        ExternalBrowser::Chrome,
        ExternalBrowser::Chromium,
        ExternalBrowser::Edge,
        ExternalBrowser::Brave,
        ExternalBrowser::Vivaldi,
        ExternalBrowser::Opera,
        ExternalBrowser::Arc,
        ExternalBrowser::Firefox,
        ExternalBrowser::Safari,
    ];

    /// A stable lowercase identifier. Imported visits carry it as their
    /// [`source`](Visit::source).
    pub const fn id(self) -> &'static str {
        match self {
            ExternalBrowser::Chrome => "chrome",
            ExternalBrowser::Chromium => "chromium",
            ExternalBrowser::Edge => "edge",
            ExternalBrowser::Brave => "brave",
            ExternalBrowser::Vivaldi => "vivaldi",
            ExternalBrowser::Opera => "opera",
            ExternalBrowser::Arc => "arc",
            ExternalBrowser::Firefox => "firefox",
            ExternalBrowser::Safari => "safari",
        }
    }

    /// Whether [`ExternalProfile::read_cookies`] can read this browser's
    /// cookies on the current platform. Firefox stores them unencrypted
    /// everywhere; Chromium-family cookies need the macOS login keychain;
    /// Safari's are never readable.
    pub const fn supports_cookies(self) -> bool {
        match self {
            ExternalBrowser::Firefox => true,
            ExternalBrowser::Safari => false,
            _ => cfg!(target_os = "macos"),
        }
    }

    /// The name people know the browser by.
    pub const fn name(self) -> &'static str {
        match self {
            ExternalBrowser::Chrome => "Google Chrome",
            ExternalBrowser::Chromium => "Chromium",
            ExternalBrowser::Edge => "Microsoft Edge",
            ExternalBrowser::Brave => "Brave",
            ExternalBrowser::Vivaldi => "Vivaldi",
            ExternalBrowser::Opera => "Opera",
            ExternalBrowser::Arc => "Arc",
            ExternalBrowser::Firefox => "Firefox",
            ExternalBrowser::Safari => "Safari",
        }
    }

    /// The current user's profiles of this browser, found in the places the
    /// browser normally keeps them. Empty if it is not installed.
    pub fn profiles(self) -> Vec<ExternalProfile> {
        let mut profiles = Vec::new();
        for location in locations(self) {
            profiles.extend(match self {
                ExternalBrowser::Firefox => firefox::profiles(&location),
                ExternalBrowser::Safari => safari::profiles(&location),
                _ => chromium::profiles(self, &location),
            });
        }
        profiles
    }
}

impl fmt::Display for ExternalBrowser {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// Every profile of every supported browser found for the current user.
pub fn detect() -> Vec<ExternalProfile> {
    ExternalBrowser::ALL
        .into_iter()
        .flat_map(ExternalBrowser::profiles)
        .collect()
}

/// A profile of an installed browser.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExternalProfile {
    browser: ExternalBrowser,
    name: String,
    path: PathBuf,
}

impl ExternalProfile {
    /// Describes a profile in a non-standard location, such as a portable
    /// installation. `path` is the profile directory itself: the one holding
    /// `History` for Chromium-family browsers, `places.sqlite` for Firefox,
    /// `History.db` for Safari.
    pub fn new(
        browser: ExternalBrowser,
        name: impl Into<String>,
        path: impl Into<PathBuf>,
    ) -> Self {
        Self {
            browser,
            name: name.into(),
            path: path.into(),
        }
    }

    pub fn browser(&self) -> ExternalBrowser {
        self.browser
    }

    /// The profile's name as the browser shows it.
    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Reads the profile's visits, oldest first, optionally only those at or
    /// after `since`. Internal pages of the browser are left out.
    pub fn read_history(&self, since: Option<Timestamp>) -> Result<Vec<Visit>> {
        let mut visits = match self.browser {
            ExternalBrowser::Firefox => firefox::read_history(&self.path, since),
            ExternalBrowser::Safari => safari::read_history(&self.path, since),
            _ => chromium::read_history(&self.path, since),
        }?;
        visits.retain(|visit| crate::is_recordable_url(&visit.url));
        for visit in &mut visits {
            visit.source = self.browser.id().to_owned();
        }
        Ok(visits)
    }

    /// Reads the profile's cookies.
    ///
    /// Firefox cookies are stored unencrypted. Chromium-family cookies are
    /// decrypted with the browser's key from the macOS login keychain, so this
    /// is macOS-only for them and the application may be prompted for keychain
    /// access; a denial is [`Error::PermissionDenied`]. Safari is
    /// [`Error::UnsupportedSource`]. See
    /// [`ExternalBrowser::supports_cookies`].
    pub fn read_cookies(&self) -> Result<Vec<Cookie>> {
        match self.browser {
            ExternalBrowser::Firefox => firefox::read_cookies(&self.path),
            ExternalBrowser::Safari => safari::read_cookies(&self.path),
            _ => chromium::read_cookies(self.browser, &self.path),
        }
    }

    /// Reads the profile's bookmarks. The top level holds the browser's own
    /// roots (bookmarks bar, other bookmarks, ...) as folders.
    pub fn read_bookmarks(&self) -> Result<Vec<BookmarkNode>> {
        match self.browser {
            ExternalBrowser::Firefox => firefox::read_bookmarks(&self.path),
            ExternalBrowser::Safari => safari::read_bookmarks(&self.path),
            _ => chromium::read_bookmarks(&self.path),
        }
    }

    fn bookmark_folder_title(&self) -> String {
        if self.name.is_empty() || self.name == "Default" {
            format!("Imported from {}", self.browser)
        } else {
            format!("Imported from {} ({})", self.browser, self.name)
        }
    }
}

/// What [`Profile::import_from`] should bring over.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportOptions {
    pub history: bool,
    pub bookmarks: bool,
    /// Only import visits at or after this time.
    pub since: Option<Timestamp>,
}

impl Default for ImportOptions {
    fn default() -> Self {
        Self {
            history: true,
            bookmarks: true,
            since: None,
        }
    }
}

/// What [`Profile::import_from`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ImportReport {
    /// Visits read from the other browser.
    pub visits_found: usize,
    /// Visits that were not in the profile yet.
    pub visits_added: usize,
    /// Bookmarks (links, not folders) copied.
    pub bookmarks_added: usize,
}

pub(crate) fn run(
    profile: &Profile,
    source: &ExternalProfile,
    options: &ImportOptions,
) -> Result<ImportReport> {
    // Read everything before writing anything, so that a source that cannot
    // be read leaves the profile untouched. A browser profile that simply has
    // no history or no bookmarks yet is not an error.
    let visits = if options.history {
        absent_is_empty(source.read_history(options.since))?
    } else {
        Vec::new()
    };
    let bookmarks = if options.bookmarks {
        absent_is_empty(source.read_bookmarks())?
    } else {
        Vec::new()
    };

    let mut report = ImportReport {
        visits_found: visits.len(),
        ..ImportReport::default()
    };
    if !visits.is_empty() {
        report.visits_added = profile.history()?.add_visits(&visits)?;
    }
    if bookmarks.iter().any(|node| node.link_count() > 0) {
        report.bookmarks_added = profile
            .bookmarks()?
            .replace_folder(&source.bookmark_folder_title(), &bookmarks)?;
    }
    Ok(report)
}

fn absent_is_empty<T>(result: Result<Vec<T>>) -> Result<Vec<T>> {
    match result {
        Err(Error::SourceNotFound(_)) => Ok(Vec::new()),
        other => other,
    }
}

/// Directories in which `browser` keeps its profiles for the current user.
fn locations(browser: ExternalBrowser) -> Vec<PathBuf> {
    let Some(home) = dirs::home_dir() else {
        return Vec::new();
    };
    let mut locations = Vec::new();

    #[cfg(target_os = "macos")]
    {
        let support = home.join("Library/Application Support");
        locations.push(match browser {
            ExternalBrowser::Chrome => support.join("Google/Chrome"),
            ExternalBrowser::Chromium => support.join("Chromium"),
            ExternalBrowser::Edge => support.join("Microsoft Edge"),
            ExternalBrowser::Brave => support.join("BraveSoftware/Brave-Browser"),
            ExternalBrowser::Vivaldi => support.join("Vivaldi"),
            ExternalBrowser::Opera => support.join("com.operasoftware.Opera"),
            ExternalBrowser::Arc => support.join("Arc/User Data"),
            ExternalBrowser::Firefox => support.join("Firefox"),
            ExternalBrowser::Safari => home.join("Library/Safari"),
        });
    }

    #[cfg(target_os = "windows")]
    {
        let _ = &home;
        let local = dirs::data_local_dir();
        let roaming = dirs::data_dir();
        let relative = match browser {
            ExternalBrowser::Chrome => Some((&local, "Google/Chrome/User Data")),
            ExternalBrowser::Chromium => Some((&local, "Chromium/User Data")),
            ExternalBrowser::Edge => Some((&local, "Microsoft/Edge/User Data")),
            ExternalBrowser::Brave => Some((&local, "BraveSoftware/Brave-Browser/User Data")),
            ExternalBrowser::Vivaldi => Some((&local, "Vivaldi/User Data")),
            ExternalBrowser::Opera => Some((&roaming, "Opera Software/Opera Stable")),
            ExternalBrowser::Firefox => Some((&roaming, "Mozilla/Firefox")),
            ExternalBrowser::Arc | ExternalBrowser::Safari => None,
        };
        if let Some((Some(base), relative)) = relative {
            locations.push(base.join(relative));
        }
    }

    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        let config = dirs::config_dir().unwrap_or_else(|| home.join(".config"));
        match browser {
            ExternalBrowser::Chrome => locations.push(config.join("google-chrome")),
            ExternalBrowser::Chromium => {
                locations.push(config.join("chromium"));
                locations.push(home.join("snap/chromium/common/chromium"));
            }
            ExternalBrowser::Edge => locations.push(config.join("microsoft-edge")),
            ExternalBrowser::Brave => locations.push(config.join("BraveSoftware/Brave-Browser")),
            ExternalBrowser::Vivaldi => locations.push(config.join("vivaldi")),
            ExternalBrowser::Opera => locations.push(config.join("opera")),
            ExternalBrowser::Firefox => {
                locations.push(home.join(".mozilla/firefox"));
                locations.push(home.join("snap/firefox/common/.mozilla/firefox"));
                locations.push(home.join(".var/app/org.mozilla.firefox/.mozilla/firefox"));
            }
            ExternalBrowser::Arc | ExternalBrowser::Safari => {}
        }
    }

    locations
}

/// A private copy of a browser's SQLite database, deleted when dropped.
struct Snapshot {
    connection: Connection,
    _directory: tempfile::TempDir,
}

/// Copies `database` aside and opens the copy.
///
/// Browsers hold their databases locked while running, and reading the
/// original could also interfere with them. Uncommitted pages in the
/// write-ahead log are copied too, so the snapshot is current.
fn snapshot(database: &Path) -> Result<Snapshot> {
    check_readable(database)?;

    let mut builder = tempfile::Builder::new();
    builder.prefix("polybrowser-import-");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        // The copy is somebody's browsing history; keep it to this user.
        builder.permissions(fs::Permissions::from_mode(0o700));
    }
    let directory = builder.tempdir()?;

    let copy = directory.path().join("database.sqlite");
    fs::copy(database, &copy).map_err(|error| read_error(database, error))?;
    for suffix in ["-wal", "-journal"] {
        let mut sidecar = database.as_os_str().to_owned();
        sidecar.push(suffix);
        let mut target = copy.as_os_str().to_owned();
        target.push(suffix);
        match fs::copy(&sidecar, &target) {
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(read_error(Path::new(&sidecar), error)),
        }
    }

    Ok(Snapshot {
        connection: Connection::open(&copy)?,
        _directory: directory,
    })
}

/// Distinguishes "not there" from "not allowed". `Path::exists` cannot: on
/// macOS a protected file reports as missing to a process without access.
fn check_readable(path: &Path) -> Result<()> {
    match fs::metadata(path) {
        Ok(_) => Ok(()),
        Err(error) => Err(read_error(path, error)),
    }
}

fn read_error(path: &Path, error: io::Error) -> Error {
    match error.kind() {
        io::ErrorKind::NotFound => Error::SourceNotFound(path.to_owned()),
        io::ErrorKind::PermissionDenied => Error::PermissionDenied {
            path: path.to_owned(),
            hint: if cfg!(target_os = "macos") {
                "grant this application Full Disk Access in System Settings > Privacy & Security"
            } else {
                "check the permissions of the browser's profile directory"
            },
        },
        _ => Error::Io(error),
    }
}

fn non_empty(text: Option<String>) -> Option<String> {
    text.filter(|text| !text.trim().is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{HistoryQuery, ProfileManager};

    #[test]
    fn import_is_repeatable() {
        let dir = tempfile::tempdir().unwrap();
        let source_dir = dir.path().join("chrome/Default");
        fs::create_dir_all(&source_dir).unwrap();
        chromium::tests::write_fixture(&source_dir);
        let source = ExternalProfile::new(ExternalBrowser::Chrome, "Work", &source_dir);

        let manager = ProfileManager::open_at(dir.path().join("data")).unwrap();
        let profile = manager.create("Main").unwrap();
        profile
            .history()
            .unwrap()
            .record_visit("https://local.example/", Some("Local"))
            .unwrap();

        let first = profile
            .import_from(&source, &ImportOptions::default())
            .unwrap();
        assert_eq!(
            first,
            ImportReport {
                visits_found: 3,
                visits_added: 3,
                bookmarks_added: 2
            }
        );
        let second = profile
            .import_from(&source, &ImportOptions::default())
            .unwrap();
        assert_eq!(
            second,
            ImportReport {
                visits_found: 3,
                visits_added: 0,
                bookmarks_added: 2
            }
        );

        let history = profile.history().unwrap();
        assert_eq!(history.visit_count().unwrap(), 4);
        let imported = history
            .visits(&HistoryQuery::new().text("rust-lang"))
            .unwrap();
        assert!(imported.iter().all(|visit| visit.source == "chrome"));

        let tree = profile.bookmarks().unwrap().tree().unwrap();
        assert_eq!(tree.len(), 1);
        assert_eq!(tree[0].title, "Imported from Google Chrome (Work)");
        assert_eq!(tree[0].link_count(), 2);
    }

    #[test]
    fn options_select_what_is_imported() {
        let dir = tempfile::tempdir().unwrap();
        chromium::tests::write_fixture(dir.path());
        let source = ExternalProfile::new(ExternalBrowser::Brave, "Default", dir.path());
        let profile = Profile::ephemeral();

        let only_recent = ImportOptions {
            bookmarks: false,
            since: Some(Timestamp::from_unix_secs(1_700_000_050)),
            ..ImportOptions::default()
        };
        let report = profile.import_from(&source, &only_recent).unwrap();
        assert_eq!(
            report,
            ImportReport {
                visits_found: 2,
                visits_added: 2,
                bookmarks_added: 0
            }
        );
        assert!(profile.bookmarks().unwrap().tree().unwrap().is_empty());
    }

    #[test]
    fn missing_data_is_not_an_error_for_import_but_is_for_reads() {
        let dir = tempfile::tempdir().unwrap();
        let source = ExternalProfile::new(ExternalBrowser::Firefox, "Empty", dir.path());
        assert!(matches!(
            source.read_history(None),
            Err(Error::SourceNotFound(_))
        ));
        assert!(matches!(
            source.read_bookmarks(),
            Err(Error::SourceNotFound(_))
        ));
        let report = Profile::ephemeral()
            .import_from(&source, &ImportOptions::default())
            .unwrap();
        assert_eq!(report, ImportReport::default());
    }
}
