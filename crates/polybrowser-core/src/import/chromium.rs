//! Chrome and the browsers built on it: Chromium, Edge, Brave, Vivaldi,
//! Opera and Arc all keep the same files in the same layout.

use std::{collections::HashMap, fs, path::Path};

use serde::Deserialize;

use super::{non_empty, read_error, snapshot, ExternalBrowser, ExternalProfile};
use crate::{BookmarkNode, Cookie, Error, Result, Timestamp, Visit};

const HISTORY_FILE: &str = "History";
const BOOKMARKS_FILE: &str = "Bookmarks";
const COOKIES_FILE: &str = "Cookies";

/// Profiles inside a "User Data" directory.
pub(super) fn profiles(browser: ExternalBrowser, user_data: &Path) -> Vec<ExternalProfile> {
    let display_names = display_names(user_data);
    let mut found = Vec::new();
    if let Ok(entries) = fs::read_dir(user_data) {
        for entry in entries.flatten() {
            let path = entry.path();
            let Some(directory) = path.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            if matches!(directory, "System Profile" | "Guest Profile") || !is_profile(&path) {
                continue;
            }
            let name = display_names
                .get(directory)
                .cloned()
                .unwrap_or_else(|| directory.to_owned());
            found.push((
                directory.to_owned(),
                ExternalProfile::new(browser, name, path),
            ));
        }
    }
    // Opera keeps a single profile directly in its data directory.
    if found.is_empty() && is_profile(user_data) {
        found.push((
            String::new(),
            ExternalProfile::new(browser, "Default", user_data),
        ));
    }
    // "Default" first, then "Profile 1", "Profile 2", ...
    found.sort_by(|(a, _), (b, _)| (a != "Default", a.len(), a).cmp(&(b != "Default", b.len(), b)));
    found.into_iter().map(|(_, profile)| profile).collect()
}

fn is_profile(directory: &Path) -> bool {
    directory.join(HISTORY_FILE).is_file() || directory.join(BOOKMARKS_FILE).is_file()
}

/// The names the user gave their profiles, keyed by directory name.
fn display_names(user_data: &Path) -> HashMap<String, String> {
    let mut names = HashMap::new();
    let Ok(bytes) = fs::read(user_data.join("Local State")) else {
        return names;
    };
    let Ok(state) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
        return names;
    };
    if let Some(cache) = state["profile"]["info_cache"].as_object() {
        for (directory, info) in cache {
            if let Some(name) = info["name"].as_str().filter(|name| !name.is_empty()) {
                names.insert(directory.clone(), name.to_owned());
            }
        }
    }
    names
}

