use std::{collections::HashMap, io::Write, sync::Arc};

use rusqlite::{params, Connection, OptionalExtension, Row};

use crate::{export, store::Store, Result, Timestamp};

/// A bookmark or bookmark folder as stored in a profile.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bookmark {
    pub id: i64,
    /// The containing folder, or `None` at the top level.
    pub parent: Option<i64>,
    pub title: String,
    /// `None` for folders.
    pub url: Option<String>,
    pub date_added: Timestamp,
}

impl Bookmark {
    pub fn is_folder(&self) -> bool {
        self.url.is_none()
    }

    fn from_row(row: &Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            id: row.get(0)?,
            parent: row.get(1)?,
            title: row.get(2)?,
            url: row.get(3)?,
            date_added: Timestamp::from_unix_micros(row.get(4)?),
        })
    }
}

/// A bookmark tree detached from any database: what importers produce and
/// exporters consume. A node without a URL is a folder.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct BookmarkNode {
    pub title: String,
    pub url: Option<String>,
    pub date_added: Option<Timestamp>,
    pub children: Vec<BookmarkNode>,
}

impl BookmarkNode {
    pub fn link(title: impl Into<String>, url: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            url: Some(url.into()),
            ..Self::default()
        }
    }

    pub fn folder(title: impl Into<String>, children: Vec<BookmarkNode>) -> Self {
        Self {
            title: title.into(),
            children,
            ..Self::default()
        }
    }

    pub fn is_folder(&self) -> bool {
        self.url.is_none()
    }

    /// Number of links in this node and everything below it.
    pub fn link_count(&self) -> usize {
        usize::from(!self.is_folder())
            + self
                .children
                .iter()
                .map(BookmarkNode::link_count)
                .sum::<usize>()
    }
}

/// The bookmarks of a [`Profile`](crate::Profile).
///
/// Cheap to clone; clones share one database connection.
#[derive(Clone)]
pub struct Bookmarks {
    store: Arc<Store>,
}

const COLUMNS: &str = "id, parent_id, title, url, date_added";

impl Bookmarks {
    pub(crate) fn new(store: Arc<Store>) -> Self {
        Self { store }
    }

    /// Adds a bookmark at the end of `parent` (the top level if `None`) and
    /// returns its id.
    pub fn add(&self, parent: Option<i64>, title: &str, url: &str) -> Result<i64> {
        let connection = self.store.lock();
        Ok(insert(
            &connection,
            parent,
            title,
            Some(url),
            Timestamp::now(),
        )?)
    }

    /// Adds an empty folder at the end of `parent` and returns its id.
    pub fn add_folder(&self, parent: Option<i64>, title: &str) -> Result<i64> {
        let connection = self.store.lock();
        Ok(insert(&connection, parent, title, None, Timestamp::now())?)
    }

    /// Adds whole trees under `parent` and returns the number of links added.
    pub fn add_tree(&self, parent: Option<i64>, nodes: &[BookmarkNode]) -> Result<usize> {
        let mut connection = self.store.lock();
        let transaction = connection.transaction()?;
        let added = insert_nodes(&transaction, parent, nodes, Timestamp::now())?;
        transaction.commit()?;
        Ok(added)
    }

    pub fn get(&self, id: i64) -> Result<Option<Bookmark>> {
        Ok(self
            .store
            .lock()
            .query_row(
                &format!("SELECT {COLUMNS} FROM bookmarks WHERE id = ?1"),
                [id],
                Bookmark::from_row,
            )
            .optional()?)
    }

