// Author: Jeff
// Date: 2026-09-19
// Description: mg-bookr command line — scan the book folders, list, progress, highlights, collections
// Notes: `--json` works everywhere; with it a failure is still JSON on stdout
//        ({"ok":false,"error":…}, exit 1), like the other suite tools the shell reads.
//        Reading happens in the reader window and listening through the audio commands; this
//        is the library's front desk

use std::process::ExitCode;

use anyhow::Result;
use clap::{Parser, Subcommand};
use serde_json::json;

use mg_bookr::store::{self, Book, Store};
use mg_bookr::{reader, scan, vault};

const DEFAULT_CONTINUE: usize = 12;

#[derive(Parser)]
#[command(
    name = "mg-bookr",
    version,
    about = "Ebooks and audiobooks for the Geist suite"
)]
struct Cli {
    /// Print JSON instead of text
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Look through ~/books and ~/audiobooks and record what is there
    Scan,
    /// Every book (by kind: epub, pdf, comic, audio)
    List {
        #[arg(long)]
        kind: Option<String>,
        /// Include books whose files have gone missing
        #[arg(long)]
        missing: bool,
    },
    /// Books started and not finished, most recent first
    Continue {
        #[arg(long, default_value_t = DEFAULT_CONTINUE)]
        limit: usize,
    },
    /// One book with its tracks, chapters and highlights
    Show { id: i64 },
    /// Everything the reader window or player needs to open a book (unpacks EPUBs and comics)
    Read { id: i64 },
    /// Record where you are in a book
    Progress {
        id: i64,
        location: String,
        percent: f64,
        #[arg(long, conflicts_with = "unfinished")]
        finished: bool,
        #[arg(long)]
        unfinished: bool,
    },
    Highlight {
        #[command(subcommand)]
        action: HighlightAction,
    },
    Collection {
        #[command(subcommand)]
        action: CollectionAction,
    },
    /// Write a book's highlights into mg-vault as one Markdown note (done after every highlight change too)
    Export { id: i64 },
}

#[derive(Subcommand)]
enum HighlightAction {
    Add {
        book: i64,
        location: String,
        quote: String,
        #[arg(long)]
        chapter: Option<String>,
        #[arg(long)]
        note: Option<String>,
        #[arg(long, default_value = "yellow")]
        color: String,
    },
    List {
        book: i64,
    },
    /// Change a highlight's note (an empty note clears it)
    Note {
        id: i64,
        text: String,
    },
    Remove {
        id: i64,
    },
}

#[derive(Subcommand)]
enum CollectionAction {
    List,
    Show { name: String },
    Add { name: String, book: i64 },
    Remove { name: String, book: i64 },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let json = cli.json;
    match run(cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            if json {
                println!("{}", json!({ "ok": false, "error": format!("{e:#}") }));
            } else {
                eprintln!("mg-bookr: {e:#}");
            }
            ExitCode::FAILURE
        }
    }
}

