// Author: Jeff
// Date: 2026-09-19
// Description: A book's highlights as one Markdown note in mg-vault, rewritten on every change
// Notes: mg-bookr owns the note: its body is made fresh from mg-bookr's records each time, and
//        the note says so at the top. Writes go through mg-vault's CLI only (never its files):
//        read the note for its fingerprint, then write with `--expected <fingerprint>` so a
//        change made in between is noticed. A missing note is created. A conflict or collision
//        (someone got there first) is answered by reading again and writing once more.
//        The binary: $MG_VAULT_BIN, else $GEIST_ROOT (or ~/geistos/mg-suite)/mg-vaultr/target/debug/mg-vault

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde_json::Value;

use crate::meta::natural_cmp;
use crate::store::{Book, Highlight};

const FOLDER: &str = "Books";
const SLUG_MAX: usize = 60;
// argv carries the body; a book's highlights never come near this, but a limit keeps it sane
const MAX_BODY_BYTES: usize = 512 * 1024;
const TIMEOUT: Duration = Duration::from_secs(20);

// Anything that can run an mg-vault command and hand back its JSON envelope
pub trait VaultCli {
    fn run(&self, args: &[&str]) -> Result<Value>;
}

pub struct RealVault {
    pub binary: PathBuf,
}

impl RealVault {
    pub fn from_env() -> Self {
        RealVault {
            binary: crate::tools::suite_binary("MG_VAULT_BIN", "mg-vaultr", "mg-vault"),
        }
    }
}

impl VaultCli for RealVault {
    fn run(&self, args: &[&str]) -> Result<Value> {
        let binary = self
            .binary
            .to_str()
            .context("mg-vault's path is not text")?;
        let mut argv = vec![binary, "--json", "--no-input"];
        argv.extend_from_slice(args);
        // mg-vault answers on stdout, but a refusal (exit 1) puts its envelope on stderr
        let run = crate::tools::run_status(&argv, TIMEOUT, MAX_BODY_BYTES * 2)?;
        let answer = if run.success { run.stdout } else { run.stderr };
        serde_json::from_slice(&answer).context("mg-vault did not answer JSON")
    }
}

// The vault path for a book's note: Books/<title-slug>-<id>.md (the id keeps two "Poems" apart)
pub fn note_path(book: &Book) -> String {
    let mut slug = String::new();
    for c in book.title.chars() {
        if c.is_alphanumeric() {
            slug.extend(c.to_lowercase());
        } else if !slug.ends_with('-') && !slug.is_empty() {
            slug.push('-');
        }
        if slug.chars().count() >= SLUG_MAX {
            break;
        }
    }
    let slug = slug.trim_matches('-');
    let slug = if slug.is_empty() { "book" } else { slug };
    format!("{FOLDER}/{slug}-{}.md", book.id)
}

// A line of Markdown that cannot start a heading, list or front-matter by accident
fn quote_line(text: &str) -> String {
    text.lines()
        .map(|l| format!("> {l}"))
        .collect::<Vec<_>>()
        .join("\n")
}

// The note: front matter, title and author, then each chapter's highlights in book order
pub fn note_body(book: &Book, highlights: &[Highlight]) -> String {
    let mut sorted: Vec<&Highlight> = highlights.iter().collect();
    sorted.sort_by(|a, b| natural_cmp(&a.location, &b.location).then(a.id.cmp(&b.id)));
    let yaml = |s: &str| s.replace('\\', "\\\\").replace('"', "\\\"");
    let mut out = format!(
        "---\nsource: mg-bookr\nbook_id: {}\ntitle: \"{}\"\nauthor: \"{}\"\nhighlights: {}\n---\n\n# {}\n",
        book.id,
        yaml(&book.title),
        yaml(book.author.as_deref().unwrap_or("")),
        highlights.len(),
        book.title
    );
    if let Some(author) = &book.author {
        out.push_str(&format!("\n*{author}*\n"));
    }
    out.push_str("\nWritten by mg-bookr from this book's highlights; edit them in the reader \u{2014} changes made here are replaced.\n");
    let mut chapter: Option<&str> = None;
    for h in sorted {
        let this = h.chapter.as_deref().unwrap_or("");
        if chapter != Some(this) {
            out.push_str(&format!(
                "\n## {}\n",
                if this.is_empty() { "Highlights" } else { this }
            ));
            chapter = Some(this);
        }
        out.push('\n');
        out.push_str(&quote_line(&h.quote));
        out.push('\n');
        if let Some(note) = &h.note {
            out.push_str(&format!("\n{note}\n"));
        }
    }
    out
}