    /// The direct children of a folder (of the top level if `None`), in order.
    pub fn children(&self, parent: Option<i64>) -> Result<Vec<Bookmark>> {
        let connection = self.store.lock();
        let mut statement = connection.prepare_cached(&format!(
            "SELECT {COLUMNS} FROM bookmarks WHERE parent_id IS ?1 ORDER BY position, id"
        ))?;
        let rows = statement.query_map([parent], Bookmark::from_row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Every bookmark pointing at exactly this URL.
    pub fn find_by_url(&self, url: &str) -> Result<Vec<Bookmark>> {
        let connection = self.store.lock();
        let mut statement = connection.prepare_cached(&format!(
            "SELECT {COLUMNS} FROM bookmarks WHERE url = ?1 ORDER BY id"
        ))?;
        let rows = statement.query_map([url], Bookmark::from_row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn rename(&self, id: i64, title: &str) -> Result<bool> {
        let changed = self.store.lock().execute(
            "UPDATE bookmarks SET title = ?2 WHERE id = ?1",
            params![id, title],
        )?;
        Ok(changed > 0)
    }

    /// Removes a bookmark, or a folder with everything in it.
    pub fn remove(&self, id: i64) -> Result<bool> {
        let removed = self
            .store
            .lock()
            .execute("DELETE FROM bookmarks WHERE id = ?1", [id])?;
        Ok(removed > 0)
    }

    pub fn clear(&self) -> Result<()> {
        self.store.lock().execute("DELETE FROM bookmarks", [])?;
        Ok(())
    }

    /// All bookmarks as a tree.
    pub fn tree(&self) -> Result<Vec<BookmarkNode>> {
        let connection = self.store.lock();
        let mut statement = connection.prepare_cached(&format!(
            "SELECT {COLUMNS} FROM bookmarks ORDER BY position, id"
        ))?;
        let mut by_parent: HashMap<Option<i64>, Vec<Bookmark>> = HashMap::new();
        for bookmark in statement.query_map([], Bookmark::from_row)? {
            let bookmark = bookmark?;
            by_parent.entry(bookmark.parent).or_default().push(bookmark);
        }
        Ok(build_tree(None, &mut by_parent))
    }

    /// Writes all bookmarks in the Netscape bookmark file format, which every
    /// mainstream browser can import.
    pub fn export_html(&self, mut writer: impl Write) -> Result<()> {
        export::bookmarks_html(&self.tree()?, &mut writer)?;
        writer.flush()?;
        Ok(())
    }

    /// Replaces the top-level folder called `title` with a fresh one holding
    /// `nodes`. Importers use this so that importing twice does not duplicate.
    pub(crate) fn replace_folder(&self, title: &str, nodes: &[BookmarkNode]) -> Result<usize> {
        let mut connection = self.store.lock();
        let transaction = connection.transaction()?;
        transaction.execute(
            "DELETE FROM bookmarks WHERE parent_id IS NULL AND url IS NULL AND title = ?1",
            [title],
        )?;
        let now = Timestamp::now();
        let folder = insert(&transaction, None, title, None, now)?;
        let added = insert_nodes(&transaction, Some(folder), nodes, now)?;
        transaction.commit()?;
        Ok(added)
    }
}

fn insert(
    connection: &Connection,
    parent: Option<i64>,
    title: &str,
    url: Option<&str>,
    date_added: Timestamp,
) -> rusqlite::Result<i64> {
    connection
        .prepare_cached(
            "INSERT INTO bookmarks (parent_id, position, title, url, date_added)
             VALUES (?1,
                     (SELECT COALESCE(MAX(position), -1) + 1 FROM bookmarks WHERE parent_id IS ?1),
                     ?2, ?3, ?4)",
        )?
        .execute(params![parent, title, url, date_added.as_unix_micros()])?;
    Ok(connection.last_insert_rowid())
}

fn insert_nodes(
    connection: &Connection,
    parent: Option<i64>,
    nodes: &[BookmarkNode],
    fallback_date: Timestamp,
) -> rusqlite::Result<usize> {
    let mut links = 0;
    for node in nodes {
        let id = insert(
            connection,
            parent,
            &node.title,
            node.url.as_deref(),
            node.date_added.unwrap_or(fallback_date),
        )?;
        if node.is_folder() {
            links += insert_nodes(connection, Some(id), &node.children, fallback_date)?;
        } else {
            links += 1;
        }
    }
    Ok(links)
}

fn build_tree(
    parent: Option<i64>,
    by_parent: &mut HashMap<Option<i64>, Vec<Bookmark>>,
) -> Vec<BookmarkNode> {
    by_parent
        .remove(&parent)
        .unwrap_or_default()
        .into_iter()
        .map(|bookmark| BookmarkNode {
            children: if bookmark.is_folder() {
                build_tree(Some(bookmark.id), by_parent)
            } else {
                Vec::new()
            },
            title: bookmark.title,
            url: bookmark.url,
            date_added: Some(bookmark.date_added),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bookmarks() -> Bookmarks {
        Bookmarks::new(Arc::new(Store::in_memory().unwrap()))
    }

    #[test]
    fn keeps_order_and_nesting() {
        let bookmarks = bookmarks();
        let folder = bookmarks.add_folder(None, "Reading").unwrap();
        bookmarks
            .add(Some(folder), "First", "https://one.example/")
            .unwrap();
        bookmarks
            .add(Some(folder), "Second", "https://two.example/")
            .unwrap();
        bookmarks.add(None, "Top", "https://top.example/").unwrap();

        let top = bookmarks.children(None).unwrap();
        assert_eq!(
            top.iter().map(|b| b.title.as_str()).collect::<Vec<_>>(),
            ["Reading", "Top"]
        );
        assert!(top[0].is_folder());

        let tree = bookmarks.tree().unwrap();
        assert_eq!(tree.len(), 2);
        assert_eq!(tree[0].children.len(), 2);
        assert_eq!(
            tree[0].children[1].url.as_deref(),
            Some("https://two.example/")
        );
        assert_eq!(tree[0].link_count(), 2);
    }

    #[test]
    fn removing_a_folder_removes_its_contents() {
        let bookmarks = bookmarks();
        let folder = bookmarks.add_folder(None, "Folder").unwrap();
        bookmarks
            .add(Some(folder), "Inside", "https://in.example/")
            .unwrap();
        assert!(bookmarks.remove(folder).unwrap());
        assert!(bookmarks.tree().unwrap().is_empty());
        assert!(bookmarks
            .find_by_url("https://in.example/")
            .unwrap()
            .is_empty());
    }

    #[test]
    fn replacing_an_import_folder_does_not_duplicate() {
        let bookmarks = bookmarks();
        let nodes = [BookmarkNode::folder(
            "Bar",
            vec![BookmarkNode::link("A", "https://a.example/")],
        )];
        assert_eq!(bookmarks.replace_folder("Imported", &nodes).unwrap(), 1);
        assert_eq!(bookmarks.replace_folder("Imported", &nodes).unwrap(), 1);
        let tree = bookmarks.tree().unwrap();
        assert_eq!(tree.len(), 1);
        assert_eq!(tree[0].link_count(), 1);
    }
}
