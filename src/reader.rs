// Author: Jeff
// Date: 2026-09-19
// Description: What the reader window needs to open a book — unpacked EPUB, comic pages, file paths
// Notes: EPUBs are unpacked once into $XDG_CACHE_HOME/mg-bookr/unpacked/<key>/ (the key changes
//        when the file does). The reader loads those files into WebEngine as file:// pages, so:
//        - an entry that would land outside the folder (`..`, absolute) is refused (zip-slip),
//          and the unpacked size, entry count and single-entry size are capped;
//        - every page gets a Content-Security-Policy that allows the book's own styles, images
//          and fonts and nothing else: no scripts from the book, nothing from the network.
//          The reader's own pagination/highlight code runs in WebEngine's separate script world,
//          which a page's policy does not govern.
//        Comics are extracted the same way (CBZ with the zip crate, CBR with bsdtar, which
//        refuses `..` and absolute paths itself). Locations: EPUB "spine:fraction", PDF and
//        comics "page", audio "track:seconds"

use std::io::Read;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::meta;
use crate::store::{Book, Store};

const MAX_UNPACKED_BYTES: u64 = 400 * 1024 * 1024;
const MAX_ENTRIES: usize = 10_000;
const MAX_ENTRY_BYTES: u64 = 100 * 1024 * 1024;
// marks a finished unpack, so a half-done one (crash, full disk) is redone
const DONE_MARKER: &str = ".mg-bookr-complete";
// the book's own styles, images and fonts; no scripts, no network, no frames
const PAGE_POLICY: &str = "<meta http-equiv=\"Content-Security-Policy\" content=\"default-src 'none'; \
img-src file: data:; style-src file: 'unsafe-inline'; font-src file: data:; media-src file:\"/>";
const PAGE_EXTENSIONS: [&str; 3] = ["xhtml", "html", "htm"];

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TocEntry {
    pub title: String,
    pub level: usize,
    // which spine item it points into, when it points into one
    pub spine: Option<usize>,
    pub fragment: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Epub {
    pub dir: String,
    // absolute paths of the reading-order pages
    pub spine: Vec<String>,
    pub toc: Vec<TocEntry>,
}

#[derive(Debug, Serialize)]
pub struct Plan {
    pub book: Book,
    pub file: String,
    pub epub: Option<Epub>,
    pub pages: Vec<String>,
    pub tracks: Vec<String>,
    pub chapters: Vec<(String, f64)>,
}

// $XDG_CACHE_HOME/mg-bookr/unpacked
pub fn default_unpack_dir() -> PathBuf {
    dirs::cache_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("mg-bookr/unpacked")
}

// A folder name that changes when the book file changes
fn unpack_key(path: &Path) -> String {
    let stamp = std::fs::metadata(path)
        .map(|m| format!("{}:{:?}", m.len(), m.modified().ok()))
        .unwrap_or_default();
    Sha256::digest(format!("{}/{stamp}", path.display()).as_bytes())
        .iter()
        .take(16)
        .map(|b| format!("{b:02x}"))
        .collect()
}

// Add the page policy right after <head …>; a page with no head gets one
fn with_policy(page: &str) -> String {
    let lower = page.to_ascii_lowercase();
    if let Some(open) = lower
        .find("<head")
        .and_then(|at| lower[at..].find('>').map(|end| at + end + 1))
    {
        return format!("{}{PAGE_POLICY}{}", &page[..open], &page[open..]);
    }
    if let Some(open) = lower
        .find("<html")
        .and_then(|at| lower[at..].find('>').map(|end| at + end + 1))
    {
        return format!(
            "{}<head>{PAGE_POLICY}</head>{}",
            &page[..open],
            &page[open..]
        );
    }
    format!("{PAGE_POLICY}{page}")
}

// Unpack a zip (EPUB or CBZ) into `dest` with every guard; pages get the policy
fn unpack_zip(path: &Path, dest: &Path, add_policy: bool) -> Result<()> {
    let mut archive = zip::ZipArchive::new(std::fs::File::open(path)?).context("not a zip file")?;
    if archive.len() > MAX_ENTRIES {
        bail!("more than {MAX_ENTRIES} files inside")
    }
    let mut total = 0u64;
    for i in 0..archive.len() {
        let mut entry = archive.by_index(i)?;
        // enclosed_name is None for `..` or absolute names: those never touch the disk
        let Some(rel) = entry.enclosed_name() else {
            bail!("{} would land outside the book's folder", entry.name())
        };
        let out = dest.join(rel);
        if entry.is_dir() {
            std::fs::create_dir_all(&out)?;
            continue;
        }
        if entry.size() > MAX_ENTRY_BYTES {
            bail!("{} is larger than {MAX_ENTRY_BYTES} bytes", entry.name())
        }
        total += entry.size();
        if total > MAX_UNPACKED_BYTES {
            bail!("the book unpacks to more than {MAX_UNPACKED_BYTES} bytes")
        }
        if let Some(parent) = out.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut bytes = Vec::new();
        (&mut entry)
            .take(MAX_ENTRY_BYTES + 1)
            .read_to_end(&mut bytes)?;
        if add_policy && PAGE_EXTENSIONS.contains(&meta::extension(entry.name()).as_str()) {
            bytes = with_policy(&String::from_utf8_lossy(&bytes)).into_bytes();
        }
        std::fs::write(&out, bytes)?;
    }
    Ok(())
}

// Unpack into the cache once; a finished unpack is reused until the file changes
fn unpacked(path: &Path, cache: &Path, add_policy: bool, cbr: bool) -> Result<PathBuf> {
    let dir = cache.join(unpack_key(path));
    if dir.join(DONE_MARKER).exists() {
        return Ok(dir);
    }
    // a half-done folder from a crash is thrown away and redone
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir)?;
    let result = if cbr {
        let (file, into) = (
            path.to_str().context("path is not text")?,
            dir.to_str().context("path is not text")?,
        );
        crate::tools::run(
            &["bsdtar", "-x", "-f", file, "-C", into],
            std::time::Duration::from_secs(120),
            1 << 20,
        )
        .map(|_| ())
    } else {
        unpack_zip(path, &dir, add_policy)
    };
    if let Err(e) = result {
        let _ = std::fs::remove_dir_all(&dir);
        return Err(e);
    }
    std::fs::write(dir.join(DONE_MARKER), b"")?;
    Ok(dir)
}