fn run(cli: Cli) -> Result<()> {
    let json = cli.json;
    let store = Store::open(store::default_path())?;
    match cli.command {
        Command::Scan => {
            let report = scan::scan(&store, &scan::default_roots(), &scan::default_cover_dir())?;
            if json {
                println!("{}", serde_json::to_string(&report)?);
            } else {
                println!(
                    "{} books: {} new, {} missing",
                    report.found, report.added, report.missing
                );
                for (path, why) in &report.unreadable {
                    println!("  could not read {path}: {why}");
                }
            }
        }
        Command::List { kind, missing } => books(json, &store.books(kind.as_deref(), missing)?)?,
        Command::Continue { limit } => books(json, &store.in_progress(limit)?)?,
        Command::Show { id } => {
            let book = store.book(id)?;
            let tracks = store.tracks(id)?;
            let chapters = store.chapters(id)?;
            let highlights = store.highlights(id)?;
            if json {
                println!(
                    "{}",
                    json!({ "book": book, "tracks": tracks, "chapters": chapters, "highlights": highlights })
                );
            } else {
                println!("{}", line(&book));
                for (title, start) in &chapters {
                    println!("  {:>8}  {title}", clock(*start));
                }
                for h in &highlights {
                    println!(
                        "  \u{201c}{}\u{201d}{}",
                        h.quote,
                        h.note
                            .as_ref()
                            .map_or(String::new(), |n| format!(" \u{2014} {n}"))
                    );
                }
            }
        }
        Command::Read { id } => {
            let plan = reader::plan(
                &store,
                id,
                &scan::default_roots(),
                &reader::default_unpack_dir(),
            )?;
            if json {
                println!("{}", serde_json::to_string(&plan)?);
            } else {
                println!("{}  {}", line(&plan.book), plan.file);
                if let Some(e) = &plan.epub {
                    println!(
                        "  {} pages, {} contents entries, unpacked in {}",
                        e.spine.len(),
                        e.toc.len(),
                        e.dir
                    );
                }
                if !plan.pages.is_empty() {
                    println!("  {} comic pages", plan.pages.len());
                }
            }
        }
        Command::Progress {
            id,
            location,
            percent,
            finished,
            unfinished,
        } => {
            let flag = if finished {
                Some(true)
            } else if unfinished {
                Some(false)
            } else {
                None
            };
            store.set_progress(id, &location, percent, flag)?;
            done(
                json,
                json!({ "ok": true, "book": store.book(id)? }),
                format!("saved: {percent:.1}%"),
            );
        }
        Command::Highlight { action } => match action {
            HighlightAction::Add {
                book,
                location,
                quote,
                chapter,
                note,
                color,
            } => {
                let id = store.add_highlight(
                    book,
                    &location,
                    chapter.as_deref(),
                    &quote,
                    note.as_deref(),
                    &color,
                )?;
                let vault = sync(&store, book);
                done(
                    json,
                    json!({ "ok": true, "id": id, "vault": vault }),
                    format!("highlight {id} saved; {vault}"),
                );
            }
            HighlightAction::List { book } => {
                let list = store.highlights(book)?;
                if json {
                    println!("{}", serde_json::to_string(&list)?);
                } else {
                    for h in &list {
                        println!("{:>5}  [{}] \u{201c}{}\u{201d}", h.id, h.color, h.quote);
                    }
                }
            }
            HighlightAction::Note { id, text } => {
                let book = store.set_note(id, &text)?;
                let vault = sync(&store, book);
                done(
                    json,
                    json!({ "ok": true, "vault": vault }),
                    format!("note saved; {vault}"),
                );
            }
            HighlightAction::Remove { id } => {
                let book = store.remove_highlight(id)?;
                let vault = sync(&store, book);
                done(
                    json,
                    json!({ "ok": true, "vault": vault }),
                    format!("highlight removed; {vault}"),
                );
            }
        },
        Command::Collection { action } => match action {
            CollectionAction::List => {
                let list = store.collections()?;
                if json {
                    println!(
                        "{}",
                        json!(
                            list.iter()
                                .map(|(n, c)| json!({ "name": n, "books": c }))
                                .collect::<Vec<_>>()
                        )
                    );
                } else {
                    for (name, count) in &list {
                        println!("{name}  ({count})");
                    }
                }
            }
            CollectionAction::Show { name } => books(json, &store.collection(&name)?)?,
            CollectionAction::Add { name, book } => {
                store.collect(&name, book)?;
                done(json, json!({ "ok": true }), format!("added to {name}"));
            }
            CollectionAction::Remove { name, book } => {
                store.uncollect(&name, book)?;
                done(json, json!({ "ok": true }), format!("removed from {name}"));
            }
        },
        Command::Export { id } => {
            let book = store.book(id)?;
            let (path, outcome) =
                vault::export(&vault::RealVault::from_env(), &book, &store.highlights(id)?)?;
            let what = match outcome {
                vault::Outcome::Created => "created",
                vault::Outcome::Updated => "updated",
                vault::Outcome::Unchanged => "already up to date",
            };
            done(
                json,
                json!({ "ok": true, "path": path, "outcome": what }),
                format!("{path}: {what}"),
            );
        }
    }
    Ok(())
}

// After a highlight change: rewrite the book's vault note, and say how it went (never fails the change)
fn sync(store: &Store, book_id: i64) -> String {
    let result = store.book(book_id).and_then(|book| {
        let highlights = store.highlights(book_id)?;
        vault::export(&vault::RealVault::from_env(), &book, &highlights)
    });
    match result {
        Ok((path, _)) => format!("vault note {path} updated"),
        Err(e) => format!("vault note not written: {e:#}"),
    }
}

// An action's answer: JSON, or one line
fn done(json: bool, value: serde_json::Value, text: String) {
    if json {
        println!("{value}");
    } else {
        println!("{text}");
    }
}

fn books(json: bool, list: &[Book]) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string(list)?);
    } else {
        for b in list {
            println!("{}", line(b));
        }
    }
    Ok(())
}

// "  12  [epub]  Title — Author  (40%)"
fn line(b: &Book) -> String {
    let by = b
        .author
        .as_ref()
        .map_or(String::new(), |a| format!(" \u{2014} {a}"));
    let at = b.percent.map_or(String::new(), |p| {
        if b.finished {
            "  (finished)".into()
        } else {
            format!("  ({p:.0}%)")
        }
    });
    let gone = if b.missing { "  [missing]" } else { "" };
    format!("{:>4}  [{}]  {}{by}{at}{gone}", b.id, b.kind, b.title)
}

// 3725 s → "1:02:05"
fn clock(seconds: f64) -> String {
    let s = seconds.max(0.0) as u64;
    format!("{}:{:02}:{:02}", s / 3600, s % 3600 / 60, s % 60)
}
