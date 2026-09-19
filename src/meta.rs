// Author: Jeff
// Date: 2026-09-19
// Description: What a book says about itself — title, author, series, pages, length, chapters, cover
// Notes: EPUB and CBZ are zip files read here; PDF, CBR and audio go through helper programs
//        (tools::run: argv only, time-limited, output-capped). Every read is size-capped: a
//        cover over 10 MB or a zip entry over its cap is skipped, not loaded. Anything that
//        cannot be read falls back to what the file name says — a book is never dropped for
//        bad metadata

use std::cmp::Ordering;
use std::io::Read;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, bail};

use crate::tools;

pub const MAX_COVER_BYTES: usize = 10 * 1024 * 1024;
// an OPF or container.xml larger than this is not a real one
const MAX_XML_BYTES: u64 = 4 * 1024 * 1024;
const TOOL_TIMEOUT: Duration = Duration::from_secs(20);
const MAX_TOOL_OUTPUT: usize = 4 * 1024 * 1024;
// a folder audiobook with more files than this is probably not one book
const MAX_TRACKS: usize = 500;
pub const IMAGE_EXTENSIONS: [&str; 5] = ["jpg", "jpeg", "png", "webp", "gif"];
pub const AUDIO_EXTENSIONS: [&str; 7] = ["mp3", "m4a", "m4b", "ogg", "opus", "flac", "aac"];

#[derive(Debug, Default, Clone, PartialEq)]
pub struct Meta {
    pub title: Option<String>,
    pub author: Option<String>,
    pub series: Option<String>,
    pub series_index: Option<f64>,
    pub pages: Option<i64>,
    pub duration_seconds: Option<f64>,
    // (bytes, file extension)
    pub cover: Option<(Vec<u8>, String)>,
    pub tracks: Vec<(String, Option<f64>)>,
    pub chapters: Vec<(String, f64)>,
}

// Lower-cased extension of a path or entry name
pub fn extension(name: &str) -> String {
    name.rsplit_once('.')
        .map(|(_, e)| e.to_ascii_lowercase())
        .unwrap_or_default()
}

// Compare names the way people count: "page2" before "page10"
pub fn natural_cmp(a: &str, b: &str) -> Ordering {
    let (mut x, mut y) = (a.chars().peekable(), b.chars().peekable());
    loop {
        match (x.peek(), y.peek()) {
            (None, None) => return Ordering::Equal,
            (None, _) => return Ordering::Less,
            (_, None) => return Ordering::Greater,
            (Some(c), Some(d)) if c.is_ascii_digit() && d.is_ascii_digit() => {
                let mut n = String::new();
                while let Some(c) = x.peek().filter(|c| c.is_ascii_digit()) {
                    n.push(*c);
                    x.next();
                }
                let mut m = String::new();
                while let Some(d) = y.peek().filter(|d| d.is_ascii_digit()) {
                    m.push(*d);
                    y.next();
                }
                // compare as numbers without parsing (they may be longer than any integer)
                let (n, m) = (n.trim_start_matches('0'), m.trim_start_matches('0'));
                match n.len().cmp(&m.len()).then_with(|| n.cmp(m)) {
                    Ordering::Equal => {}
                    other => return other,
                }
            }
            (Some(c), Some(d)) => match c.to_ascii_lowercase().cmp(&d.to_ascii_lowercase()) {
                Ordering::Equal => {
                    x.next();
                    y.next();
                }
                other => return other,
            },
        }
    }
}

