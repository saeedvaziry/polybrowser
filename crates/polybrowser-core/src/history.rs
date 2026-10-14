use std::{fs::File, io::BufWriter, io::Read, io::Write, path::Path, sync::Arc};

use rusqlite::{params, Connection};

use crate::{export, store::Store, HistoryFormat, Result, Timestamp};

/// One visit to a page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Visit {
    pub url: String,
    pub title: Option<String>,
    pub time: Timestamp,
    /// Where the visit was recorded: [`Visit::LOCAL`] for pages opened in this
    /// application, otherwise the id of the browser it was imported from
    /// (see [`ExternalBrowser::id`](crate::ExternalBrowser::id)).
    pub source: String,
}

impl Visit {
    pub const LOCAL: &'static str = "local";

    /// A visit made in this application.
    pub fn new(url: impl Into<String>, title: Option<String>, time: Timestamp) -> Self {
        Self {
            url: url.into(),
            title,
            time,
            source: Self::LOCAL.to_owned(),
        }
    }
}

/// A page in the history, with its visits rolled up.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryEntry {
    pub url: String,
    pub title: Option<String>,
    pub visit_count: u32,
    pub last_visit: Timestamp,
}

/// Filter for [`History::entries`] and [`History::visits`]. The default
/// matches everything.
#[derive(Debug, Clone, Default)]
pub struct HistoryQuery {
    /// Keep pages whose URL or title contains this text (case-insensitive for
    /// ASCII).
    pub text: Option<String>,
    /// Keep visits at or after this time.
    pub since: Option<Timestamp>,
    /// Keep visits before this time.
    pub until: Option<Timestamp>,
    pub limit: Option<u32>,
    pub offset: u32,
}

impl HistoryQuery {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn text(mut self, text: impl Into<String>) -> Self {
        self.text = Some(text.into());
        self
    }

    pub fn since(mut self, since: Timestamp) -> Self {
        self.since = Some(since);
        self
    }

    pub fn until(mut self, until: Timestamp) -> Self {
        self.until = Some(until);
        self
    }

    pub fn limit(mut self, limit: u32) -> Self {
        self.limit = Some(limit);
        self
    }

    pub fn offset(mut self, offset: u32) -> Self {
        self.offset = offset;
        self
    }

    fn like_pattern(&self) -> Option<String> {
        let text = self
            .text
            .as_deref()
            .map(str::trim)
            .filter(|t| !t.is_empty())?;
        let mut pattern = String::with_capacity(text.len() + 2);
        pattern.push('%');
        for character in text.chars() {
            if matches!(character, '%' | '_' | '\\') {
                pattern.push('\\');
            }
            pattern.push(character);
        }
        pattern.push('%');
        Some(pattern)
    }

    fn sql_limit(&self) -> i64 {
        self.limit.map_or(-1, i64::from)
    }
}

/// Whether a URL belongs in browsing history. Internal pages (`about:`,
/// `chrome://`, extension pages) and inline documents (`data:`, `blob:`) do not.
pub fn is_recordable_url(url: &str) -> bool {
    let Some((scheme, rest)) = url.split_once(':') else {
        return false;
    };
    !rest.is_empty()
        && ["http", "https", "ftp", "file"]
            .iter()
            .any(|allowed| scheme.eq_ignore_ascii_case(allowed))
}

/// The browsing history of a [`Profile`](crate::Profile).
///
/// Cheap to clone; clones share one database connection.
#[derive(Clone)]
pub struct History {
    store: Arc<Store>,
}

impl History {
    pub(crate) fn new(store: Arc<Store>) -> Self {
        Self { store }
    }

    /// Records a visit made just now. Returns `false` if the URL is not
    /// [recordable](is_recordable_url).
    pub fn record_visit(&self, url: &str, title: Option<&str>) -> Result<bool> {
        self.add_visit(&Visit::new(url, title.map(str::to_owned), Timestamp::now()))
    }

    /// Adds one visit. Returns `false` if it was skipped, either because the
    /// URL is not recordable or because that page already has a visit at
    /// exactly that time.
    pub fn add_visit(&self, visit: &Visit) -> Result<bool> {
        if !is_recordable_url(&visit.url) {
            return Ok(false);
        }
        let mut connection = self.store.lock();
        let transaction = connection.transaction()?;
        let inserted = insert_visit(&transaction, visit)?;
        transaction.commit()?;
        Ok(inserted)
    }