// Join an href to the folder of the file it appears in, as a path inside the book
fn resolve(base_dir: &str, href: &str) -> (String, Option<String>) {
    let (path, fragment) = match href.split_once('#') {
        Some((p, f)) => (p, Some(f.to_string())),
        None => (href, None),
    };
    let mut parts: Vec<&str> = base_dir.split('/').filter(|p| !p.is_empty()).collect();
    for piece in path.split('/') {
        match piece {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            p => parts.push(p),
        }
    }
    (parts.join("/"), fragment)
}

// The table of contents: EPUB 3 nav document first, else the EPUB 2 NCX
fn toc(dir: &Path, opf_dir: &str, doc: &roxmltree::Document, spine: &[String]) -> Vec<TocEntry> {
    let items: Vec<roxmltree::Node> = doc
        .descendants()
        .filter(|n| n.tag_name().name() == "item")
        .collect();
    let index_of = |path: &str| spine.iter().position(|s| s == path);
    let nav = items.iter().find(|n| {
        n.attribute("properties")
            .is_some_and(|p| p.split_whitespace().any(|w| w == "nav"))
    });
    if let Some(href) = nav.and_then(|n| n.attribute("href")) {
        let (nav_path, _) = resolve(opf_dir, href);
        let nav_dir = nav_path.rsplit_once('/').map_or("", |(d, _)| d).to_string();
        if let Ok(xml) = std::fs::read_to_string(dir.join(&nav_path))
            && let Ok(nav_doc) = roxmltree::Document::parse(&xml)
        {
            // the <nav> whose epub:type is toc; its links, with depth from nested lists
            let toc_nav = nav_doc.descendants().find(|n| {
                n.tag_name().name() == "nav"
                    && n.attributes().any(|a| {
                        a.name() == "type" && a.value().split_whitespace().any(|w| w == "toc")
                    })
            });
            if let Some(toc_nav) = toc_nav {
                return toc_nav
                    .descendants()
                    .filter(|n| n.tag_name().name() == "a")
                    .filter_map(|a| {
                        let (path, fragment) = resolve(&nav_dir, a.attribute("href")?);
                        let title = a
                            .descendants()
                            // text nodes only: an element's text() repeats its first child's
                            .filter(|t| t.is_text())
                            .filter_map(|t| t.text())
                            .collect::<String>()
                            .split_whitespace()
                            .collect::<Vec<_>>()
                            .join(" ");
                        let level = a
                            .ancestors()
                            .filter(|n| n.tag_name().name() == "ol")
                            .count()
                            .max(1);
                        Some(TocEntry {
                            title,
                            level,
                            spine: index_of(&path),
                            fragment,
                        })
                    })
                    .collect();
            }
        }
    }
    // EPUB 2: <spine toc="ncx-id"> → the NCX's navPoints
    let ncx_id = doc
        .descendants()
        .find(|n| n.tag_name().name() == "spine")
        .and_then(|n| n.attribute("toc"));
    let ncx = ncx_id
        .and_then(|id| items.iter().find(|n| n.attribute("id") == Some(id)))
        .and_then(|n| n.attribute("href"));
    let Some(ncx_href) = ncx else {
        return Vec::new();
    };
    let (ncx_path, _) = resolve(opf_dir, ncx_href);
    let ncx_dir = ncx_path.rsplit_once('/').map_or("", |(d, _)| d).to_string();
    let Ok(xml) = std::fs::read_to_string(dir.join(&ncx_path)) else {
        return Vec::new();
    };
    let Ok(ncx_doc) = roxmltree::Document::parse(&xml) else {
        return Vec::new();
    };
    ncx_doc
        .descendants()
        .filter(|n| n.tag_name().name() == "navPoint")
        .filter_map(|point| {
            let title = point
                .children()
                .find(|c| c.tag_name().name() == "navLabel")?
                .descendants()
                .find(|t| t.tag_name().name() == "text")?
                .text()?
                .trim()
                .to_string();
            let src = point
                .children()
                .find(|c| c.tag_name().name() == "content")?
                .attribute("src")?;
            let (path, fragment) = resolve(&ncx_dir, src);
            let level = point
                .ancestors()
                .filter(|n| n.tag_name().name() == "navPoint")
                .count();
            Some(TocEntry {
                title,
                level,
                spine: index_of(&path),
                fragment,
            })
        })
        .collect()
}