// A title from a file name: "the_left_hand-of.darkness.epub" → "the left hand of.darkness"
pub fn title_from_name(name: &str) -> String {
    let stem = name.rsplit('/').next().unwrap_or(name);
    let stem = stem.rsplit_once('.').map_or(stem, |(s, _)| s);
    stem.replace(['_', '-'], " ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

// One zip entry's bytes, refusing anything over `cap`
fn zip_entry(
    archive: &mut zip::ZipArchive<std::fs::File>,
    name: &str,
    cap: u64,
) -> Result<Vec<u8>> {
    let entry = archive
        .by_name(name)
        .with_context(|| format!("{name} is not in the archive"))?;
    if entry.size() > cap {
        bail!("{name} is larger than {cap} bytes")
    }
    let mut bytes = Vec::with_capacity(entry.size() as usize);
    entry.take(cap + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > cap {
        bail!("{name} is larger than {cap} bytes")
    }
    Ok(bytes)
}

// Resolve an href relative to the OPF's folder ("OEBPS/content.opf" + "images/c.jpg")
fn resolve(base_dir: &str, href: &str) -> String {
    let mut parts: Vec<&str> = base_dir.split('/').filter(|p| !p.is_empty()).collect();
    for piece in href.split('#').next().unwrap_or(href).split('/') {
        match piece {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            p => parts.push(p),
        }
    }
    parts.join("/")
}

// ── EPUB ──

// The OPF (package file) path inside an EPUB, from META-INF/container.xml
pub fn opf_path(archive: &mut zip::ZipArchive<std::fs::File>) -> Result<String> {
    let xml = String::from_utf8(zip_entry(archive, "META-INF/container.xml", MAX_XML_BYTES)?)?;
    let doc = roxmltree::Document::parse(&xml).context("container.xml is not XML")?;
    doc.descendants()
        .find(|n| n.tag_name().name() == "rootfile")
        .and_then(|n| n.attribute("full-path"))
        .map(str::to_string)
        .context("container.xml names no package file")
}

pub fn epub(path: &Path) -> Result<Meta> {
    let mut archive = zip::ZipArchive::new(std::fs::File::open(path)?).context("not a zip file")?;
    let opf = opf_path(&mut archive)?;
    let xml = String::from_utf8(zip_entry(&mut archive, &opf, MAX_XML_BYTES)?)?;
    let doc = roxmltree::Document::parse(&xml).context("the package file is not XML")?;
    let base = opf.rsplit_once('/').map_or("", |(dir, _)| dir);
    let text_of = |local: &str| {
        doc.descendants()
            .filter(|n| {
                n.tag_name().name() == local
                    && n.tag_name()
                        .namespace()
                        .is_some_and(|ns| ns.contains("purl.org/dc"))
            })
            .filter_map(|n| {
                n.text()
                    .map(|t| t.split_whitespace().collect::<Vec<_>>().join(" "))
            })
            .filter(|t| !t.is_empty())
            .collect::<Vec<_>>()
    };
    let meta_named = |name: &str| {
        doc.descendants()
            .find(|n| n.tag_name().name() == "meta" && n.attribute("name") == Some(name))
            .and_then(|n| n.attribute("content"))
            .map(str::to_string)
    };
    let mut meta = Meta {
        title: text_of("title").into_iter().next(),
        author: Some(text_of("creator").join(", ")).filter(|a| !a.is_empty()),
        series: meta_named("calibre:series"),
        series_index: meta_named("calibre:series_index").and_then(|v| v.parse().ok()),
        ..Default::default()
    };
    // EPUB 3 series: <meta property="belongs-to-collection" id="c">Name</meta> refined by group-position
    if meta.series.is_none()
        && let Some(collection) = doc.descendants().find(|n| {
            n.tag_name().name() == "meta"
                && n.attribute("property") == Some("belongs-to-collection")
        })
    {
        meta.series = collection.text().map(str::trim).map(str::to_string);
        let refines = collection.attribute("id").map(|id| format!("#{id}"));
        meta.series_index = doc
            .descendants()
            .find(|n| {
                n.attribute("property") == Some("group-position")
                    && n.attribute("refines").map(str::to_string) == refines
            })
            .and_then(|n| n.text())
            .and_then(|t| t.trim().parse().ok());
    }
    // the cover: an item marked cover-image (EPUB 3), else <meta name="cover"> pointing at an item id
    let items: Vec<roxmltree::Node> = doc
        .descendants()
        .filter(|n| n.tag_name().name() == "item")
        .collect();
    let by_property = items.iter().find(|n| {
        n.attribute("properties")
            .is_some_and(|p| p.split_whitespace().any(|w| w == "cover-image"))
    });
    let by_meta = meta_named("cover").and_then(|id| {
        items
            .iter()
            .find(|n| n.attribute("id") == Some(id.as_str()))
    });
    if let Some(href) = by_property.or(by_meta).and_then(|n| n.attribute("href")) {
        let name = resolve(base, href);
        if let Ok(bytes) = zip_entry(&mut archive, &name, MAX_COVER_BYTES as u64) {
            meta.cover = Some((bytes, extension(&name)));
        }
    }
    Ok(meta)
}

// ── Comics ──

// Image entries of a CBZ in reading order
pub fn cbz_pages(archive: &zip::ZipArchive<std::fs::File>) -> Vec<String> {
    let mut pages: Vec<String> = archive
        .file_names()
        .filter(|n| {
            !n.ends_with('/')
                && !n.contains("__MACOSX")
                && IMAGE_EXTENSIONS.contains(&extension(n).as_str())
        })
        .map(str::to_string)
        .collect();
    pages.sort_by(|a, b| natural_cmp(a, b));
    pages
}

pub fn cbz(path: &Path) -> Result<Meta> {
    let mut archive = zip::ZipArchive::new(std::fs::File::open(path)?).context("not a zip file")?;
    let pages = cbz_pages(&archive);
    let cover = pages.first().and_then(|first| {
        zip_entry(&mut archive, first, MAX_COVER_BYTES as u64)
            .ok()
            .map(|b| (b, extension(first)))
    });
    Ok(Meta {
        pages: Some(pages.len() as i64),
        cover,
        ..Default::default()
    })
}

// Image entries of a CBR in reading order, listed by bsdtar
pub fn cbr_pages(path: &Path) -> Result<Vec<String>> {
    let file = path.to_str().context("the path is not text")?;
    let listing = tools::run(&["bsdtar", "-tf", file], TOOL_TIMEOUT, MAX_TOOL_OUTPUT)?;
    let mut pages: Vec<String> = String::from_utf8_lossy(&listing)
        .lines()
        .filter(|n| !n.ends_with('/') && IMAGE_EXTENSIONS.contains(&extension(n).as_str()))
        .map(str::to_string)
        .collect();
    pages.sort_by(|a, b| natural_cmp(a, b));
    Ok(pages)
}

// One CBR page's bytes
pub fn cbr_page(path: &Path, entry: &str) -> Result<Vec<u8>> {
    let file = path.to_str().context("the path is not text")?;
    // `--` so an entry named like an option is still just a name
    tools::run(
        &["bsdtar", "-xOf", file, "--", entry],
        TOOL_TIMEOUT,
        MAX_COVER_BYTES,
    )
}

pub fn cbr(path: &Path) -> Result<Meta> {
    let pages = cbr_pages(path)?;
    let cover = pages
        .first()
        .and_then(|first| cbr_page(path, first).ok().map(|b| (b, extension(first))));
    Ok(Meta {
        pages: Some(pages.len() as i64),
        cover,
        ..Default::default()
    })
}

// ── PDF ──

pub fn pdf(path: &Path) -> Result<Meta> {
    let file = path.to_str().context("the path is not text")?;
    let info = String::from_utf8_lossy(&tools::run(
        &["pdfinfo", "--", file],
        TOOL_TIMEOUT,
        MAX_TOOL_OUTPUT,
    )?)
    .into_owned();
    let field = |key: &str| {
        info.lines()
            .find_map(|l| l.strip_prefix(key)?.strip_prefix(':'))
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
    };
    Ok(Meta {
        title: field("Title"),
        author: field("Author"),
        pages: field("Pages").and_then(|p| p.parse().ok()),
        ..Default::default()
    })
}

// Render a PDF's first page as a JPEG cover at `dest` (without extension; pdftoppm adds .jpg)
pub fn pdf_cover(path: &Path, dest_stem: &Path) -> Result<()> {
    let file = path.to_str().context("the path is not text")?;
    let dest = dest_stem.to_str().context("the path is not text")?;
    tools::run(
        &[
            "pdftoppm",
            "-f",
            "1",
            "-l",
            "1",
            "-singlefile",
            "-jpeg",
            "-scale-to",
            "480",
            "--",
            file,
            dest,
        ],
        TOOL_TIMEOUT,
        MAX_TOOL_OUTPUT,
    )?;
    Ok(())
}

// ── Audio ──

// What ffprobe says about one audio file
struct Probe {
    title: Option<String>,
    artist: Option<String>,
    album: Option<String>,
    duration: Option<f64>,
    chapters: Vec<(String, f64)>,
}

// ffprobe's view of one file
fn probe(path: &Path) -> Result<Probe> {
    let file = path.to_str().context("the path is not text")?;
    let out = tools::run(
        &[
            "ffprobe",
            "-v",
            "quiet",
            "-print_format",
            "json",
            "-show_format",
            "-show_chapters",
            "--",
            file,
        ],
        TOOL_TIMEOUT,
        MAX_TOOL_OUTPUT,
    )?;
    let v: serde_json::Value =
        serde_json::from_slice(&out).context("ffprobe did not answer JSON")?;
    let tag = |key: &str| {
        let tags = &v["format"]["tags"];
        [key, &key.to_ascii_uppercase()]
            .iter()
            .find_map(|k| tags[*k].as_str())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    };
    let duration = v["format"]["duration"]
        .as_str()
        .and_then(|d| d.parse().ok());
    let chapters = v["chapters"]
        .as_array()
        .map(|list| {
            list.iter()
                .enumerate()
                .filter_map(|(i, c)| {
                    let start: f64 = c["start_time"].as_str()?.parse().ok()?;
                    let title = c["tags"]["title"]
                        .as_str()
                        .map(str::to_string)
                        .unwrap_or_else(|| format!("Chapter {}", i + 1));
                    Some((title, start))
                })
                .collect()
        })
        .unwrap_or_default();
    Ok(Probe {
        title: tag("title"),
        artist: tag("artist"),
        album: tag("album"),
        duration,
        chapters,
    })
}

// A picture embedded in an audio file, through ffmpeg
fn embedded_picture(path: &Path) -> Option<(Vec<u8>, String)> {
    let file = path.to_str()?;
    let bytes = tools::run(
        &[
            "ffmpeg",
            "-v",
            "quiet",
            "-i",
            file,
            "-an",
            "-frames:v",
            "1",
            "-c:v",
            "mjpeg",
            "-f",
            "image2pipe",
            "-",
        ],
        TOOL_TIMEOUT,
        MAX_COVER_BYTES,
    )
    .ok()?;
    (!bytes.is_empty()).then(|| (bytes, "jpg".to_string()))
}

// One audio file that is a whole book (M4B/M4A): its own tags, length and chapters
pub fn audio_file(path: &Path) -> Result<Meta> {
    let Probe {
        title,
        artist,
        album,
        duration,
        chapters,
    } = probe(path)?;
    Ok(Meta {
        title: album.or(title),
        author: artist,
        duration_seconds: duration,
        cover: embedded_picture(path),
        tracks: vec![(
            path.file_name()
                .and_then(|n| n.to_str())
                .unwrap_or_default()
                .to_string(),
            duration,
        )],
        chapters,
        ..Default::default()
    })
}

// The audio files directly in a folder, in listening order
pub fn folder_tracks(dir: &Path) -> Result<Vec<String>> {
    let mut names: Vec<String> = std::fs::read_dir(dir)?
        .filter_map(|e| {
            let e = e.ok()?;
            let name = e.file_name().into_string().ok()?;
            (e.file_type().ok()?.is_file() && AUDIO_EXTENSIONS.contains(&extension(&name).as_str()))
                .then_some(name)
        })
        .collect();
    names.sort_by(|a, b| natural_cmp(a, b));
    if names.len() > MAX_TRACKS {
        bail!("{} has more than {MAX_TRACKS} audio files", dir.display())
    }
    Ok(names)
}

// A folder of audio files as one book: one chapter per file, the album tag as the title
pub fn audio_folder(dir: &Path) -> Result<Meta> {
    let names = folder_tracks(dir)?;
    let mut meta = Meta::default();
    let mut start = 0.0;
    for name in &names {
        let probed = probe(&dir.join(name)).ok();
        let duration = probed.as_ref().and_then(|p| p.duration);
        if meta.title.is_none() {
            meta.title = probed.as_ref().and_then(|p| p.album.clone());
            meta.author = probed.as_ref().and_then(|p| p.artist.clone());
        }
        let chapter = probed
            .as_ref()
            .and_then(|p| p.title.clone())
            .unwrap_or_else(|| title_from_name(name));
        meta.chapters.push((chapter, start));
        start += duration.unwrap_or(0.0);
        meta.tracks.push((name.clone(), duration));
    }
    meta.duration_seconds = (start > 0.0).then_some(start);
    // a cover file beside the tracks, else the first track's embedded picture
    meta.cover = [
        "cover.jpg",
        "cover.png",
        "folder.jpg",
        "folder.png",
        "front.jpg",
    ]
    .iter()
    .find_map(|n| {
        let p = dir.join(n);
        let size = std::fs::metadata(&p).ok()?.len();
        (size as usize <= MAX_COVER_BYTES)
            .then(|| std::fs::read(&p).ok().map(|b| (b, extension(n))))?
    })
    .or_else(|| names.first().and_then(|n| embedded_picture(&dir.join(n))));
    Ok(meta)
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use std::io::Write;

    // A small EPUB: container, OPF with title/authors/series/cover, and the cover image
    pub fn write_epub(path: &Path, epub3_series: bool) {
        let file = std::fs::File::create(path).unwrap();
        let mut zip = zip::ZipWriter::new(file);
        let opts = zip::write::SimpleFileOptions::default();
        zip.start_file("mimetype", opts).unwrap();
        zip.write_all(b"application/epub+zip").unwrap();
        zip.start_file("META-INF/container.xml", opts).unwrap();
        zip.write_all(br#"<?xml version="1.0"?><container xmlns="urn:oasis:names:tc:opendocument:xmlns:container" version="1.0"><rootfiles><rootfile full-path="OEBPS/content.opf" media-type="application/oebps-package+xml"/></rootfiles></container>"#).unwrap();
        let series = if epub3_series {
            r##"<meta property="belongs-to-collection" id="c1">Hainish Cycle</meta><meta refines="#c1" property="group-position">4</meta>"##
        } else {
            r#"<meta name="calibre:series" content="Hainish Cycle"/><meta name="calibre:series_index" content="4"/><meta name="cover" content="cov"/>"#
        };
        let cover_item = if epub3_series {
            r#"<item id="cov" href="images/cover.jpg" media-type="image/jpeg" properties="cover-image"/>"#
        } else {
            r#"<item id="cov" href="images/cover.jpg" media-type="image/jpeg"/>"#
        };
        let opf = format!(
            r#"<?xml version="1.0"?><package xmlns="http://www.idpf.org/2007/opf" version="3.0"><metadata xmlns:dc="http://purl.org/dc/elements/1.1/"><dc:title>The Left Hand of
            Darkness</dc:title><dc:creator>Ursula K. Le Guin</dc:creator>{series}</metadata><manifest>{cover_item}<item id="c1x" href="text/ch1.xhtml" media-type="application/xhtml+xml"/></manifest><spine><itemref idref="c1x"/></spine></package>"#
        );
        zip.start_file("OEBPS/content.opf", opts).unwrap();
        zip.write_all(opf.as_bytes()).unwrap();
        zip.start_file("OEBPS/images/cover.jpg", opts).unwrap();
        zip.write_all(b"\xff\xd8JPEGDATA").unwrap();
        zip.start_file("OEBPS/text/ch1.xhtml", opts).unwrap();
        zip.write_all(b"<html xmlns=\"http://www.w3.org/1999/xhtml\"><body><h1>One</h1><p>Hello.</p></body></html>").unwrap();
        zip.finish().unwrap();
    }

    // A small CBZ with pages named so that plain sorting would put page10 first
    pub fn write_cbz(path: &Path) {
        let mut zip = zip::ZipWriter::new(std::fs::File::create(path).unwrap());
        let opts = zip::write::SimpleFileOptions::default();
        for name in [
            "page10.png",
            "page2.png",
            "page1.png",
            "notes.txt",
            "__MACOSX/page1.png",
        ] {
            zip.start_file(name, opts).unwrap();
            zip.write_all(name.as_bytes()).unwrap();
        }
        zip.finish().unwrap();
    }

    #[test]
    fn epub_metadata_series_and_cover_both_styles() {
        let dir = tempfile::tempdir().unwrap();
        for epub3 in [false, true] {
            let path = dir.path().join(format!("b{epub3}.epub"));
            write_epub(&path, epub3);
            let m = epub(&path).unwrap();
            assert_eq!(
                m.title.as_deref(),
                Some("The Left Hand of Darkness"),
                "line breaks in the title collapse"
            );
            assert_eq!(m.author.as_deref(), Some("Ursula K. Le Guin"));
            assert_eq!(
                (m.series.as_deref(), m.series_index),
                (Some("Hainish Cycle"), Some(4.0))
            );
            assert_eq!(m.cover, Some((b"\xff\xd8JPEGDATA".to_vec(), "jpg".into())));
        }
        std::fs::write(dir.path().join("bad.epub"), b"not a zip").unwrap();
        assert!(epub(&dir.path().join("bad.epub")).is_err());
    }

    #[test]
    fn comic_pages_come_in_reading_order() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("c.cbz");
        write_cbz(&path);
        let archive = zip::ZipArchive::new(std::fs::File::open(&path).unwrap()).unwrap();
        assert_eq!(
            cbz_pages(&archive),
            ["page1.png", "page2.png", "page10.png"]
        );
        let m = cbz(&path).unwrap();
        assert_eq!(m.pages, Some(3));
        assert_eq!(m.cover.unwrap().0, b"page1.png");
    }

    #[test]
    fn natural_order_names_and_paths() {
        let mut v = vec![
            "Track 10.mp3",
            "track 2.mp3",
            "Track 1.mp3",
            "track 02b.mp3",
        ];
        v.sort_by(|a, b| natural_cmp(a, b));
        assert_eq!(
            v,
            [
                "Track 1.mp3",
                "track 2.mp3",
                "track 02b.mp3",
                "Track 10.mp3"
            ]
        );
        assert_eq!(
            title_from_name("dir/the_left-hand.of.darkness.epub"),
            "the left hand.of.darkness"
        );
        assert_eq!(
            resolve("OEBPS/text", "../images/c.jpg#x"),
            "OEBPS/images/c.jpg"
        );
        assert_eq!(resolve("", "c.jpg"), "c.jpg");
    }

    #[test]
    fn a_folder_audiobook_orders_its_files_and_falls_back_to_names() {
        let dir = tempfile::tempdir().unwrap();
        for n in ["02 - Two.mp3", "10 - Ten.mp3", "01 - One.mp3", "notes.txt"] {
            std::fs::write(dir.path().join(n), b"not really audio").unwrap();
        }
        std::fs::write(dir.path().join("cover.jpg"), b"COVER").unwrap();
        let m = audio_folder(dir.path()).unwrap();
        assert_eq!(
            m.tracks.iter().map(|t| t.0.as_str()).collect::<Vec<_>>(),
            ["01 - One.mp3", "02 - Two.mp3", "10 - Ten.mp3"]
        );
        assert_eq!(
            m.chapters[0].0, "01 One",
            "untagged files name their chapter"
        );
        assert_eq!(m.cover.unwrap().0, b"COVER");
    }
}
