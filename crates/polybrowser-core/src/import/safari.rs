//! Safari, which exists on macOS only.

use std::path::Path;

use super::ExternalProfile;
use crate::{BookmarkNode, Cookie, Result, Timestamp, Visit};

#[cfg(not(target_os = "macos"))]
mod imp {
    use super::*;
    use crate::Error;

    pub(in crate::import) fn profiles(_directory: &Path) -> Vec<ExternalProfile> {
        Vec::new()
    }

    pub(in crate::import) fn read_history(
        _directory: &Path,
        _since: Option<Timestamp>,
    ) -> Result<Vec<Visit>> {
        Err(Error::UnsupportedSource("Safari"))
    }

    pub(in crate::import) fn read_bookmarks(_directory: &Path) -> Result<Vec<BookmarkNode>> {
        Err(Error::UnsupportedSource("Safari"))
    }

    pub(in crate::import) fn read_cookies(_directory: &Path) -> Result<Vec<Cookie>> {
        Err(Error::UnsupportedSource("Safari cookies"))
    }
}

#[cfg(target_os = "macos")]
mod imp {
    use std::{fs::File, io::BufReader};

    use plist::{Dictionary, Value};

    use super::*;
    use crate::{
        import::{check_readable, non_empty, read_error, snapshot, ExternalBrowser},
        Error,
    };

    const HISTORY_FILE: &str = "History.db";
    const BOOKMARKS_FILE: &str = "Bookmarks.plist";

    /// Safari is part of macOS, so it is always reported. Its data directory
    /// is protected: without Full Disk Access the files in it look missing,
    /// which is why this does not check for them.
    pub(in crate::import) fn profiles(directory: &Path) -> Vec<ExternalProfile> {
        vec![ExternalProfile::new(
            ExternalBrowser::Safari,
            "Default",
            directory,
        )]
    }

