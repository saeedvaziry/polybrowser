//! Serialisation of history and bookmarks into files other tools can read.

use std::io::{self, Read, Write};

use serde::{Deserialize, Serialize};

use crate::{BookmarkNode, Error, HistoryEntry, Result, Timestamp, Visit};

/// File formats [`History::export`](crate::History::export) can write.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum HistoryFormat {
    /// Every visit, losslessly. The only format that can be read back, with
    /// [`History::import_json`](crate::History::import_json).
    Json,
    /// Every visit as one row of `url,title,visited_at,visited_at_unix_micros,source`,
    /// for spreadsheets and scripts.
    Csv,
    /// One link per page in the Netscape bookmark file format, newest first.
    /// Browsers can import it, as a folder of bookmarks.
    Html,
}

impl HistoryFormat {
    /// The conventional file extension, without the dot.
    pub const fn extension(self) -> &'static str {
        match self {
            HistoryFormat::Json => "json",
            HistoryFormat::Csv => "csv",
            HistoryFormat::Html => "html",
        }
    }
}

const JSON_FORMAT: &str = "polybrowser-history";
const JSON_VERSION: u32 = 1;

#[derive(Serialize)]
struct JsonVisitOut<'a> {
    url: &'a str,
    title: Option<&'a str>,
    visited_at: String,
    visited_at_unix_micros: i64,
    source: &'a str,
}

#[derive(Deserialize)]
struct JsonFileIn {
    format: String,
    version: u32,
    visits: Vec<JsonVisitIn>,
}

#[derive(Deserialize)]
struct JsonVisitIn {
    url: String,
    title: Option<String>,
    visited_at_unix_micros: i64,
    #[serde(default)]
    source: Option<String>,
}

pub(crate) fn history_json(visits: &[Visit], writer: &mut dyn Write) -> Result<()> {
    // Written by hand rather than as one serde value so that the visit list
    // is streamed instead of being duplicated in memory.
    write!(
        writer,
        "{{\n  \"format\": \"{JSON_FORMAT}\",\n  \"version\": {JSON_VERSION},\n  \"exported_at\": \"{}\",\n  \"visits\": [",
        Timestamp::now().to_rfc3339()
    )?;
    for (index, visit) in visits.iter().enumerate() {
        writer.write_all(if index == 0 { b"\n    " } else { b",\n    " })?;
        serde_json::to_writer(
            &mut *writer,
            &JsonVisitOut {
                url: &visit.url,
                title: visit.title.as_deref(),
                visited_at: visit.time.to_rfc3339(),
                visited_at_unix_micros: visit.time.as_unix_micros(),
                source: &visit.source,
            },
        )?;
    }
    writer.write_all(if visits.is_empty() {
        b"]\n}\n"
    } else {
        b"\n  ]\n}\n"
    })?;
    Ok(())
}

pub(crate) fn read_history_json(reader: impl Read) -> Result<Vec<Visit>> {
    let file: JsonFileIn = serde_json::from_reader(io::BufReader::new(reader))?;
    if file.format != JSON_FORMAT || file.version != JSON_VERSION {
        return Err(Error::Malformed {
            what: "history export",
            message: format!(
                "expected {JSON_FORMAT} version {JSON_VERSION}, found {} version {}",
                file.format, file.version
            ),
        });
    }
    Ok(file
        .visits
        .into_iter()
        .map(|visit| Visit {
            url: visit.url,
            title: visit.title,
            time: Timestamp::from_unix_micros(visit.visited_at_unix_micros),
            source: visit.source.unwrap_or_else(|| Visit::LOCAL.to_owned()),
        })
        .collect())
}

pub(crate) fn history_csv(visits: &[Visit], writer: &mut dyn Write) -> Result<()> {
    writer.write_all(b"url,title,visited_at,visited_at_unix_micros,source\r\n")?;
    for visit in visits {
        csv_field(writer, &visit.url)?;
        writer.write_all(b",")?;
        csv_field(writer, visit.title.as_deref().unwrap_or(""))?;
        write!(
            writer,
            ",{},{},",
            visit.time.to_rfc3339(),
            visit.time.as_unix_micros()
        )?;
        csv_field(writer, &visit.source)?;
        writer.write_all(b"\r\n")?;
    }
    Ok(())
}

fn csv_field(writer: &mut dyn Write, value: &str) -> io::Result<()> {
    // Page titles are chosen by web sites, and a spreadsheet will evaluate a
    // cell that starts like a formula. A leading apostrophe makes it text.
    let formula_like = value.starts_with(['=', '+', '-', '@', '\t', '\r']);
    let needs_quotes = formula_like || value.contains([',', '"', '\n', '\r']);
    if !needs_quotes {
        return writer.write_all(value.as_bytes());
    }
    writer.write_all(b"\"")?;
    if formula_like {
        writer.write_all(b"'")?;
    }
    writer.write_all(value.replace('"', "\"\"").as_bytes())?;
    writer.write_all(b"\"")
}

const NETSCAPE_HEADER: &str = "<!DOCTYPE NETSCAPE-Bookmark-file-1>\n\
<!-- This is an automatically generated file.\n     It will be read and overwritten.\n     DO NOT EDIT! -->\n\
<META HTTP-EQUIV=\"Content-Type\" CONTENT=\"text/html; charset=UTF-8\">\n";