// Unpack an EPUB and read its reading order and contents
pub fn epub(path: &Path, cache: &Path) -> Result<Epub> {
    let dir = unpacked(path, cache, true, false)?;
    let container = std::fs::read_to_string(dir.join("META-INF/container.xml"))
        .context("no META-INF/container.xml")?;
    let container_doc = roxmltree::Document::parse(&container)?;
    let opf = container_doc
        .descendants()
        .find(|n| n.tag_name().name() == "rootfile")
        .and_then(|n| n.attribute("full-path"))
        .context("container.xml names no package file")?
        .to_string();
    let opf_dir = opf.rsplit_once('/').map_or("", |(d, _)| d).to_string();
    let xml = std::fs::read_to_string(dir.join(&opf)).context("the package file is missing")?;
    let doc = roxmltree::Document::parse(&xml)?;
    let manifest: Vec<(String, String)> = doc
        .descendants()
        .filter(|n| n.tag_name().name() == "item")
        .filter_map(|n| {
            Some((
                n.attribute("id")?.to_string(),
                resolve(&opf_dir, n.attribute("href")?).0,
            ))
        })
        .collect();
    let spine: Vec<String> = doc
        .descendants()
        .filter(|n| n.tag_name().name() == "itemref" && n.attribute("linear") != Some("no"))
        .filter_map(|n| n.attribute("idref"))
        .filter_map(|id| {
            manifest
                .iter()
                .find(|(mid, _)| mid == id)
                .map(|(_, p)| p.clone())
        })
        .collect();
    if spine.is_empty() {
        bail!("the book has no pages in its reading order")
    }
    let toc = toc(&dir, &opf_dir, &doc, &spine);
    Ok(Epub {
        dir: dir.display().to_string(),
        spine: spine
            .iter()
            .map(|p| dir.join(p).display().to_string())
            .collect(),
        toc,
    })
}

