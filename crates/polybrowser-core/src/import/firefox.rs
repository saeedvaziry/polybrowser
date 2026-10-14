//! Firefox keeps history and bookmarks together in `places.sqlite`.

use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
};

use super::{non_empty, snapshot, ExternalBrowser, ExternalProfile};
use crate::{BookmarkNode, Result, Timestamp, Visit};

const PLACES_FILE: &str = "places.sqlite";

/// Profiles listed in the `profiles.ini` of a Firefox data directory.
pub(super) fn profiles(root: &Path) -> Vec<ExternalProfile> {
    let mut found: Vec<ExternalProfile> = Vec::new();
    let mut add = |name: String, path: PathBuf| {
        if path.join(PLACES_FILE).is_file() && !found.iter().any(|known| known.path() == path) {
            found.push(ExternalProfile::new(ExternalBrowser::Firefox, name, path));
        }
    };

    if let Ok(ini) = fs::read_to_string(root.join("profiles.ini")) {
        for section in parse_ini(&ini) {
            if !section.name.starts_with("Profile") {
                continue;
            }
            let Some(path) = section.values.get("Path") else {
                continue;
            };
            let relative = section
                .values
                .get("IsRelative")
                .is_none_or(|value| value == "1");
            let path = if relative {
                root.join(path)
            } else {
                PathBuf::from(path)
            };
            let name = section
                .values
                .get("Name")
                .cloned()
                .unwrap_or_else(|| directory_name(&path));
            add(name, path);
        }
    }
    // Without a usable profiles.ini, take any directory that looks like a profile.
    for directory in [root.join("Profiles"), root.to_owned()] {
        if let Ok(entries) = fs::read_dir(directory) {
            for entry in entries.flatten() {
                add(directory_name(&entry.path()), entry.path());
            }
        }
    }
    found
}

fn directory_name(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default()
}

struct IniSection {
    name: String,
    values: HashMap<String, String>,
}

fn parse_ini(text: &str) -> Vec<IniSection> {
    let mut sections: Vec<IniSection> = Vec::new();
    for line in text.lines().map(str::trim) {
        if let Some(name) = line
            .strip_prefix('[')
            .and_then(|rest| rest.strip_suffix(']'))
        {
            sections.push(IniSection {
                name: name.to_owned(),
                values: HashMap::new(),
            });
        } else if let (Some(section), Some((key, value))) =
            (sections.last_mut(), line.split_once('='))
        {
            section
                .values
                .insert(key.trim().to_owned(), value.trim().to_owned());
        }
    }
    sections
}