pub(super) fn read_history(profile: &Path, since: Option<Timestamp>) -> Result<Vec<Visit>> {
    let snapshot = snapshot(&profile.join(HISTORY_FILE))?;
    let mut statement = snapshot.connection.prepare(
        "SELECT urls.url, urls.title, visits.visit_time
         FROM visits JOIN urls ON urls.id = visits.url
         WHERE visits.visit_time >= ?1
         ORDER BY visits.visit_time",
    )?;
    let since = since.map_or(i64::MIN, Timestamp::as_webkit_micros);
    let rows = statement.query_map([since], |row| {
        Ok(Visit::new(
            row.get::<_, String>(0)?,
            non_empty(row.get(1)?),
            Timestamp::from_webkit_micros(row.get(2)?),
        ))
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// Reads and decrypts the profile's cookies.
///
/// Decryption needs the browser's key from the macOS login keychain, so this
/// is macOS-only; elsewhere it reports [`Error::UnsupportedSource`].
pub(super) fn read_cookies(browser: ExternalBrowser, profile: &Path) -> Result<Vec<Cookie>> {
    cookies::read(browser, &profile.join(COOKIES_FILE))
}

/// A row of the `cookies` table, before its value is decrypted.
struct RawCookie {
    name: String,
    encrypted_value: Vec<u8>,
    value: String,
    host_key: String,
    path: String,
    secure: bool,
    http_only: bool,
    expires_utc: i64,
}

fn read_raw_cookies(database: &Path) -> Result<Vec<RawCookie>> {
    let snapshot = snapshot(database)?;
    let mut statement = snapshot.connection.prepare(
        "SELECT name, encrypted_value, value, host_key, path, is_secure, is_httponly, expires_utc
         FROM cookies",
    )?;
    let rows = statement.query_map([], |row| {
        Ok(RawCookie {
            name: row.get(0)?,
            encrypted_value: row.get::<_, Option<Vec<u8>>>(1)?.unwrap_or_default(),
            value: row.get::<_, Option<String>>(2)?.unwrap_or_default(),
            host_key: row.get(3)?,
            path: row.get(4)?,
            secure: row.get::<_, i64>(5)? != 0,
            http_only: row.get::<_, i64>(6)? != 0,
            expires_utc: row.get(7)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// `expires_utc` is microseconds since 1601; `0` means a session cookie.
fn cookie_expiry(expires_utc: i64) -> Option<Timestamp> {
    (expires_utc != 0).then(|| Timestamp::from_webkit_micros(expires_utc))
}

/// The login-keychain generic password item each browser keeps its cookie key
/// in: `(service, account)`.
fn safe_storage(browser: ExternalBrowser) -> Option<(&'static str, &'static str)> {
    Some(match browser {
        ExternalBrowser::Chrome => ("Chrome Safe Storage", "Chrome"),
        ExternalBrowser::Chromium => ("Chromium Safe Storage", "Chromium"),
        ExternalBrowser::Edge => ("Microsoft Edge Safe Storage", "Microsoft Edge"),
        ExternalBrowser::Brave => ("Brave Safe Storage", "Brave"),
        ExternalBrowser::Vivaldi => ("Vivaldi Safe Storage", "Vivaldi"),
        ExternalBrowser::Opera => ("Opera Safe Storage", "Opera"),
        ExternalBrowser::Arc => ("Arc Safe Storage", "Arc"),
        ExternalBrowser::Firefox | ExternalBrowser::Safari => return None,
    })
}

#[cfg(target_os = "macos")]
mod cookies {
    use std::path::Path;

    use super::{read_raw_cookies, safe_storage};
    use crate::{import::cookie_crypto, Cookie, Error, ExternalBrowser, Result};

    pub(super) fn read(browser: ExternalBrowser, database: &Path) -> Result<Vec<Cookie>> {
        let (service, account) =
            safe_storage(browser).ok_or(Error::UnsupportedSource("this browser's cookies"))?;
        let password = keychain_password(database, service, account)?;
        let key = cookie_crypto::derive_key(&password);

        let mut cookies = Vec::new();
        for raw in read_raw_cookies(database)? {
            let Some(value) =
                cookie_crypto::decrypt_value(&key, &raw.encrypted_value, &raw.value, &raw.host_key)
            else {
                continue;
            };
            cookies.push(Cookie {
                name: raw.name,
                value,
                domain: raw.host_key,
                path: raw.path,
                secure: raw.secure,
                http_only: raw.http_only,
                expires: super::cookie_expiry(raw.expires_utc),
            });
        }
        Ok(cookies)
    }

    /// Reads the browser's cookie-encryption password from the login keychain.
    /// Any failure, including the user declining the prompt, maps to
    /// [`Error::PermissionDenied`].
    fn keychain_password(database: &Path, service: &str, account: &str) -> Result<Vec<u8>> {
        security_framework::passwords::get_generic_password(service, account).map_err(|_| {
            Error::PermissionDenied {
                path: database.to_owned(),
                hint: "allow access to the browser's \"Safe Storage\" key in the login keychain",
            }
        })
    }
}

#[cfg(not(target_os = "macos"))]
mod cookies {
    use std::path::Path;

    use crate::{Cookie, Error, ExternalBrowser, Result};

    pub(super) fn read(_browser: ExternalBrowser, _database: &Path) -> Result<Vec<Cookie>> {
        Err(Error::UnsupportedSource("Chromium cookies off macOS"))
    }
}

#[derive(Deserialize)]
struct BookmarksFile {
    roots: HashMap<String, serde_json::Value>,
}

#[derive(Deserialize)]
struct Node {
    #[serde(default)]
    name: String,
    #[serde(rename = "type", default)]
    kind: String,
    url: Option<String>,
    /// Microseconds since 1601, as a decimal string.
    date_added: Option<String>,
    #[serde(default)]
    children: Vec<Node>,
}

pub(super) fn read_bookmarks(profile: &Path) -> Result<Vec<BookmarkNode>> {
    let path = profile.join(BOOKMARKS_FILE);
    let bytes = fs::read(&path).map_err(|error| read_error(&path, error))?;
    let mut file: BookmarksFile =
        serde_json::from_slice(&bytes).map_err(|error| Error::Malformed {
            what: "Chromium bookmarks file",
            message: error.to_string(),
        })?;

    let mut roots = Vec::new();
    for key in ["bookmark_bar", "other", "synced"] {
        // `roots` also holds entries that are not folders; skip what does not parse.
        let Some(node) = file
            .roots
            .remove(key)
            .and_then(|value| serde_json::from_value::<Node>(value).ok())
        else {
            continue;
        };
        if let Some(root) = convert(node).filter(|root| root.link_count() > 0) {
            roots.push(root);
        }
    }
    Ok(roots)
}

fn convert(node: Node) -> Option<BookmarkNode> {
    let date_added = node
        .date_added
        .as_deref()
        .and_then(|micros| micros.parse::<i64>().ok())
        .filter(|micros| *micros > 0)
        .map(Timestamp::from_webkit_micros);
    match node.kind.as_str() {
        "url" => Some(BookmarkNode {
            title: node.name,
            url: Some(node.url?),
            date_added,
            children: Vec::new(),
        }),
        "folder" => Some(BookmarkNode {
            title: node.name,
            url: None,
            date_added,
            children: node.children.into_iter().filter_map(convert).collect(),
        }),
        _ => None,
    }
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use rusqlite::Connection;

    /// 2023-11-14T22:13:20Z in Chromium's clock.
    const BASE: i64 = 1_700_000_000_000_000 + 11_644_473_600_000_000;

    /// Writes a profile directory the way Chrome lays it out, with the columns
    /// the importer relies on. Three visits to web pages, one to a settings page.
    pub(in crate::import) fn write_fixture(profile: &Path) {
        let connection = Connection::open(profile.join(HISTORY_FILE)).unwrap();
        connection
            .execute_batch(&format!(
                "CREATE TABLE urls (id INTEGER PRIMARY KEY AUTOINCREMENT, url LONGVARCHAR,
                     title LONGVARCHAR, visit_count INTEGER DEFAULT 0 NOT NULL,
                     typed_count INTEGER DEFAULT 0 NOT NULL, last_visit_time INTEGER NOT NULL,
                     hidden INTEGER DEFAULT 0 NOT NULL);
                 CREATE TABLE visits (id INTEGER PRIMARY KEY AUTOINCREMENT, url INTEGER NOT NULL,
                     visit_time INTEGER NOT NULL, from_visit INTEGER, transition INTEGER DEFAULT 0 NOT NULL,
                     segment_id INTEGER, visit_duration INTEGER DEFAULT 0 NOT NULL);
                 INSERT INTO urls (id, url, title, last_visit_time) VALUES
                     (1, 'https://www.rust-lang.org/', 'Rust', {last}),
                     (2, 'https://example.com/', '', {mid}),
                     (3, 'chrome://settings/', 'Settings', {mid});
                 INSERT INTO visits (url, visit_time) VALUES
                     (1, {BASE}), (2, {mid}), (3, {mid}), (1, {last});",
                mid = BASE + 60_000_000,
                last = BASE + 120_000_000,
            ))
            .unwrap();

        fs::write(
            profile.join(BOOKMARKS_FILE),
            format!(
                r#"{{
                  "checksum": "0",
                  "roots": {{
                    "bookmark_bar": {{ "type": "folder", "name": "Bookmarks Bar", "children": [
                      {{ "type": "url", "name": "Rust", "url": "https://www.rust-lang.org/", "date_added": "{BASE}" }},
                      {{ "type": "folder", "name": "Docs", "children": [
                        {{ "type": "url", "name": "std", "url": "https://doc.rust-lang.org/std/", "date_added": "0" }}
                      ] }}
                    ] }},
                    "other": {{ "type": "folder", "name": "Other Bookmarks", "children": [] }},
                    "synced": {{ "type": "folder", "name": "Mobile Bookmarks", "children": [] }},
                    "sync_transaction_version": "1"
                  }},
                  "version": 1
                }}"#
            ),
        )
        .unwrap();
    }

    #[test]
    fn reads_history() {
        let dir = tempfile::tempdir().unwrap();
        write_fixture(dir.path());
        let profile = ExternalProfile::new(ExternalBrowser::Chrome, "Default", dir.path());

        let visits = profile.read_history(None).unwrap();
        assert_eq!(visits.len(), 3, "the settings page is not web history");
        assert_eq!(visits[0].url, "https://www.rust-lang.org/");
        assert_eq!(visits[0].title.as_deref(), Some("Rust"));
        assert_eq!(visits[0].time, Timestamp::from_unix_secs(1_700_000_000));
        assert_eq!(visits[0].source, "chrome");
        assert_eq!(visits[1].title, None, "an empty title is no title");
        assert_eq!(visits[2].time, Timestamp::from_unix_secs(1_700_000_120));

        let recent = profile
            .read_history(Some(Timestamp::from_unix_secs(1_700_000_100)))
            .unwrap();
        assert_eq!(recent.len(), 1);
    }

    #[test]
    fn reads_bookmarks() {
        let dir = tempfile::tempdir().unwrap();
        write_fixture(dir.path());
        let roots = read_bookmarks(dir.path()).unwrap();
        assert_eq!(roots.len(), 1, "empty roots are dropped");
        let bar = &roots[0];
        assert_eq!(bar.title, "Bookmarks Bar");
        assert_eq!(
            bar.children[0].url.as_deref(),
            Some("https://www.rust-lang.org/")
        );
        assert_eq!(
            bar.children[0].date_added,
            Some(Timestamp::from_unix_secs(1_700_000_000))
        );
        assert_eq!(bar.children[1].title, "Docs");
        assert_eq!(bar.children[1].children[0].date_added, None);
    }

    fn write_cookies_fixture(profile: &Path) {
        Connection::open(profile.join(COOKIES_FILE))
            .unwrap()
            .execute_batch(
                "CREATE TABLE cookies (host_key TEXT, name TEXT, value TEXT, encrypted_value BLOB,
                     path TEXT, is_secure INTEGER, is_httponly INTEGER, expires_utc INTEGER);
                 INSERT INTO cookies VALUES
                     ('.example.com', 'sess', '', x'763130abcd', '/', 1, 1, 13300000000000000),
                     ('legacy.example', 'plain', 'kept', NULL, '/app', 0, 0, 0);",
            )
            .unwrap();
    }

    #[test]
    fn reads_raw_cookie_rows_and_expiry() {
        let dir = tempfile::tempdir().unwrap();
        write_cookies_fixture(dir.path());
        let raw = read_raw_cookies(&dir.path().join(COOKIES_FILE)).unwrap();
        assert_eq!(raw.len(), 2);

        assert_eq!(raw[0].name, "sess");
        assert_eq!(raw[0].host_key, ".example.com");
        assert!(raw[0].secure && raw[0].http_only);
        assert_eq!(raw[0].encrypted_value, b"v10\xab\xcd");
        // 13_300_000_000_000_000 µs since 1601 is 2022-03-03T05:46:40Z.
        assert_eq!(
            cookie_expiry(raw[0].expires_utc),
            Some(Timestamp::from_webkit_micros(13_300_000_000_000_000))
        );

        assert_eq!(raw[1].value, "kept");
        assert!(raw[1].encrypted_value.is_empty());
        assert_eq!(
            cookie_expiry(raw[1].expires_utc),
            None,
            "0 is a session cookie"
        );
    }

    #[test]
    fn maps_browsers_to_keychain_items() {
        assert_eq!(
            safe_storage(ExternalBrowser::Edge),
            Some(("Microsoft Edge Safe Storage", "Microsoft Edge"))
        );
        assert_eq!(safe_storage(ExternalBrowser::Firefox), None);
    }

    #[test]
    fn finds_profiles_and_their_names() {
        let dir = tempfile::tempdir().unwrap();
        for name in [
            "Default",
            "Profile 1",
            "Profile 10",
            "Profile 2",
            "System Profile",
        ] {
            let profile = dir.path().join(name);
            fs::create_dir_all(&profile).unwrap();
            fs::write(profile.join(HISTORY_FILE), b"").unwrap();
        }
        fs::create_dir_all(dir.path().join("Crashpad")).unwrap();
        fs::write(
            dir.path().join("Local State"),
            r#"{"profile":{"info_cache":{"Default":{"name":"Personal"},"Profile 1":{"name":"Work"}}}}"#,
        )
        .unwrap();

        let found = profiles(ExternalBrowser::Chrome, dir.path());
        let names: Vec<&str> = found.iter().map(ExternalProfile::name).collect();
        assert_eq!(names, ["Personal", "Work", "Profile 2", "Profile 10"]);
        assert!(found[0].path().ends_with("Default"));
    }

    #[test]
    fn treats_a_bare_data_directory_as_one_profile() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join(HISTORY_FILE), b"").unwrap();
        let found = profiles(ExternalBrowser::Opera, dir.path());
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].path(), dir.path());
        assert!(profiles(ExternalBrowser::Opera, &dir.path().join("missing")).is_empty());
    }
}
