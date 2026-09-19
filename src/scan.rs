// Author: Jeff
// Date: 2026-09-19
// Description: Walk the book folders and record what is there — the library's only way in
// Notes: Ebooks: EPUB, PDF, CBZ, CBR under the books root. Audiobooks under the audiobooks root:
//        any M4B/M4A file is one book, and any folder holding other audio files is one book
//        (its files are the tracks, and the walk does not go below it). Symlinks are not
//        followed and the walk stops at depth 8, so a loop or a huge tree cannot run away.
//        A book whose metadata cannot be read is still recorded under its file name.
//        Covers are cached under a key made from the book's place, size and change time, so a
//        replaced file gets a fresh cover and an unchanged one is not redone

use std::path::{Path, PathBuf};

use anyhow::Result;
use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::meta::{self, Meta};
use crate::store::{Found, Store};

pub const BOOKS: &str = "books";
pub const AUDIOBOOKS: &str = "audiobooks";
const MAX_DEPTH: usize = 8;
const EBOOK_EXTENSIONS: [&str; 4] = ["epub", "pdf", "cbz", "cbr"];
// single-file audiobooks; other audio files make their folder a book
const BOOK_AUDIO: [&str; 2] = ["m4b", "m4a"];

pub struct Roots {
    pub books: PathBuf,
    pub audiobooks: PathBuf,
}

// $MG_BOOKR_BOOKS and $MG_BOOKR_AUDIOBOOKS, else ~/books and ~/audiobooks
pub fn default_roots() -> Roots {
    let home = dirs::home_dir().unwrap_or_default();
    let from = |var: &str, default: &str| {
        std::env::var_os(var)
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(default))
    };
    Roots {
        books: from("MG_BOOKR_BOOKS", "books"),
        audiobooks: from("MG_BOOKR_AUDIOBOOKS", "audiobooks"),
    }
}

// $XDG_CACHE_HOME/mg-bookr/covers
pub fn default_cover_dir() -> PathBuf {
    dirs::cache_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("mg-bookr/covers")
}

#[derive(Debug, Default, Serialize)]
pub struct Report {
    pub found: usize,
    pub added: usize,
    pub missing: usize,
    // books whose metadata could not be read (recorded anyway): (path, why)
    pub unreadable: Vec<(String, String)>,
}

// One candidate: (kind, path inside the root)
fn walk(root: &Path, audio: bool) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut stack = vec![(root.to_path_buf(), 0usize)];
    while let Some((dir, depth)) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        let mut subdirs = Vec::new();
        let mut folder_audio = false;
        for entry in entries.flatten() {
            // file_type does not follow symlinks: a link is neither a file nor a folder here
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            let Ok(name) = entry.file_name().into_string() else {
                continue;
            };
            if name.starts_with('.') {
                continue;
            }
            let path = entry.path();
            let rel = || {
                path.strip_prefix(root)
                    .ok()
                    .and_then(|p| p.to_str())
                    .map(str::to_string)
            };
            let ext = meta::extension(&name);
            if kind.is_dir() {
                subdirs.push(path.clone());
            } else if kind.is_file() && !audio && EBOOK_EXTENSIONS.contains(&ext.as_str()) {
                let book_kind = match ext.as_str() {
                    "cbz" | "cbr" => "comic",
                    other => other,
                };
                if let Some(rel) = rel() {
                    out.push((book_kind.to_string(), rel));
                }
            } else if kind.is_file() && audio && BOOK_AUDIO.contains(&ext.as_str()) {
                if let Some(rel) = rel() {
                    out.push(("audio".to_string(), rel));
                }
            } else if kind.is_file() && audio && meta::AUDIO_EXTENSIONS.contains(&ext.as_str()) {
                folder_audio = true;
            }
        }
        // a folder of audio files is one book, and nothing below it is looked at
        if folder_audio
            && let Some(rel) = dir
                .strip_prefix(root)
                .ok()
                .and_then(|p| p.to_str())
                .filter(|r| !r.is_empty())
        {
            out.push(("audio-folder".to_string(), rel.to_string()));
            continue;
        }
        if depth < MAX_DEPTH {
            stack.extend(subdirs.into_iter().map(|d| (d, depth + 1)));
        }
    }
    out.sort();
    out
}