pub(super) fn read_history(profile: &Path, since: Option<Timestamp>) -> Result<Vec<Visit>> {
    let snapshot = snapshot(&profile.join(PLACES_FILE))?;
    let mut statement = snapshot.connection.prepare(
        "SELECT moz_places.url, moz_places.title, moz_historyvisits.visit_date
         FROM moz_historyvisits JOIN moz_places ON moz_places.id = moz_historyvisits.place_id
         WHERE moz_historyvisits.visit_date >= ?1
         ORDER BY moz_historyvisits.visit_date",
    )?;
    // Firefox already counts microseconds from the Unix epoch.
    let since = since.map_or(i64::MIN, Timestamp::as_unix_micros);
    let rows = statement.query_map([since], |row| {
        Ok(Visit::new(
            row.get::<_, String>(0)?,
            non_empty(row.get(1)?),
            Timestamp::from_unix_micros(row.get(2)?),
        ))
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

const TYPE_LINK: i64 = 1;
const TYPE_FOLDER: i64 = 2;

struct Row {
    id: i64,
    kind: i64,
    title: Option<String>,
    url: Option<String>,
    date_added: Option<i64>,
}

pub(super) fn read_bookmarks(profile: &Path) -> Result<Vec<BookmarkNode>> {
    let snapshot = snapshot(&profile.join(PLACES_FILE))?;
    let mut statement = snapshot.connection.prepare(
        "SELECT moz_bookmarks.id, moz_bookmarks.parent, moz_bookmarks.type, moz_bookmarks.title,
                moz_places.url, moz_bookmarks.dateAdded, moz_bookmarks.guid
         FROM moz_bookmarks LEFT JOIN moz_places ON moz_places.id = moz_bookmarks.fk
         ORDER BY moz_bookmarks.parent, moz_bookmarks.position",
    )?;
    let mut children: HashMap<i64, Vec<Row>> = HashMap::new();
    let mut roots: HashMap<String, i64> = HashMap::new();
    let mut rows = statement.query([])?;
    while let Some(row) = rows.next()? {
        let id: i64 = row.get(0)?;
        let parent: Option<i64> = row.get(1)?;
        if let Some(guid) = row.get::<_, Option<String>>(6)? {
            roots.insert(guid, id);
        }
        children.entry(parent.unwrap_or(0)).or_default().push(Row {
            id,
            kind: row.get(2)?,
            title: row.get(3)?,
            url: row.get(4)?,
            date_added: row.get(5)?,
        });
    }

    // The roots are identified by fixed GUIDs; their stored titles are
    // internal names, so use the ones Firefox shows. Tags are stored as
    // bookmark folders under a root of their own and are skipped.
    let mut result = Vec::new();
    for (guid, title) in [
        ("toolbar_____", "Bookmarks Toolbar"),
        ("menu________", "Bookmarks Menu"),
        ("unfiled_____", "Other Bookmarks"),
        ("mobile______", "Mobile Bookmarks"),
    ] {
        let Some(root) = roots.get(guid) else {
            continue;
        };
        let nodes = build(*root, &mut children);
        if nodes.iter().any(|node| node.link_count() > 0) {
            result.push(BookmarkNode::folder(title, nodes));
        }
    }
    Ok(result)
}

fn build(parent: i64, children: &mut HashMap<i64, Vec<Row>>) -> Vec<BookmarkNode> {
    let mut nodes = Vec::new();
    for row in children.remove(&parent).unwrap_or_default() {
        let date_added = row
            .date_added
            .filter(|micros| *micros > 0)
            .map(Timestamp::from_unix_micros);
        let title = row.title.unwrap_or_default();
        match (row.kind, row.url) {
            // "place:" URLs are saved searches, not pages.
            (TYPE_LINK, Some(url)) if !url.starts_with("place:") => nodes.push(BookmarkNode {
                title,
                url: Some(url),
                date_added,
                children: Vec::new(),
            }),
            (TYPE_FOLDER, _) => nodes.push(BookmarkNode {
                title,
                url: None,
                date_added,
                children: build(row.id, children),
            }),
            _ => {}
        }
    }
    nodes
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    fn write_fixture(profile: &Path) {
        fs::create_dir_all(profile).unwrap();
        Connection::open(profile.join(PLACES_FILE))
            .unwrap()
            .execute_batch(
                "CREATE TABLE moz_places (id INTEGER PRIMARY KEY, url LONGVARCHAR, title LONGVARCHAR,
                     visit_count INTEGER DEFAULT 0, last_visit_date INTEGER);
                 CREATE TABLE moz_historyvisits (id INTEGER PRIMARY KEY, from_visit INTEGER,
                     place_id INTEGER, visit_date INTEGER, visit_type INTEGER, session INTEGER);
                 CREATE TABLE moz_bookmarks (id INTEGER PRIMARY KEY, type INTEGER, fk INTEGER DEFAULT NULL,
                     parent INTEGER, position INTEGER, title LONGVARCHAR, dateAdded INTEGER,
                     lastModified INTEGER, guid TEXT);
                 INSERT INTO moz_places (id, url, title) VALUES
                     (1, 'https://www.mozilla.org/', 'Mozilla'),
                     (2, 'https://example.com/', NULL),
                     (3, 'place:sort=8&maxResults=10', 'Most Visited'),
                     (4, 'about:preferences', 'Settings');
                 INSERT INTO moz_historyvisits (place_id, visit_date) VALUES
                     (1, 1700000000000000), (2, 1700000060000000), (4, 1700000070000000);
                 INSERT INTO moz_bookmarks (id, type, fk, parent, position, title, dateAdded, guid) VALUES
                     (1, 2, NULL, 0, 0, '', 0, 'root________'),
                     (2, 2, NULL, 1, 0, 'menu', 0, 'menu________'),
                     (3, 2, NULL, 1, 1, 'toolbar', 0, 'toolbar_____'),
                     (4, 2, NULL, 1, 2, 'tags', 0, 'tags________'),
                     (5, 2, NULL, 1, 3, 'unfiled', 0, 'unfiled_____'),
                     (6, 1, 2, 3, 1, 'Example', 1700000000000000, 'aaaaaaaaaaaa'),
                     (7, 1, 1, 3, 0, 'Mozilla', 1700000000000000, 'bbbbbbbbbbbb'),
                     (8, 1, 3, 3, 2, 'Most Visited', 0, 'cccccccccccc'),
                     (9, 3, NULL, 3, 3, NULL, 0, 'dddddddddddd'),
                     (10, 2, NULL, 2, 0, 'Folder', 0, 'eeeeeeeeeeee'),
                     (11, 1, 2, 10, 0, 'Nested', 0, 'ffffffffffff'),
                     (12, 2, NULL, 4, 0, 'a-tag', 0, 'gggggggggggg'),
                     (13, 1, 1, 12, 0, NULL, 0, 'hhhhhhhhhhhh');",
            )
            .unwrap();
    }

    #[test]
    fn reads_history() {
        let dir = tempfile::tempdir().unwrap();
        write_fixture(dir.path());
        let profile = ExternalProfile::new(ExternalBrowser::Firefox, "default", dir.path());
        let visits = profile.read_history(None).unwrap();
        assert_eq!(visits.len(), 2, "about: pages are not web history");
        assert_eq!(visits[0].title.as_deref(), Some("Mozilla"));
        assert_eq!(visits[0].time, Timestamp::from_unix_secs(1_700_000_000));
        assert_eq!(visits[1].title, None);
        assert_eq!(visits[1].source, "firefox");
    }

    #[test]
    fn reads_bookmarks_in_order_without_tags_or_queries() {
        let dir = tempfile::tempdir().unwrap();
        write_fixture(dir.path());
        let roots = read_bookmarks(dir.path()).unwrap();
        let titles: Vec<&str> = roots.iter().map(|root| root.title.as_str()).collect();
        assert_eq!(titles, ["Bookmarks Toolbar", "Bookmarks Menu"]);

        let toolbar: Vec<&str> = roots[0]
            .children
            .iter()
            .map(|node| node.title.as_str())
            .collect();
        assert_eq!(toolbar, ["Mozilla", "Example"]);
        assert_eq!(
            roots[0].children[0].date_added,
            Some(Timestamp::from_unix_secs(1_700_000_000))
        );
        assert_eq!(roots[1].children[0].title, "Folder");
        assert_eq!(
            roots[1].children[0].children[0].url.as_deref(),
            Some("https://example.com/")
        );
    }

    #[test]
    fn finds_profiles_from_profiles_ini() {
        let dir = tempfile::tempdir().unwrap();
        let absolute = dir.path().join("elsewhere");
        write_fixture(&dir.path().join("Profiles/abc.default-release"));
        write_fixture(&absolute);
        write_fixture(&dir.path().join("Profiles/unlisted"));
        fs::write(
            dir.path().join("profiles.ini"),
            format!(
                "[Install123]\nDefault=Profiles/abc.default-release\n\n\
                 [Profile0]\nName=default-release\nIsRelative=1\nPath=Profiles/abc.default-release\n\n\
                 [Profile1]\nName=Elsewhere\nIsRelative=0\nPath={}\n\n\
                 [Profile2]\nName=Gone\nIsRelative=1\nPath=Profiles/gone\n\n[General]\nVersion=2\n",
                absolute.display()
            ),
        )
        .unwrap();

        let found = profiles(dir.path());
        let names: Vec<&str> = found.iter().map(ExternalProfile::name).collect();
        assert_eq!(names, ["default-release", "Elsewhere", "unlisted"]);
    }
}