    /// Adds many visits in one transaction and returns how many were new.
    pub fn add_visits<'a>(&self, visits: impl IntoIterator<Item = &'a Visit>) -> Result<usize> {
        let mut connection = self.store.lock();
        let transaction = connection.transaction()?;
        let mut added = 0;
        for visit in visits {
            if is_recordable_url(&visit.url) && insert_visit(&transaction, visit)? {
                added += 1;
            }
        }
        transaction.commit()?;
        Ok(added)
    }

    /// Sets the title of a page that is already in the history.
    pub fn set_title(&self, url: &str, title: &str) -> Result<()> {
        self.store
            .lock()
            .prepare_cached("UPDATE urls SET title = ?2 WHERE url = ?1")?
            .execute(params![url, title])?;
        Ok(())
    }

    /// Pages matching the query, most recently visited first. `since` and
    /// `until` apply to the last visit of each page.
    pub fn entries(&self, query: &HistoryQuery) -> Result<Vec<HistoryEntry>> {
        let connection = self.store.lock();
        let mut statement = connection.prepare_cached(
            "SELECT url, title, visit_count, last_visit_time FROM urls
             WHERE visit_count > 0
               AND (?1 IS NULL OR url LIKE ?1 ESCAPE '\\' OR title LIKE ?1 ESCAPE '\\')
               AND (?2 IS NULL OR last_visit_time >= ?2)
               AND (?3 IS NULL OR last_visit_time < ?3)
             ORDER BY last_visit_time DESC, id DESC
             LIMIT ?4 OFFSET ?5",
        )?;
        let rows = statement.query_map(
            params![
                query.like_pattern(),
                query.since.map(Timestamp::as_unix_micros),
                query.until.map(Timestamp::as_unix_micros),
                query.sql_limit(),
                query.offset,
            ],
            |row| {
                Ok(HistoryEntry {
                    url: row.get(0)?,
                    title: row.get(1)?,
                    visit_count: row.get(2)?,
                    last_visit: Timestamp::from_unix_micros(row.get(3)?),
                })
            },
        )?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Individual visits matching the query, newest first.
    pub fn visits(&self, query: &HistoryQuery) -> Result<Vec<Visit>> {
        let connection = self.store.lock();
        let mut statement = connection.prepare_cached(
            "SELECT urls.url, urls.title, visits.visit_time, visits.source
             FROM visits JOIN urls ON urls.id = visits.url_id
             WHERE (?1 IS NULL OR urls.url LIKE ?1 ESCAPE '\\' OR urls.title LIKE ?1 ESCAPE '\\')
               AND (?2 IS NULL OR visits.visit_time >= ?2)
               AND (?3 IS NULL OR visits.visit_time < ?3)
             ORDER BY visits.visit_time DESC, visits.id DESC
             LIMIT ?4 OFFSET ?5",
        )?;
        let rows = statement.query_map(
            params![
                query.like_pattern(),
                query.since.map(Timestamp::as_unix_micros),
                query.until.map(Timestamp::as_unix_micros),
                query.sql_limit(),
                query.offset,
            ],
            |row| {
                Ok(Visit {
                    url: row.get(0)?,
                    title: row.get(1)?,
                    time: Timestamp::from_unix_micros(row.get(2)?),
                    source: row.get(3)?,
                })
            },
        )?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Number of distinct pages in the history.
    pub fn page_count(&self) -> Result<u64> {
        let count: i64 = self.store.lock().query_row(
            "SELECT COUNT(*) FROM urls WHERE visit_count > 0",
            [],
            |row| row.get(0),
        )?;
        Ok(count.unsigned_abs())
    }

    /// Number of visits in the history.
    pub fn visit_count(&self) -> Result<u64> {
        let count: i64 = self
            .store
            .lock()
            .query_row("SELECT COUNT(*) FROM visits", [], |row| row.get(0))?;
        Ok(count.unsigned_abs())
    }

    /// Forgets a page and all its visits. Returns whether it was there.
    pub fn delete_url(&self, url: &str) -> Result<bool> {
        let removed = self
            .store
            .lock()
            .execute("DELETE FROM urls WHERE url = ?1", [url])?;
        Ok(removed > 0)
    }

    /// Forgets every visit in `since..until` and returns how many there were.
    /// Pages left without visits are forgotten too.
    pub fn delete_range(&self, since: Timestamp, until: Timestamp) -> Result<usize> {
        let (since, until) = (since.as_unix_micros(), until.as_unix_micros());
        let mut connection = self.store.lock();
        let transaction = connection.transaction()?;
        transaction.execute_batch(
            "CREATE TEMP TABLE IF NOT EXISTS affected_urls (id INTEGER PRIMARY KEY);
             DELETE FROM affected_urls;",
        )?;
        transaction.execute(
            "INSERT OR IGNORE INTO affected_urls
             SELECT url_id FROM visits WHERE visit_time >= ?1 AND visit_time < ?2",
            params![since, until],
        )?;
        let removed = transaction.execute(
            "DELETE FROM visits WHERE visit_time >= ?1 AND visit_time < ?2",
            params![since, until],
        )?;
        transaction.execute_batch(
            "UPDATE urls SET
                 visit_count = (SELECT COUNT(*) FROM visits WHERE url_id = urls.id),
                 last_visit_time =
                     (SELECT COALESCE(MAX(visit_time), 0) FROM visits WHERE url_id = urls.id)
             WHERE id IN (SELECT id FROM affected_urls);
             DELETE FROM urls WHERE visit_count = 0 AND id IN (SELECT id FROM affected_urls);
             DELETE FROM affected_urls;",
        )?;
        transaction.commit()?;
        Ok(removed)
    }

    /// Forgets everything.
    pub fn clear(&self) -> Result<()> {
        self.store
            .lock()
            .execute_batch("DELETE FROM visits; DELETE FROM urls;")?;
        Ok(())
    }

    /// Writes the whole history to `writer` and returns the number of records
    /// written. See [`HistoryFormat`] for what each format contains.
    pub fn export(&self, format: HistoryFormat, mut writer: impl Write) -> Result<usize> {
        let written = match format {
            HistoryFormat::Json => {
                let visits = self.visits(&HistoryQuery::new())?;
                export::history_json(&visits, &mut writer)?;
                visits.len()
            }
            HistoryFormat::Csv => {
                let visits = self.visits(&HistoryQuery::new())?;
                export::history_csv(&visits, &mut writer)?;
                visits.len()
            }
            HistoryFormat::Html => {
                let entries = self.entries(&HistoryQuery::new())?;
                export::history_html(&entries, &mut writer)?;
                entries.len()
            }
        };
        writer.flush()?;
        Ok(written)
    }

    /// [`export`](Self::export) into a file, replacing it if it exists.
    pub fn export_to_file(&self, format: HistoryFormat, path: impl AsRef<Path>) -> Result<usize> {
        self.export(format, BufWriter::new(File::create(path)?))
    }

    /// Reads back a file written by [`export`](Self::export) with
    /// [`HistoryFormat::Json`] and returns how many visits were new.
    pub fn import_json(&self, reader: impl Read) -> Result<usize> {
        let visits = export::read_history_json(reader)?;
        self.add_visits(&visits)
    }
}

/// Inserts a visit, keeping the page's rolled-up columns in step. Must run
/// inside a transaction.
fn insert_visit(connection: &Connection, visit: &Visit) -> rusqlite::Result<bool> {
    let title = visit
        .title
        .as_deref()
        .map(str::trim)
        .filter(|title| !title.is_empty());
    let time = visit.time.as_unix_micros();

    // A title only replaces the stored one when it comes from a visit at least
    // as recent, so importing old history never clobbers a current title.
    let url_id: i64 = connection
        .prepare_cached(
            "INSERT INTO urls (url, title) VALUES (?1, ?2)
             ON CONFLICT (url) DO UPDATE SET title = CASE
                 WHEN ?2 IS NOT NULL AND (urls.title IS NULL OR ?3 >= urls.last_visit_time)
                 THEN ?2 ELSE urls.title END
             RETURNING id",
        )?
        .query_row(params![visit.url, title, time], |row| row.get(0))?;

    let inserted = connection
        .prepare_cached(
            "INSERT OR IGNORE INTO visits (url_id, visit_time, source) VALUES (?1, ?2, ?3)",
        )?
        .execute(params![url_id, time, visit.source])?
        == 1;
    if inserted {
        connection
            .prepare_cached(
                "UPDATE urls SET visit_count = visit_count + 1,
                                 last_visit_time = MAX(last_visit_time, ?2)
                 WHERE id = ?1",
            )?
            .execute(params![url_id, time])?;
    }
    Ok(inserted)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn history() -> History {
        History::new(Arc::new(Store::in_memory().unwrap()))
    }

    fn visit(url: &str, title: &str, secs: i64) -> Visit {
        Visit::new(url, Some(title.to_owned()), Timestamp::from_unix_secs(secs))
    }

    #[test]
    fn rolls_up_visits_per_page() {
        let history = history();
        assert!(history
            .add_visit(&visit("https://a.example/", "A", 100))
            .unwrap());
        assert!(history
            .add_visit(&visit("https://a.example/", "A again", 300))
            .unwrap());
        assert!(history
            .add_visit(&visit("https://b.example/", "B", 200))
            .unwrap());

        let entries = history.entries(&HistoryQuery::new()).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].url, "https://a.example/");
        assert_eq!(entries[0].title.as_deref(), Some("A again"));
        assert_eq!(entries[0].visit_count, 2);
        assert_eq!(entries[0].last_visit, Timestamp::from_unix_secs(300));
        assert_eq!(entries[1].url, "https://b.example/");
        assert_eq!(history.page_count().unwrap(), 2);
        assert_eq!(history.visit_count().unwrap(), 3);
    }

    #[test]
    fn duplicate_visits_are_ignored() {
        let history = history();
        let first = visit("https://a.example/", "A", 100);
        assert_eq!(history.add_visits([&first, &first]).unwrap(), 1);
        assert!(!history.add_visit(&first).unwrap());
        assert_eq!(
            history.entries(&HistoryQuery::new()).unwrap()[0].visit_count,
            1
        );
    }

    #[test]
    fn older_visits_do_not_replace_the_title() {
        let history = history();
        history
            .add_visit(&visit("https://a.example/", "Current", 500))
            .unwrap();
        history
            .add_visit(&visit("https://a.example/", "Stale", 100))
            .unwrap();
        let entries = history.entries(&HistoryQuery::new()).unwrap();
        assert_eq!(entries[0].title.as_deref(), Some("Current"));
        assert_eq!(entries[0].last_visit, Timestamp::from_unix_secs(500));
    }

    #[test]
    fn skips_internal_urls() {
        let history = history();
        for url in [
            "about:blank",
            "chrome://settings",
            "data:text/html,hi",
            "javascript:void(0)",
            "nonsense",
        ] {
            assert!(!history.record_visit(url, None).unwrap(), "{url}");
        }
        assert!(history.record_visit("HTTPS://example.com/", None).unwrap());
        assert_eq!(history.page_count().unwrap(), 1);
    }

    #[test]
    fn searches_text_and_time() {
        let history = history();
        history
            .add_visit(&visit("https://rust-lang.org/", "Rust", 100))
            .unwrap();
        history
            .add_visit(&visit("https://example.com/100%_off", "Sale", 200))
            .unwrap();
        history
            .add_visit(&visit("https://example.com/other", "Other", 300))
            .unwrap();

        let by_title = history.entries(&HistoryQuery::new().text("rust")).unwrap();
        assert_eq!(by_title.len(), 1);
        // LIKE wildcards in the search text are matched literally.
        assert_eq!(
            history
                .entries(&HistoryQuery::new().text("100%_"))
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            history
                .entries(&HistoryQuery::new().text("%"))
                .unwrap()
                .len(),
            1
        );

        let window = HistoryQuery::new()
            .since(Timestamp::from_unix_secs(200))
            .until(Timestamp::from_unix_secs(300));
        let visits = history.visits(&window).unwrap();
        assert_eq!(visits.len(), 1);
        assert_eq!(visits[0].title.as_deref(), Some("Sale"));

        let paged = history
            .visits(&HistoryQuery::new().limit(1).offset(1))
            .unwrap();
        assert_eq!(paged[0].url, "https://example.com/100%_off");
    }

    #[test]
    fn deleting_a_range_fixes_the_roll_up() {
        let history = history();
        history
            .add_visit(&visit("https://a.example/", "A", 100))
            .unwrap();
        history
            .add_visit(&visit("https://a.example/", "A", 300))
            .unwrap();
        history
            .add_visit(&visit("https://b.example/", "B", 310))
            .unwrap();

        let removed = history
            .delete_range(
                Timestamp::from_unix_secs(250),
                Timestamp::from_unix_secs(400),
            )
            .unwrap();
        assert_eq!(removed, 2);

        let entries = history.entries(&HistoryQuery::new()).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].visit_count, 1);
        assert_eq!(entries[0].last_visit, Timestamp::from_unix_secs(100));

        assert!(history.delete_url("https://a.example/").unwrap());
        assert!(!history.delete_url("https://a.example/").unwrap());
        assert_eq!(history.visit_count().unwrap(), 0);
    }
}