// Where a book's cover goes: a key from its place, size and change time
fn cover_key(root: &str, rel: &str, path: &Path) -> String {
    let stamp = std::fs::metadata(path)
        .map(|m| format!("{}:{:?}", m.len(), m.modified().ok()))
        .unwrap_or_default();
    Sha256::digest(format!("{root}/{rel}/{stamp}").as_bytes())
        .iter()
        .take(16)
        .map(|b| format!("{b:02x}"))
        .collect()
}

// Read one book's metadata by kind
fn read(kind: &str, path: &Path) -> Result<Meta> {
    match kind {
        "epub" => meta::epub(path),
        "pdf" => meta::pdf(path),
        "comic" if meta::extension(path.to_str().unwrap_or_default()) == "cbr" => meta::cbr(path),
        "comic" => meta::cbz(path),
        "audio" => meta::audio_file(path),
        _ => meta::audio_folder(path),
    }
}

// Save a cover to the cache (once) and return its path
fn store_cover(kind: &str, path: &Path, meta: &mut Meta, dir: &Path, key: &str) -> Option<String> {
    std::fs::create_dir_all(dir).ok()?;
    // an already cached cover for this exact file is reused
    for ext in ["jpg", "jpeg", "png", "webp", "gif"] {
        let existing = dir.join(format!("{key}.{ext}"));
        if existing.exists() {
            return Some(existing.display().to_string());
        }
    }
    if let Some((bytes, ext)) = meta.cover.take() {
        let ext = if meta::IMAGE_EXTENSIONS.contains(&ext.as_str()) {
            ext
        } else {
            "jpg".into()
        };
        let dest = dir.join(format!("{key}.{ext}"));
        let part = dir.join(format!(".{key}.part"));
        std::fs::write(&part, bytes).ok()?;
        std::fs::rename(&part, &dest).ok()?;
        return Some(dest.display().to_string());
    }
    // a PDF's cover is its first page, drawn by pdftoppm straight into the cache
    if kind == "pdf" && meta::pdf_cover(path, &dir.join(key)).is_ok() {
        let dest = dir.join(format!("{key}.jpg"));
        return dest.exists().then(|| dest.display().to_string());
    }
    None
}