// An envelope's error code and message, if it is an error
fn error_of(v: &Value) -> Option<(String, String)> {
    if v["ok"].as_bool() == Some(true) {
        return None;
    }
    Some((
        v["error"]["code"].as_str().unwrap_or("error").to_string(),
        v["error"]["message"]
            .as_str()
            .or_else(|| v["error"].as_str())
            .unwrap_or("mg-vault refused")
            .to_string(),
    ))
}

#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    Created,
    Updated,
    Unchanged,
}

// Put a book's note into the vault, creating or rewriting it
pub fn export(
    cli: &dyn VaultCli,
    book: &Book,
    highlights: &[Highlight],
) -> Result<(String, Outcome)> {
    let path = note_path(book);
    let body = note_body(book, highlights);
    if body.len() > MAX_BODY_BYTES {
        bail!("the note would be larger than {MAX_BODY_BYTES} bytes")
    }
    // joined to its flag: the body starts with "---", which a separate argument would read as one
    let body_arg = format!("--body={body}");
    // two tries: a conflict or collision means someone wrote in between — read again, write again
    for _ in 0..2 {
        let read = cli.run(&["note", "read", &path])?;
        let outcome = match error_of(&read) {
            None => {
                if read["data"]["content"].as_str() == Some(body.as_str()) {
                    return Ok((path, Outcome::Unchanged));
                }
                let fingerprint = read["data"]["fingerprint"]
                    .as_str()
                    .context("mg-vault gave no fingerprint")?
                    .to_string();
                (
                    cli.run(&[
                        "note",
                        "write",
                        &path,
                        &body_arg,
                        "--expected",
                        &fingerprint,
                    ])?,
                    Outcome::Updated,
                )
            }
            // a note that does not exist yet reads as an i/o error
            Some((code, message)) if code == "io" && message.contains("No such file") => (
                cli.run(&["note", "create", &path, &body_arg])?,
                Outcome::Created,
            ),
            Some((_, message)) => bail!("mg-vault: {message}"),
        };
        match error_of(&outcome.0) {
            None => return Ok((path, outcome.1)),
            Some((code, _)) if code == "conflict" || code == "collision" => continue,
            Some((_, message)) => bail!("mg-vault: {message}"),
        }
    }
    bail!("the note kept changing while mg-bookr wrote it; try again")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::cell::RefCell;

    // A scripted mg-vault: answers in order, and remembers what it was asked
    struct Fake {
        answers: RefCell<Vec<Value>>,
        asked: RefCell<Vec<Vec<String>>>,
    }

    impl VaultCli for Fake {
        fn run(&self, args: &[&str]) -> Result<Value> {
            self.asked
                .borrow_mut()
                .push(args.iter().map(|a| a.to_string()).collect());
            Ok(self.answers.borrow_mut().remove(0))
        }
    }

    fn fake(answers: Vec<Value>) -> Fake {
        Fake {
            answers: RefCell::new(answers),
            asked: RefCell::new(Vec::new()),
        }
    }

    fn book() -> Book {
        Book {
            id: 12,
            kind: "epub".into(),
            root: "books".into(),
            path: "x.epub".into(),
            title: "The Left Hand of Darkness: A \"Novel\"".into(),
            author: Some("Ursula K. Le Guin".into()),
            series: None,
            series_index: None,
            cover: None,
            pages: None,
            duration_seconds: None,
            missing: false,
            location: None,
            percent: None,
            finished: false,
            updated_at: None,
        }
    }

    fn hl(
        id: i64,
        location: &str,
        chapter: Option<&str>,
        quote: &str,
        note: Option<&str>,
    ) -> Highlight {
        Highlight {
            id,
            book_id: 12,
            location: location.into(),
            chapter: chapter.map(Into::into),
            quote: quote.into(),
            note: note.map(Into::into),
            color: "yellow".into(),
            created_at: String::new(),
        }
    }

    const MISSING: &str = r#"{"error":{"code":"io","message":"I/O error at /v/Books/x.md: No such file or directory (os error 2)"},"ok":false,"version":1}"#;

    #[test]
    fn the_note_groups_highlights_by_chapter_in_book_order() {
        let body = note_body(
            &book(),
            &[
                hl(2, "10:0.5", Some("Ch 10"), "later", None),
                hl(
                    1,
                    "2:0.1",
                    Some("Ch 2"),
                    "earlier\nsecond line",
                    Some("my thought"),
                ),
            ],
        );
        assert!(body.starts_with("---\nsource: mg-bookr\nbook_id: 12\ntitle: \"The Left Hand of Darkness: A \\\"Novel\\\"\""));
        let ch2 = body.find("## Ch 2").unwrap();
        assert!(
            ch2 < body.find("## Ch 10").unwrap(),
            "spine 2 before spine 10, not text order"
        );
        assert!(body.contains("> earlier\n> second line\n\nmy thought"));
        assert_eq!(
            note_path(&book()),
            "Books/the-left-hand-of-darkness-a-novel-12.md"
        );
    }

    #[test]
    fn a_missing_note_is_created_and_an_existing_one_written_with_its_fingerprint() {
        let f = fake(vec![
            serde_json::from_str(MISSING).unwrap(),
            json!({"ok":true,"data":{"fingerprint":"sha256:a"}}),
        ]);
        assert_eq!(export(&f, &book(), &[]).unwrap().1, Outcome::Created);
        assert_eq!(
            f.asked.borrow()[1][..2],
            ["note".to_string(), "create".to_string()]
        );
        assert!(
            f.asked.borrow()[1][3].starts_with("--body=---\n"),
            "the body rides joined to its flag"
        );

        let f = fake(vec![
            json!({"ok":true,"data":{"content":"old","fingerprint":"sha256:b"}}),
            json!({"ok":true,"data":{}}),
        ]);
        assert_eq!(export(&f, &book(), &[]).unwrap().1, Outcome::Updated);
        let asked = f.asked.borrow();
        assert_eq!(
            asked[1].last().unwrap(),
            "sha256:b",
            "written against the fingerprint just read"
        );
    }

    #[test]
    fn same_content_is_left_alone_and_a_conflict_is_retried_once() {
        let body = note_body(&book(), &[]);
        let f = fake(vec![
            json!({"ok":true,"data":{"content":body,"fingerprint":"x"}}),
        ]);
        assert_eq!(export(&f, &book(), &[]).unwrap().1, Outcome::Unchanged);

        let conflict = json!({"ok":false,"error":{"code":"conflict","message":"source changed"}});
        let f = fake(vec![
            json!({"ok":true,"data":{"content":"a","fingerprint":"1"}}),
            conflict.clone(),
            json!({"ok":true,"data":{"content":"b","fingerprint":"2"}}),
            json!({"ok":true,"data":{}}),
        ]);
        assert_eq!(export(&f, &book(), &[]).unwrap().1, Outcome::Updated);
        assert_eq!(f.asked.borrow().len(), 4);

        let f = fake(vec![
            json!({"ok":false,"error":{"code":"no_vault_selected","message":"no vault is selected"}}),
        ]);
        assert_eq!(
            export(&f, &book(), &[]).unwrap_err().to_string(),
            "mg-vault: no vault is selected"
        );
    }
}