// Extract a comic's pages and list them in reading order
pub fn comic_pages(path: &Path, cache: &Path) -> Result<Vec<String>> {
    let cbr = meta::extension(path.to_str().unwrap_or_default()) == "cbr";
    let dir = unpacked(path, cache, false, cbr)?;
    let mut pages: Vec<PathBuf> = Vec::new();
    let mut stack = vec![dir.clone()];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d)?.flatten() {
            let p = e.path();
            let name = e.file_name().to_string_lossy().to_string();
            if e.file_type()?.is_dir() && name != "__MACOSX" {
                stack.push(p);
            } else if e.file_type()?.is_file()
                && meta::IMAGE_EXTENSIONS.contains(&meta::extension(&name).as_str())
            {
                pages.push(p);
            }
        }
    }
    let mut pages: Vec<String> = pages.iter().map(|p| p.display().to_string()).collect();
    pages.sort_by(|a, b| meta::natural_cmp(a, b));
    Ok(pages)
}

// Everything the reader or player needs for one book
pub fn plan(store: &Store, id: i64, roots: &crate::scan::Roots, cache: &Path) -> Result<Plan> {
    let book = store.book(id)?;
    if book.missing {
        bail!("{} is missing from its folder", book.title)
    }
    let root = if book.root == crate::scan::AUDIOBOOKS {
        &roots.audiobooks
    } else {
        &roots.books
    };
    let file = root.join(&book.path);
    let mut plan = Plan {
        file: file.display().to_string(),
        epub: None,
        pages: Vec::new(),
        tracks: Vec::new(),
        chapters: store.chapters(id)?,
        book,
    };
    match plan.book.kind.as_str() {
        "epub" => plan.epub = Some(epub(&file, cache)?),
        "comic" => plan.pages = comic_pages(&file, cache)?,
        "audio" => plan.tracks = vec![file.display().to_string()],
        "audio-folder" => {
            plan.tracks = store
                .tracks(id)?
                .into_iter()
                .map(|(t, _)| file.join(t).display().to_string())
                .collect()
        }
        _ => {}
    }
    Ok(plan)
}

// ── The reader window ──

// The reader program: $MG_BOOKR_READER, else this repository's reader/reader.py
pub fn reader_program() -> PathBuf {
    std::env::var_os("MG_BOOKR_READER")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            let root = std::env::var_os("GEIST_ROOT")
                .map(PathBuf::from)
                .unwrap_or_else(|| {
                    dirs::home_dir()
                        .unwrap_or_default()
                        .join("geistos/mg-suite")
                });
            root.join("mg-bookr/reader/reader.py")
        })
}

