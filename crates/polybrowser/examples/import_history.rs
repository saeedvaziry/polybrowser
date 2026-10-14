//! Profiles, import and export without any window.
//!
//! ```text
//! cargo run -p polybrowser --example import_history                 # list what can be imported
//! cargo run -p polybrowser --example import_history -- <directory>  # import everything, then export
//! ```
//!
//! With a directory, a profile is created inside it, the history and
//! bookmarks of every browser found on this machine are imported into the
//! profile, and the result is exported next to it.

use std::{fs::File, path::PathBuf};

use polybrowser::{import, HistoryFormat, HistoryQuery, ImportOptions, ProfileManager};

fn main() -> polybrowser::Result<()> {
    let sources = import::detect();
    if sources.is_empty() {
        println!("No supported browser found for this user.");
    }
    for source in &sources {
        println!(
            "{:<16} {:<24} {}",
            source.browser().name(),
            source.name(),
            source.path().display()
        );
    }

    let Some(directory) = std::env::args().nth(1).map(PathBuf::from) else {
        println!("\nPass a directory to import these into a profile stored there.");
        return Ok(());
    };

    let profiles = ProfileManager::open_at(directory.join("data"))?;
    let profile = profiles.get_or_create("Imported")?;
    println!();
    for source in &sources {
        match profile.import_from(source, &ImportOptions::default()) {
            Ok(report) => println!(
                "{} / {}: {} of {} visits new, {} bookmarks",
                source.browser(),
                source.name(),
                report.visits_added,
                report.visits_found,
                report.bookmarks_added,
            ),
            // One unreadable browser should not stop the others.
            Err(error) => println!(
                "{} / {}: skipped ({error})",
                source.browser(),
                source.name()
            ),
        }
    }

    let history = profile.history()?;
    println!(
        "\n{} pages, {} visits in profile \"{}\"",
        history.page_count()?,
        history.visit_count()?,
        profile.name()
    );
    for entry in history.entries(&HistoryQuery::new().limit(5))? {
        println!(
            "  {}  {}",
            entry.last_visit.to_rfc3339(),
            entry.title.as_deref().unwrap_or(&entry.url)
        );
    }

    for format in [HistoryFormat::Json, HistoryFormat::Csv, HistoryFormat::Html] {
        let path = directory.join(format!("history.{}", format.extension()));
        let records = history.export_to_file(format, &path)?;
        println!("wrote {records} records to {}", path.display());
    }
    let bookmarks_path = directory.join("bookmarks.html");
    profile
        .bookmarks()?
        .export_html(File::create(&bookmarks_path)?)?;
    println!("wrote bookmarks to {}", bookmarks_path.display());
    Ok(())
}