    pub(in crate::import) fn read_history(
        directory: &Path,
        since: Option<Timestamp>,
    ) -> Result<Vec<Visit>> {
        let snapshot = snapshot(&directory.join(HISTORY_FILE))?;
        let mut statement = snapshot.connection.prepare(
            "SELECT history_items.url, history_visits.title, history_visits.visit_time
             FROM history_visits JOIN history_items ON history_items.id = history_visits.history_item
             WHERE history_visits.visit_time >= ?1
             ORDER BY history_visits.visit_time",
        )?;
        let since = since.map_or(f64::MIN, Timestamp::as_cocoa_secs);
        let rows = statement.query_map([since], |row| {
            Ok(Visit::new(
                row.get::<_, String>(0)?,
                non_empty(row.get(1)?),
                Timestamp::from_cocoa_secs(row.get(2)?),
            ))
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Safari encrypts cookies and stores them in a binary format macOS
    /// protects; reading them is not supported.
    pub(in crate::import) fn read_cookies(_directory: &Path) -> Result<Vec<Cookie>> {
        Err(Error::UnsupportedSource("Safari cookies"))
    }

    pub(in crate::import) fn read_bookmarks(directory: &Path) -> Result<Vec<BookmarkNode>> {
        let path = directory.join(BOOKMARKS_FILE);
        check_readable(&path)?;
        let file = File::open(&path).map_err(|error| read_error(&path, error))?;
        let root = Value::from_reader(BufReader::new(file)).map_err(|error| Error::Malformed {
            what: "Safari bookmarks file",
            message: error.to_string(),
        })?;
        let children = root
            .as_dictionary()
            .and_then(|root| root.get("Children"))
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or_default();
        Ok(children
            .iter()
            .filter_map(convert)
            .filter(|node| node.link_count() > 0)
            .collect())
    }

    fn convert(value: &Value) -> Option<BookmarkNode> {
        let item = value.as_dictionary()?;
        match text(item, "WebBookmarkType")? {
            "WebBookmarkTypeLeaf" => {
                let url = text(item, "URLString")?;
                let title = item
                    .get("URIDictionary")
                    .and_then(Value::as_dictionary)
                    .and_then(|uri| text(uri, "title"))
                    .unwrap_or(url);
                Some(BookmarkNode::link(title, url))
            }
            "WebBookmarkTypeList" => {
                let title = match text(item, "Title").unwrap_or_default() {
                    "BookmarksBar" => "Favorites",
                    "BookmarksMenu" => "Bookmarks Menu",
                    "com.apple.ReadingList" => "Reading List",
                    other => other,
                };
                let children = item
                    .get("Children")
                    .and_then(Value::as_array)
                    .map(|children| children.iter().filter_map(convert).collect())
                    .unwrap_or_default();
                Some(BookmarkNode::folder(title, children))
            }
            // "WebBookmarkTypeProxy" entries stand for History and similar.
            _ => None,
        }
    }

    fn text<'a>(dictionary: &'a Dictionary, key: &str) -> Option<&'a str> {
        dictionary.get(key).and_then(Value::as_string)
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use rusqlite::Connection;

        #[test]
        fn reads_history() {
            let dir = tempfile::tempdir().unwrap();
            Connection::open(dir.path().join(HISTORY_FILE))
                .unwrap()
                .execute_batch(
                    "CREATE TABLE history_items (id INTEGER PRIMARY KEY AUTOINCREMENT, url TEXT NOT NULL UNIQUE,
                         domain_expansion TEXT NULL, visit_count INTEGER NOT NULL);
                     CREATE TABLE history_visits (id INTEGER PRIMARY KEY AUTOINCREMENT,
                         history_item INTEGER NOT NULL REFERENCES history_items(id) ON DELETE CASCADE,
                         visit_time REAL NOT NULL, title TEXT NULL);
                     INSERT INTO history_items (id, url, visit_count) VALUES
                         (1, 'https://www.apple.com/', 2), (2, 'https://example.com/', 1);
                     INSERT INTO history_visits (history_item, visit_time, title) VALUES
                         (1, 631152000.0, 'Apple'), (2, 631152060.5, ''), (1, 631152120.0, 'Apple');",
                )
                .unwrap();
            let profile = ExternalProfile::new(ExternalBrowser::Safari, "Default", dir.path());

            let visits = profile.read_history(None).unwrap();
            assert_eq!(visits.len(), 3);
            assert_eq!(visits[0].title.as_deref(), Some("Apple"));
            assert_eq!(visits[0].time, Timestamp::from_unix_secs(1_609_459_200));
            assert_eq!(visits[1].title, None);
            assert_eq!(
                visits[1].time,
                Timestamp::from_unix_micros(1_609_459_260_500_000)
            );
            assert_eq!(visits[2].source, "safari");

            let recent = profile
                .read_history(Some(Timestamp::from_unix_secs(1_609_459_300)))
                .unwrap();
            assert_eq!(recent.len(), 1);
        }

        #[test]
        fn reads_bookmarks() {
            let dir = tempfile::tempdir().unwrap();
            let leaf = |title: &str, url: &str| {
                let mut uri = Dictionary::new();
                uri.insert("title".into(), Value::String(title.into()));
                let mut leaf = Dictionary::new();
                leaf.insert(
                    "WebBookmarkType".into(),
                    Value::String("WebBookmarkTypeLeaf".into()),
                );
                leaf.insert("URLString".into(), Value::String(url.into()));
                leaf.insert("URIDictionary".into(), Value::Dictionary(uri));
                Value::Dictionary(leaf)
            };
            let list = |title: &str, children: Vec<Value>| {
                let mut list = Dictionary::new();
                list.insert(
                    "WebBookmarkType".into(),
                    Value::String("WebBookmarkTypeList".into()),
                );
                list.insert("Title".into(), Value::String(title.into()));
                list.insert("Children".into(), Value::Array(children));
                Value::Dictionary(list)
            };
            let mut proxy = Dictionary::new();
            proxy.insert(
                "WebBookmarkType".into(),
                Value::String("WebBookmarkTypeProxy".into()),
            );
            proxy.insert("Title".into(), Value::String("History".into()));

            let root = list(
                "",
                vec![
                    Value::Dictionary(proxy),
                    list(
                        "BookmarksBar",
                        vec![
                            leaf("Apple", "https://www.apple.com/"),
                            list("News", vec![leaf("Example", "https://example.com/")]),
                        ],
                    ),
                    list("BookmarksMenu", vec![]),
                ],
            );
            // Safari writes the binary flavour of the format.
            root.to_file_binary(dir.path().join(BOOKMARKS_FILE))
                .unwrap();

            let roots = read_bookmarks(dir.path()).unwrap();
            assert_eq!(roots.len(), 1, "empty and proxy entries are dropped");
            assert_eq!(roots[0].title, "Favorites");
            assert_eq!(
                roots[0].children[0],
                BookmarkNode::link("Apple", "https://www.apple.com/")
            );
            assert_eq!(roots[0].children[1].children[0].title, "Example");
        }

        #[test]
        fn missing_files_are_reported_as_missing() {
            let dir = tempfile::tempdir().unwrap();
            assert!(matches!(
                read_history(dir.path(), None),
                Err(Error::SourceNotFound(_))
            ));
            assert!(matches!(
                read_bookmarks(dir.path()),
                Err(Error::SourceNotFound(_))
            ));
        }
    }
}

pub(super) use imp::{profiles, read_bookmarks, read_cookies, read_history};
