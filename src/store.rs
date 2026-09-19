// Author: Jeff
// Date: 2026-09-19
// Description: mg-bookr's records — books, where you are in each, highlights, collections, chapters
// Notes: One SQLite file, $MG_BOOKR_DB or $XDG_DATA_HOME/mg-bookr/bookr.sqlite, WAL, foreign keys.
//        A book is known by its root ("books" | "audiobooks") and its path inside that root, so
//        pointing a root somewhere else keeps every book's history. A book that vanishes is
//        marked missing, never deleted: its progress and highlights wait for it to come back.
//        Migrations are append-only and recorded

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use chrono::Utc;
use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;

const MIGRATIONS: &[&str] = &[
    "\
CREATE TABLE books (id INTEGER PRIMARY KEY, kind TEXT NOT NULL, root TEXT NOT NULL, path TEXT NOT NULL, \
  title TEXT NOT NULL, author TEXT, series TEXT, series_index REAL, cover TEXT, pages INTEGER, \
  duration_seconds REAL, added_at TEXT NOT NULL, missing INTEGER NOT NULL DEFAULT 0, UNIQUE(root, path)); \
CREATE TABLE progress (book_id INTEGER PRIMARY KEY REFERENCES books(id) ON DELETE CASCADE, location TEXT NOT NULL, \
  percent REAL NOT NULL, finished INTEGER NOT NULL DEFAULT 0, updated_at TEXT NOT NULL); \
CREATE TABLE highlights (id INTEGER PRIMARY KEY, book_id INTEGER NOT NULL REFERENCES books(id) ON DELETE CASCADE, \
  location TEXT NOT NULL, chapter TEXT, quote TEXT NOT NULL, note TEXT, color TEXT NOT NULL DEFAULT 'yellow', \
  created_at TEXT NOT NULL); \
CREATE TABLE collections (id INTEGER PRIMARY KEY, name TEXT NOT NULL UNIQUE); \
CREATE TABLE collection_books (collection_id INTEGER NOT NULL REFERENCES collections(id) ON DELETE CASCADE, \
  book_id INTEGER NOT NULL REFERENCES books(id) ON DELETE CASCADE, PRIMARY KEY(collection_id, book_id)); \
CREATE TABLE tracks (book_id INTEGER NOT NULL REFERENCES books(id) ON DELETE CASCADE, idx INTEGER NOT NULL, \
  path TEXT NOT NULL, duration_seconds REAL, PRIMARY KEY(book_id, idx)); \
CREATE TABLE chapters (book_id INTEGER NOT NULL REFERENCES books(id) ON DELETE CASCADE, idx INTEGER NOT NULL, \
  title TEXT NOT NULL, start_seconds REAL NOT NULL, PRIMARY KEY(book_id, idx));",
    // M2: the speed an audiobook was last played at; a scan never touches it
    "ALTER TABLE books ADD COLUMN speed REAL;",
    // M3: how this book is read, pages or scroll; unset means the reader's own default
    "ALTER TABLE books ADD COLUMN reading_mode TEXT;",
];

// how a book can be read
pub const READING_MODES: [&str; 2] = ["pages", "scroll"];

// what kinds of book there are
pub const KINDS: [&str; 5] = ["epub", "pdf", "comic", "audio", "audio-folder"];

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Book {
    pub id: i64,
    pub kind: String,
    pub root: String,
    pub path: String,
    pub title: String,
    pub author: Option<String>,
    pub series: Option<String>,
    pub series_index: Option<f64>,
    pub cover: Option<String>,
    pub pages: Option<i64>,
    pub duration_seconds: Option<f64>,
    pub missing: bool,
    // progress, when there is any
    pub location: Option<String>,
    pub percent: Option<f64>,
    pub finished: bool,
    pub updated_at: Option<String>,
}