// Walk both roots, read each book, and record the lot; a root that does not exist yet is skipped
pub fn scan(store: &Store, roots: &Roots, cover_dir: &Path) -> Result<Report> {
    let mut report = Report::default();
    let mut found = Vec::new();
    let mut scanned = Vec::new();
    for (root_name, root, audio) in [
        (BOOKS, &roots.books, false),
        (AUDIOBOOKS, &roots.audiobooks, true),
    ] {
        if !root.is_dir() {
            continue;
        }
        scanned.push(root_name);
        for (kind, rel) in walk(root, audio) {
            let path = root.join(&rel);
            let mut meta = read(&kind, &path).unwrap_or_else(|e| {
                report
                    .unreadable
                    .push((format!("{root_name}/{rel}"), format!("{e:#}")));
                Meta::default()
            });
            let key = cover_key(root_name, &rel, &path);
            let cover = store_cover(&kind, &path, &mut meta, cover_dir, &key);
            found.push(Found {
                kind,
                root: root_name.to_string(),
                title: meta
                    .title
                    .clone()
                    .unwrap_or_else(|| meta::title_from_name(&rel)),
                path: rel,
                author: meta.author,
                series: meta.series,
                series_index: meta.series_index,
                cover,
                pages: meta.pages,
                duration_seconds: meta.duration_seconds,
                tracks: meta.tracks,
                chapters: meta.chapters,
            });
        }
    }
    report.found = found.len();
    let (added, missing) = store.save_scan(&scanned, &found)?;
    report.added = added;
    report.missing = missing;
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::meta::tests::{write_cbz, write_epub};

    // A tiny one-page PDF; poppler repairs the missing cross-reference table
    const PDF: &[u8] = b"%PDF-1.4\n1 0 obj<</Type/Catalog/Pages 2 0 R>>endobj\n2 0 obj<</Type/Pages/Kids[3 0 R]/Count 1>>endobj\n3 0 obj<</Type/Page/Parent 2 0 R/MediaBox[0 0 200 200]>>endobj\ntrailer<</Root 1 0 R>>\n%%EOF\n";

    fn tool(name: &str) -> bool {
        std::process::Command::new("sh")
            .args(["-c", &format!("command -v {name}")])
            .output()
            .is_ok_and(|o| o.status.success())
    }

    #[test]
    fn a_scan_finds_every_kind_and_a_second_scan_changes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let (books, audio, covers) = (
            dir.path().join("books"),
            dir.path().join("audiobooks"),
            dir.path().join("covers"),
        );
        std::fs::create_dir_all(books.join("Le Guin")).unwrap();
        std::fs::create_dir_all(audio.join("Dune/disc 1")).unwrap();
        write_epub(&books.join("Le Guin/left-hand.epub"), false);
        write_cbz(&books.join("saga-01.cbz"));
        std::fs::write(books.join("paper.pdf"), PDF).unwrap();
        std::fs::write(books.join("notes.txt"), b"not a book").unwrap();
        std::fs::write(books.join(".hidden.epub"), b"skip").unwrap();
        for n in ["01.mp3", "02.mp3"] {
            std::fs::write(audio.join("Dune/disc 1").join(n), b"x").unwrap();
        }
        std::fs::write(audio.join("single.m4b"), b"x").unwrap();
        // a symlink loop must not be followed
        #[cfg(unix)]
        std::os::unix::fs::symlink(&books, books.join("loop")).unwrap();

        let store = Store::open(dir.path().join("b.sqlite")).unwrap();
        let roots = Roots {
            books: books.clone(),
            audiobooks: audio.clone(),
        };
        let report = scan(&store, &roots, &covers).unwrap();
        let all = store.books(None, false).unwrap();
        let mut kinds: Vec<(String, String)> = all
            .iter()
            .map(|b| (b.kind.clone(), b.path.clone()))
            .collect();
        kinds.sort();
        assert_eq!(
            kinds,
            [
                ("audio".into(), "single.m4b".into()),
                ("audio-folder".into(), "Dune/disc 1".into()),
                ("comic".into(), "saga-01.cbz".into()),
                ("epub".into(), "Le Guin/left-hand.epub".into()),
                ("pdf".into(), "paper.pdf".into()),
            ]
        );
        assert_eq!((report.found, report.added, report.missing), (5, 5, 0));
        let epub = all.iter().find(|b| b.kind == "epub").unwrap();
        assert_eq!(epub.title, "The Left Hand of Darkness");
        assert!(
            epub.cover
                .as_ref()
                .is_some_and(|c| std::path::Path::new(c).exists())
        );
        let folder = all.iter().find(|b| b.kind == "audio-folder").unwrap();
        assert_eq!(store.tracks(folder.id).unwrap().len(), 2);
        if tool("pdfinfo") {
            let pdf = all.iter().find(|b| b.kind == "pdf").unwrap();
            assert_eq!(pdf.pages, Some(1));
        }
        let again = scan(&store, &roots, &covers).unwrap();
        assert_eq!((again.added, again.missing), (0, 0));

        // the comic moves away: missing, not gone
        std::fs::remove_file(books.join("saga-01.cbz")).unwrap();
        assert_eq!(scan(&store, &roots, &covers).unwrap().missing, 1);
        assert_eq!(store.books(None, true).unwrap().len(), 5);
    }

    #[test]
    fn a_root_that_does_not_exist_is_skipped_not_emptied() {
        let dir = tempfile::tempdir().unwrap();
        let books = dir.path().join("books");
        std::fs::create_dir_all(&books).unwrap();
        write_epub(&books.join("a.epub"), true);
        let store = Store::open(dir.path().join("b.sqlite")).unwrap();
        let roots = Roots {
            books,
            audiobooks: dir.path().join("nope"),
        };
        let report = scan(&store, &roots, &dir.path().join("covers")).unwrap();
        assert_eq!((report.found, report.missing), (1, 0));
    }
}