pub(crate) fn history_html(entries: &[HistoryEntry], writer: &mut dyn Write) -> Result<()> {
    write!(
        writer,
        "{NETSCAPE_HEADER}<TITLE>History</TITLE>\n<H1>History</H1>\n<DL><p>\n"
    )?;
    for entry in entries {
        let title = entry.title.as_deref().filter(|title| !title.is_empty());
        writeln!(
            writer,
            "    <DT><A HREF=\"{}\" ADD_DATE=\"{}\" LAST_VISIT=\"{}\">{}</A>",
            escape_html(&entry.url),
            entry.last_visit.as_unix_secs(),
            entry.last_visit.as_unix_secs(),
            escape_html(title.unwrap_or(&entry.url)),
        )?;
    }
    writer.write_all(b"</DL><p>\n")?;
    Ok(())
}

pub(crate) fn bookmarks_html(nodes: &[BookmarkNode], writer: &mut dyn Write) -> Result<()> {
    write!(
        writer,
        "{NETSCAPE_HEADER}<TITLE>Bookmarks</TITLE>\n<H1>Bookmarks</H1>\n<DL><p>\n"
    )?;
    bookmark_nodes_html(nodes, 1, writer)?;
    writer.write_all(b"</DL><p>\n")?;
    Ok(())
}

fn bookmark_nodes_html(
    nodes: &[BookmarkNode],
    depth: usize,
    writer: &mut dyn Write,
) -> io::Result<()> {
    let indent = "    ".repeat(depth);
    for node in nodes {
        let date = node.date_added.map(Timestamp::as_unix_secs).unwrap_or(0);
        match &node.url {
            Some(url) => writeln!(
                writer,
                "{indent}<DT><A HREF=\"{}\" ADD_DATE=\"{date}\">{}</A>",
                escape_html(url),
                escape_html(&node.title),
            )?,
            None => {
                writeln!(
                    writer,
                    "{indent}<DT><H3 ADD_DATE=\"{date}\">{}</H3>\n{indent}<DL><p>",
                    escape_html(&node.title),
                )?;
                bookmark_nodes_html(&node.children, depth + 1, writer)?;
                writeln!(writer, "{indent}</DL><p>")?;
            }
        }
    }
    Ok(())
}

fn escape_html(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    for character in text.chars() {
        match character {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            '\'' => escaped.push_str("&#39;"),
            other => escaped.push(other),
        }
    }
    escaped
}

#[cfg(test)]
mod tests {
    use super::*;

    fn visits() -> Vec<Visit> {
        vec![
            Visit {
                url: "https://example.com/?a=1&b=2".into(),
                title: Some("Quotes \"and\", commas".into()),
                time: Timestamp::from_unix_secs(1_609_459_200),
                source: "chrome".into(),
            },
            Visit::new(
                "https://untitled.example/",
                None,
                Timestamp::from_unix_micros(1_500_000),
            ),
        ]
    }

    #[test]
    fn json_round_trips() {
        let mut out = Vec::new();
        history_json(&visits(), &mut out).unwrap();
        let text = String::from_utf8(out.clone()).unwrap();
        assert!(
            text.contains("\"visited_at\":\"2021-01-01T00:00:00Z\""),
            "{text}"
        );
        assert_eq!(read_history_json(out.as_slice()).unwrap(), visits());

        let mut empty = Vec::new();
        history_json(&[], &mut empty).unwrap();
        assert!(read_history_json(empty.as_slice()).unwrap().is_empty());
    }

    #[test]
    fn json_rejects_other_documents() {
        let other = br#"{"format":"something-else","version":1,"visits":[]}"#;
        assert!(matches!(
            read_history_json(other.as_slice()),
            Err(Error::Malformed { .. })
        ));
    }

    #[test]
    fn csv_quotes_and_defuses_formulas() {
        let mut out = Vec::new();
        let mut rows = visits();
        rows.push(Visit::new(
            "https://evil.example/",
            Some("=HYPERLINK(\"https://evil.example\")".into()),
            Timestamp::UNIX_EPOCH,
        ));
        history_csv(&rows, &mut out).unwrap();
        let text = String::from_utf8(out).unwrap();
        let lines: Vec<&str> = text.split("\r\n").collect();
        assert_eq!(
            lines[0],
            "url,title,visited_at,visited_at_unix_micros,source"
        );
        assert_eq!(
            lines[1],
            "https://example.com/?a=1&b=2,\"Quotes \"\"and\"\", commas\",2021-01-01T00:00:00Z,1609459200000000,chrome"
        );
        assert_eq!(
            lines[2],
            "https://untitled.example/,,1970-01-01T00:00:01Z,1500000,local"
        );
        assert!(
            lines[3].contains(",\"'=HYPERLINK(\"\"https://evil.example\"\")\","),
            "{}",
            lines[3]
        );
    }

    #[test]
    fn html_escapes_markup() {
        let nodes = [BookmarkNode::folder(
            "A & B",
            vec![BookmarkNode::link("<script>", "https://x.example/?q=\"1\"")],
        )];
        let mut out = Vec::new();
        bookmarks_html(&nodes, &mut out).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.starts_with("<!DOCTYPE NETSCAPE-Bookmark-file-1>"));
        assert!(text.contains("<H3 ADD_DATE=\"0\">A &amp; B</H3>"));
        assert!(text.contains(
            "<A HREF=\"https://x.example/?q=&quot;1&quot;\" ADD_DATE=\"0\">&lt;script&gt;</A>"
        ));
    }
}