// What a scan learned about one book, before it has an id
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Found {
    pub kind: String,
    pub root: String,
    pub path: String,
    pub title: String,
    pub author: Option<String>,
    pub series: Option<String>,
    pub series_index: Option<f64>,
    pub cover: Option<String>,
    pub pages: Option<i64>,
    pub duration_seconds: Option<f64>,
    // audiobooks: the files, in order, with their lengths
    pub tracks: Vec<(String, Option<f64>)>,
    // audiobooks: (title, start seconds) across the whole book
    pub chapters: Vec<(String, f64)>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Highlight {
    pub id: i64,
    pub book_id: i64,
    pub location: String,
    pub chapter: Option<String>,
    pub quote: String,
    pub note: Option<String>,
    pub color: String,
    pub created_at: String,
}

pub struct Store {
    path: PathBuf,
}

// $MG_BOOKR_DB, else the XDG data folder
pub fn default_path() -> PathBuf {
    std::env::var_os("MG_BOOKR_DB")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            dirs::data_dir()
                .unwrap_or_else(|| PathBuf::from("."))
                .join("mg-bookr/bookr.sqlite")
        })
}

const BOOK_SELECT: &str = "SELECT b.id,b.kind,b.root,b.path,b.title,b.author,b.series,b.series_index,b.cover,b.pages, \
    b.duration_seconds,b.missing,p.location,p.percent,COALESCE(p.finished,0),p.updated_at \
    FROM books b LEFT JOIN progress p ON p.book_id=b.id";

fn book_from_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Book> {
    Ok(Book {
        id: r.get(0)?,
        kind: r.get(1)?,
        root: r.get(2)?,
        path: r.get(3)?,
        title: r.get(4)?,
        author: r.get(5)?,
        series: r.get(6)?,
        series_index: r.get(7)?,
        cover: r.get(8)?,
        pages: r.get(9)?,
        duration_seconds: r.get(10)?,
        missing: r.get::<_, i64>(11)? != 0,
        location: r.get(12)?,
        percent: r.get(13)?,
        finished: r.get::<_, i64>(14)? != 0,
        updated_at: r.get(15)?,
    })
}

fn highlight_from_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Highlight> {
    Ok(Highlight {
        id: r.get(0)?,
        book_id: r.get(1)?,
        location: r.get(2)?,
        chapter: r.get(3)?,
        quote: r.get(4)?,
        note: r.get(5)?,
        color: r.get(6)?,
        created_at: r.get(7)?,
    })
}

// highlight colours the reader offers
pub const COLORS: [&str; 5] = ["yellow", "green", "blue", "pink", "purple"];