// Open a book in the reader window and leave it running on its own.
// The reader is a separate Qt program (PySide6) rather than part of the shell: WebEngine needs
// the argument list a real application has, and inside Quickshell it dies at once without it.
// It talks back through mg-bookr alone, so the window never touches the database
pub fn open(book: &Book) -> Result<()> {
    if book.missing {
        bail!("{} is missing from its folder", book.title)
    }
    let program = reader_program();
    if !program.is_file() {
        bail!("the reader window is not at {}", program.display())
    }
    let python =
        std::env::var_os("MG_BOOKR_PYTHON").map_or_else(|| PathBuf::from("python3"), PathBuf::from);
    std::process::Command::new(&python)
        .arg(&program)
        .arg("--book")
        .arg(book.id.to_string())
        .arg("--bookr")
        .arg(std::env::current_exe()?)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        // its own process group: closing the terminal that opened the book does not close it
        .process_group(0)
        .spawn()
        .with_context(|| format!("{} could not run the reader", python.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    // An EPUB 3 with a nav table of contents, a script, and one page without a head
    fn write_book(path: &Path, evil: bool) {
        let mut zip = zip::ZipWriter::new(std::fs::File::create(path).unwrap());
        let o = zip::write::SimpleFileOptions::default();
        let mut put = |name: &str, body: &str| {
            zip.start_file(name, o).unwrap();
            zip.write_all(body.as_bytes()).unwrap();
        };
        put(
            "META-INF/container.xml",
            r#"<?xml version="1.0"?><container xmlns="urn:oasis:names:tc:opendocument:xmlns:container" version="1.0"><rootfiles><rootfile full-path="OPS/book.opf"/></rootfiles></container>"#,
        );
        put(
            "OPS/book.opf",
            r#"<?xml version="1.0"?><package xmlns="http://www.idpf.org/2007/opf" version="3.0"><metadata/><manifest><item id="nav" href="nav.xhtml" properties="nav"/><item id="a" href="text/a.xhtml"/><item id="b" href="text/b.xhtml"/><item id="n" href="notes.xhtml"/></manifest><spine><itemref idref="a"/><itemref idref="b"/><itemref idref="n" linear="no"/></spine></package>"#,
        );
        put(
            "OPS/nav.xhtml",
            r#"<?xml version="1.0"?><html xmlns="http://www.w3.org/1999/xhtml" xmlns:epub="http://www.idpf.org/2007/ops"><body><nav epub:type="toc"><ol><li><a href="text/a.xhtml">Part One</a><ol><li><a href="text/b.xhtml#s2">  Section
            Two</a></li></ol></li></ol></nav></body></html>"#,
        );
        put(
            "OPS/text/a.xhtml",
            r#"<html xmlns="http://www.w3.org/1999/xhtml"><HEAD><title>a</title></HEAD><body><script>fetch("https://evil.example")</script><p>A</p></body></html>"#,
        );
        put(
            "OPS/text/b.xhtml",
            r#"<html xmlns="http://www.w3.org/1999/xhtml"><body><p id="s2">B</p></body></html>"#,
        );
        put("OPS/notes.xhtml", "<p>notes</p>");
        if evil {
            put("../../escape.txt", "gotcha");
        }
        zip.finish().unwrap();
    }

    #[test]
    fn an_epub_unpacks_with_its_order_contents_and_page_policy() {
        let dir = tempfile::tempdir().unwrap();
        let book = dir.path().join("b.epub");
        write_book(&book, false);
        let e = epub(&book, &dir.path().join("cache")).unwrap();
        assert_eq!(
            e.spine.len(),
            2,
            "the non-linear notes page is not in the reading order"
        );
        assert!(e.spine[0].ends_with("OPS/text/a.xhtml"));
        assert_eq!(e.toc.len(), 2);
        assert_eq!(
            (e.toc[0].title.as_str(), e.toc[0].level, e.toc[0].spine),
            ("Part One", 1, Some(0))
        );
        assert_eq!(
            (
                e.toc[1].title.as_str(),
                e.toc[1].level,
                e.toc[1].spine,
                e.toc[1].fragment.as_deref()
            ),
            ("Section Two", 2, Some(1), Some("s2"))
        );
        let a = std::fs::read_to_string(&e.spine[0]).unwrap();
        assert!(
            a.contains("<HEAD><meta http-equiv=\"Content-Security-Policy\""),
            "the policy follows the head, whatever its case"
        );
        assert!(a.contains("default-src 'none'"));
        let b = std::fs::read_to_string(&e.spine[1]).unwrap();
        assert!(
            b.contains("<head><meta http-equiv"),
            "a page with no head gets one"
        );
        // a second open reuses the unpacked folder
        assert_eq!(epub(&book, &dir.path().join("cache")).unwrap(), e);
    }

    #[test]
    fn an_entry_that_climbs_out_is_refused_and_nothing_is_left_behind() {
        let dir = tempfile::tempdir().unwrap();
        let book = dir.path().join("evil.epub");
        write_book(&book, true);
        let cache = dir.path().join("cache");
        assert!(epub(&book, &cache).is_err());
        assert!(
            !dir.path().join("escape.txt").exists()
                && !dir.path().parent().unwrap().join("escape.txt").exists()
        );
        assert_eq!(
            std::fs::read_dir(&cache).unwrap().count(),
            0,
            "the half-unpacked folder is removed"
        );
    }

    #[test]
    fn comic_pages_extract_in_reading_order() {
        let dir = tempfile::tempdir().unwrap();
        let book = dir.path().join("c.cbz");
        crate::meta::tests::write_cbz(&book);
        let pages = comic_pages(&book, &dir.path().join("cache")).unwrap();
        let names: Vec<&str> = pages
            .iter()
            .map(|p| p.rsplit('/').next().unwrap())
            .collect();
        assert_eq!(
            names,
            ["page1.png", "page2.png", "page10.png"],
            "__MACOSX copies and text files are left out"
        );
    }
}