impl Store {
    // Open (creating) the store and bring its schema up to date
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let store = Store { path };
        let mut c = store.conn()?;
        let mode: String = c.query_row("PRAGMA journal_mode=WAL", [], |r| r.get(0))?;
        if !mode.eq_ignore_ascii_case("wal") {
            bail!("store could not switch to WAL (journal mode {mode})")
        }
        let tx = c.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        tx.execute_batch(
            "CREATE TABLE IF NOT EXISTS schema_migrations (version INTEGER PRIMARY KEY)",
        )?;
        let applied: i64 =
            tx.query_row("SELECT COUNT(*) FROM schema_migrations", [], |r| r.get(0))?;
        if applied as usize > MIGRATIONS.len() {
            bail!("this store was written by a newer mg-bookr")
        }
        for (index, sql) in MIGRATIONS.iter().enumerate().skip(applied as usize) {
            tx.execute_batch(sql)?;
            tx.execute(
                "INSERT INTO schema_migrations(version) VALUES (?1)",
                params![index as i64 + 1],
            )?;
        }
        tx.commit()?;
        Ok(store)
    }

    fn conn(&self) -> Result<Connection> {
        let c = Connection::open(&self.path)
            .with_context(|| format!("opening {}", self.path.display()))?;
        c.busy_timeout(Duration::from_secs(5))?;
        c.execute_batch("PRAGMA foreign_keys = ON;")?;
        Ok(c)
    }

    // Record what a scan found: new books added, known ones refreshed (their history untouched),
    // and every book of these roots not seen this time marked missing. Returns (added, missing)
    pub fn save_scan(&self, roots: &[&str], found: &[Found]) -> Result<(usize, usize)> {
        let mut c = self.conn()?;
        let tx = c.transaction()?;
        let now = Utc::now().to_rfc3339();
        // everything starts presumed gone; each book found is marked back
        for root in roots {
            tx.execute(
                "UPDATE books SET missing=2 WHERE root=?1 AND missing=0",
                params![root],
            )?;
        }
        let mut added = 0;
        for f in found {
            let known: Option<i64> = tx
                .query_row(
                    "SELECT id FROM books WHERE root=?1 AND path=?2",
                    params![f.root, f.path],
                    |r| r.get(0),
                )
                .optional()?;
            let id = match known {
                Some(id) => {
                    tx.execute(
                        "UPDATE books SET kind=?1,title=?2,author=?3,series=?4,series_index=?5,cover=?6,pages=?7, \
                         duration_seconds=?8,missing=0 WHERE id=?9",
                        params![f.kind, f.title, f.author, f.series, f.series_index, f.cover, f.pages, f.duration_seconds, id],
                    )?;
                    id
                }
                None => {
                    tx.execute(
                        "INSERT INTO books(kind,root,path,title,author,series,series_index,cover,pages,duration_seconds,added_at) \
                         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)",
                        params![f.kind, f.root, f.path, f.title, f.author, f.series, f.series_index, f.cover, f.pages, f.duration_seconds, now],
                    )?;
                    added += 1;
                    tx.last_insert_rowid()
                }
            };
            // an audiobook's files and chapters are whatever the scan saw now
            tx.execute("DELETE FROM tracks WHERE book_id=?1", params![id])?;
            for (i, (path, duration)) in f.tracks.iter().enumerate() {
                tx.execute(
                    "INSERT INTO tracks(book_id,idx,path,duration_seconds) VALUES (?1,?2,?3,?4)",
                    params![id, i as i64, path, duration],
                )?;
            }
            tx.execute("DELETE FROM chapters WHERE book_id=?1", params![id])?;
            for (i, (title, start)) in f.chapters.iter().enumerate() {
                tx.execute(
                    "INSERT INTO chapters(book_id,idx,title,start_seconds) VALUES (?1,?2,?3,?4)",
                    params![id, i as i64, title, start],
                )?;
            }
        }
        let missing = tx.execute("UPDATE books SET missing=1 WHERE missing=2", [])?;
        tx.commit()?;
        Ok((added, missing))
    }

    // Books, most recently read first, then by title; missing ones only when asked
    pub fn books(&self, kind: Option<&str>, include_missing: bool) -> Result<Vec<Book>> {
        let c = self.conn()?;
        let mut st = c.prepare(&format!(
            "{BOOK_SELECT} WHERE (?1 IS NULL OR b.kind=?1 OR (?1='audio' AND b.kind='audio-folder')) AND (?2=1 OR b.missing=0) \
             ORDER BY p.updated_at IS NULL, p.updated_at DESC, b.title COLLATE NOCASE"
        ))?;
        let rows = st
            .query_map(params![kind, include_missing as i64], book_from_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    pub fn book(&self, id: i64) -> Result<Book> {
        self.conn()?
            .query_row(
                &format!("{BOOK_SELECT} WHERE b.id=?1"),
                params![id],
                book_from_row,
            )
            .optional()?
            .with_context(|| format!("no book {id}"))
    }

    // Books started and not finished, most recent first — "continue reading / listening"
    pub fn in_progress(&self, limit: usize) -> Result<Vec<Book>> {
        let c = self.conn()?;
        let mut st = c.prepare(&format!(
            "{BOOK_SELECT} WHERE p.book_id IS NOT NULL AND p.finished=0 AND b.missing=0 ORDER BY p.updated_at DESC LIMIT ?1"
        ))?;
        let rows = st
            .query_map(params![limit.clamp(1, 100) as i64], book_from_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    // Remember where you are; 99.5% or more counts as finished unless told otherwise
    pub fn set_progress(
        &self,
        id: i64,
        location: &str,
        percent: f64,
        finished: Option<bool>,
    ) -> Result<()> {
        if location.is_empty() || location.len() > 200 {
            bail!("a location is 1–200 characters")
        }
        let percent = percent.clamp(0.0, 100.0);
        let finished = finished.unwrap_or(percent >= 99.5);
        let changed = self.conn()?.execute(
            "INSERT INTO progress(book_id,location,percent,finished,updated_at) SELECT id,?2,?3,?4,?5 FROM books WHERE id=?1 \
             ON CONFLICT(book_id) DO UPDATE SET location=excluded.location,percent=excluded.percent, \
             finished=excluded.finished,updated_at=excluded.updated_at",
            params![id, location, percent, finished as i64, Utc::now().to_rfc3339()],
        )?;
        if changed == 0 {
            bail!("no book {id}")
        }
        Ok(())
    }

    pub fn tracks(&self, id: i64) -> Result<Vec<(String, Option<f64>)>> {
        let c = self.conn()?;
        let mut st =
            c.prepare("SELECT path,duration_seconds FROM tracks WHERE book_id=?1 ORDER BY idx")?;
        let rows = st
            .query_map(params![id], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    pub fn chapters(&self, id: i64) -> Result<Vec<(String, f64)>> {
        let c = self.conn()?;
        let mut st =
            c.prepare("SELECT title,start_seconds FROM chapters WHERE book_id=?1 ORDER BY idx")?;
        let rows = st
            .query_map(params![id], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    // Remember the speed an audiobook was last played at
    pub fn set_speed(&self, id: i64, speed: f64) -> Result<()> {
        if !speed.is_finite() || speed <= 0.0 {
            bail!("a speed is a positive number")
        }
        let changed = self
            .conn()?
            .execute("UPDATE books SET speed=?2 WHERE id=?1", params![id, speed])?;
        if changed == 0 {
            bail!("no book {id}")
        }
        Ok(())
    }

    // The speed an audiobook was last played at, if it has been played
    pub fn speed(&self, id: i64) -> Result<Option<f64>> {
        self.conn()?
            .query_row("SELECT speed FROM books WHERE id=?1", params![id], |r| {
                r.get(0)
            })
            .optional()?
            .with_context(|| format!("no book {id}"))
    }

    // Remember whether this book is read in pages or as one scroll
    pub fn set_mode(&self, id: i64, mode: &str) -> Result<()> {
        if !READING_MODES.contains(&mode) {
            bail!("a reading mode is {}", READING_MODES.join(" or "))
        }
        let changed = self.conn()?.execute(
            "UPDATE books SET reading_mode=?2 WHERE id=?1",
            params![id, mode],
        )?;
        if changed == 0 {
            bail!("no book {id}")
        }
        Ok(())
    }

    // How this book is read, if it has been said
    pub fn mode(&self, id: i64) -> Result<Option<String>> {
        self.conn()?
            .query_row(
                "SELECT reading_mode FROM books WHERE id=?1",
                params![id],
                |r| r.get(0),
            )
            .optional()?
            .with_context(|| format!("no book {id}"))
    }

    // ── Highlights ──

    pub fn add_highlight(
        &self,
        book_id: i64,
        location: &str,
        chapter: Option<&str>,
        quote: &str,
        note: Option<&str>,
        color: &str,
    ) -> Result<i64> {
        if quote.trim().is_empty() || quote.len() > 10_000 {
            bail!("a highlight quotes 1–10000 characters")
        }
        if !COLORS.contains(&color) {
            bail!("colour is one of {}", COLORS.join(", "))
        }
        let c = self.conn()?;
        c.execute(
            "INSERT INTO highlights(book_id,location,chapter,quote,note,color,created_at) SELECT id,?2,?3,?4,?5,?6,?7 FROM books WHERE id=?1",
            params![book_id, location, chapter, quote.trim(), note.map(str::trim).filter(|n| !n.is_empty()), color, Utc::now().to_rfc3339()],
        )?;
        if c.changes() == 0 {
            bail!("no book {book_id}")
        }
        Ok(c.last_insert_rowid())
    }

    pub fn highlights(&self, book_id: i64) -> Result<Vec<Highlight>> {
        let c = self.conn()?;
        let mut st = c.prepare(
            "SELECT id,book_id,location,chapter,quote,note,color,created_at FROM highlights WHERE book_id=?1 ORDER BY id",
        )?;
        let rows = st
            .query_map(params![book_id], highlight_from_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    // Change a highlight's note (empty clears it); returns its book
    pub fn set_note(&self, id: i64, note: &str) -> Result<i64> {
        let c = self.conn()?;
        let note = Some(note.trim()).filter(|n| !n.is_empty());
        if c.execute(
            "UPDATE highlights SET note=?1 WHERE id=?2",
            params![note, id],
        )? != 1
        {
            bail!("no highlight {id}")
        }
        Ok(c.query_row(
            "SELECT book_id FROM highlights WHERE id=?1",
            params![id],
            |r| r.get(0),
        )?)
    }

    // Remove a highlight; returns its book
    pub fn remove_highlight(&self, id: i64) -> Result<i64> {
        let c = self.conn()?;
        let book: i64 = c
            .query_row(
                "SELECT book_id FROM highlights WHERE id=?1",
                params![id],
                |r| r.get(0),
            )
            .optional()?
            .with_context(|| format!("no highlight {id}"))?;
        c.execute("DELETE FROM highlights WHERE id=?1", params![id])?;
        Ok(book)
    }

    // ── Collections ──

    pub fn collect(&self, name: &str, book_id: i64) -> Result<()> {
        let name = name.trim();
        if name.is_empty() || name.len() > 80 {
            bail!("a collection name is 1–80 characters")
        }
        let mut c = self.conn()?;
        let tx = c.transaction()?;
        tx.execute(
            "INSERT INTO collections(name) VALUES (?1) ON CONFLICT(name) DO NOTHING",
            params![name],
        )?;
        let added = tx.execute(
            "INSERT OR IGNORE INTO collection_books(collection_id,book_id) SELECT c.id,b.id FROM collections c, books b WHERE c.name=?1 AND b.id=?2",
            params![name, book_id],
        )?;
        let exists: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM books WHERE id=?1)",
            params![book_id],
            |r| r.get(0),
        )?;
        if !exists {
            bail!("no book {book_id}")
        }
        let _ = added;
        tx.commit()?;
        Ok(())
    }

    pub fn uncollect(&self, name: &str, book_id: i64) -> Result<()> {
        self.conn()?.execute(
            "DELETE FROM collection_books WHERE book_id=?2 AND collection_id=(SELECT id FROM collections WHERE name=?1)",
            params![name, book_id],
        )?;
        Ok(())
    }

    // Each collection with how many books it holds
    pub fn collections(&self) -> Result<Vec<(String, i64)>> {
        let c = self.conn()?;
        let mut st = c.prepare(
            "SELECT c.name, COUNT(cb.book_id) FROM collections c LEFT JOIN collection_books cb ON cb.collection_id=c.id GROUP BY c.id ORDER BY c.name COLLATE NOCASE",
        )?;
        let rows = st
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    pub fn collection(&self, name: &str) -> Result<Vec<Book>> {
        let c = self.conn()?;
        let mut st = c.prepare(&format!(
            "{BOOK_SELECT} JOIN collection_books cb ON cb.book_id=b.id JOIN collections c ON c.id=cb.collection_id WHERE c.name=?1 ORDER BY b.title COLLATE NOCASE"
        ))?;
        let rows = st
            .query_map(params![name], book_from_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn found(root: &str, path: &str, title: &str) -> Found {
        Found {
            kind: "epub".into(),
            root: root.into(),
            path: path.into(),
            title: title.into(),
            ..Default::default()
        }
    }

    fn store() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().unwrap();
        let s = Store::open(dir.path().join("b.sqlite")).unwrap();
        (dir, s)
    }

    #[test]
    fn a_rescan_keeps_history_and_a_vanished_book_waits_as_missing() {
        let (_d, s) = store();
        assert_eq!(
            s.save_scan(
                &["books"],
                &[found("books", "a.epub", "A"), found("books", "b.epub", "B")]
            )
            .unwrap(),
            (2, 0)
        );
        let a = s
            .books(None, false)
            .unwrap()
            .into_iter()
            .find(|b| b.title == "A")
            .unwrap();
        s.set_progress(a.id, "3:0.5", 40.0, None).unwrap();
        s.add_highlight(a.id, "3:0.5", Some("Ch 3"), "a line", None, "yellow")
            .unwrap();
        // a.epub disappears
        assert_eq!(
            s.save_scan(&["books"], &[found("books", "b.epub", "B")])
                .unwrap(),
            (0, 1)
        );
        assert_eq!(s.books(None, false).unwrap().len(), 1);
        assert!(s.book(a.id).unwrap().missing);
        // and comes back with everything intact
        assert_eq!(
            s.save_scan(
                &["books"],
                &[
                    found("books", "a.epub", "A2"),
                    found("books", "b.epub", "B")
                ]
            )
            .unwrap(),
            (0, 0)
        );
        let back = s.book(a.id).unwrap();
        assert_eq!(
            (back.title.as_str(), back.missing, back.location.as_deref()),
            ("A2", false, Some("3:0.5"))
        );
        assert_eq!(s.highlights(a.id).unwrap().len(), 1);
    }

    #[test]
    fn a_speed_is_kept_per_book_and_survives_a_rescan() {
        let (_d, s) = store();
        let roots = [crate::scan::AUDIOBOOKS];
        s.save_scan(&roots, &[found(crate::scan::AUDIOBOOKS, "a.m4b", "A")])
            .unwrap();
        let id = s.books(None, false).unwrap()[0].id;
        assert_eq!(s.speed(id).unwrap(), None);
        s.set_speed(id, 1.75).unwrap();
        s.save_scan(
            &roots,
            &[found(crate::scan::AUDIOBOOKS, "a.m4b", "A again")],
        )
        .unwrap();
        assert_eq!(s.speed(id).unwrap(), Some(1.75));
        assert!(s.set_speed(id, f64::NAN).is_err());
        assert!(s.set_speed(999, 1.0).is_err());
        assert!(s.speed(999).is_err());
    }

    #[test]
    fn a_reading_mode_is_kept_per_book() {
        let (_d, s) = store();
        s.save_scan(&["books"], &[found("books", "a.epub", "A")])
            .unwrap();
        let id = s.books(None, false).unwrap()[0].id;
        assert_eq!(
            s.mode(id).unwrap(),
            None,
            "the reader decides until it is said"
        );
        s.set_mode(id, "scroll").unwrap();
        s.save_scan(&["books"], &[found("books", "a.epub", "A")])
            .unwrap();
        assert_eq!(
            s.mode(id).unwrap().as_deref(),
            Some("scroll"),
            "a rescan keeps it"
        );
        assert!(s.set_mode(id, "sideways").is_err());
        assert!(s.mode(999).is_err());
    }

    #[test]
    fn a_scan_of_one_root_leaves_the_other_alone() {
        let (_d, s) = store();
        s.save_scan(
            &["books", "audiobooks"],
            &[found("books", "a.epub", "A"), found("audiobooks", "x", "X")],
        )
        .unwrap();
        s.save_scan(&["books"], &[found("books", "a.epub", "A")])
            .unwrap();
        assert_eq!(
            s.books(None, false).unwrap().len(),
            2,
            "the audiobook was not in this scan's roots"
        );
    }

    #[test]
    fn continue_reading_is_the_recent_unfinished_ones() {
        let (_d, s) = store();
        s.save_scan(
            &["books"],
            &[
                found("books", "a.epub", "A"),
                found("books", "b.epub", "B"),
                found("books", "c.epub", "C"),
            ],
        )
        .unwrap();
        let ids: Vec<i64> = s.books(None, false).unwrap().iter().map(|b| b.id).collect();
        s.set_progress(ids[0], "1", 10.0, None).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(5));
        s.set_progress(ids[1], "9", 99.8, None).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(5));
        s.set_progress(ids[2], "2", 20.0, None).unwrap();
        let reading: Vec<i64> = s.in_progress(10).unwrap().iter().map(|b| b.id).collect();
        assert_eq!(
            reading,
            [ids[2], ids[0]],
            "newest first; the finished one is gone"
        );
        assert!(s.book(ids[1]).unwrap().finished);
        assert!(s.set_progress(999, "1", 1.0, None).is_err());
    }

    #[test]
    fn highlights_notes_and_collections() {
        let (_d, s) = store();
        s.save_scan(&["books"], &[found("books", "a.epub", "A")])
            .unwrap();
        let id = s.books(None, false).unwrap()[0].id;
        let h = s
            .add_highlight(id, "1:0.1", None, "  quoted  ", Some(" thought "), "green")
            .unwrap();
        assert!(
            s.add_highlight(id, "1", None, "x", None, "chartreuse")
                .is_err()
        );
        assert!(
            s.add_highlight(id, "1", None, "   ", None, "yellow")
                .is_err()
        );
        assert_eq!(s.highlights(id).unwrap()[0].quote, "quoted");
        assert_eq!(s.set_note(h, "").unwrap(), id);
        assert_eq!(s.highlights(id).unwrap()[0].note, None);
        s.collect("Sci-fi", id).unwrap();
        s.collect("Sci-fi", id).unwrap();
        assert_eq!(s.collections().unwrap(), [("Sci-fi".to_string(), 1)]);
        assert_eq!(s.collection("Sci-fi").unwrap()[0].id, id);
        assert!(s.collect("X", 999).is_err());
        s.uncollect("Sci-fi", id).unwrap();
        assert_eq!(s.collections().unwrap()[0].1, 0);
        assert_eq!(s.remove_highlight(h).unwrap(), id);
        assert!(s.remove_highlight(h).is_err());
    }
}
