// Author: Jeff
// Date: 2026-09-19
// Description: `mg-bookr tui` — the library in a terminal: draw, read a key, act
// Notes: A thread asks the listening session where it is once a second and sends it over, so
//        the screen follows the book without the loop ever waiting on mpv. Between answers the
//        clock runs on in State. A scan takes seconds, so it runs on its own thread too and
//        reports back as a message

use std::sync::mpsc::{self, Receiver, Sender};
use std::time::Duration;

use anyhow::Result;
use ratatui::DefaultTerminal;
use ratatui::crossterm::event::{self, Event, KeyEventKind};

use crate::listen::{self, Now, Sleep};
use crate::scan;
use crate::store::{self, Store};

mod draw;
pub mod state;

use state::{Control, Effect, State};

const FRAME: Duration = Duration::from_millis(250);
const ASK_EVERY: Duration = Duration::from_secs(1);
const CONTINUE_SHOWN: usize = 50;

// What the background threads report
enum Update {
    Now(Option<Box<Now>>, i64),
    Message(String),
}

// Ask the session where the book is, once a second
fn follow(to_loop: Sender<Update>) {
    let Ok(store) = Store::open(store::default_path()) else {
        return;
    };
    loop {
        let now = listen::now(&store).unwrap_or(None);
        let at = chrono::Utc::now().timestamp_millis();
        if to_loop.send(Update::Now(now.map(Box::new), at)).is_err() {
            return;
        }
        std::thread::sleep(ASK_EVERY);
    }
}

// Take over the terminal until quit, and always give it back
pub fn run() -> Result<()> {
    let (tx, rx) = mpsc::channel();
    let follower = tx.clone();
    std::thread::spawn(move || follow(follower));
    let mut terminal = ratatui::init();
    let result = event_loop(&mut terminal, &rx, &tx);
    ratatui::restore();
    result
}

fn event_loop(
    terminal: &mut DefaultTerminal,
    rx: &Receiver<Update>,
    tx: &Sender<Update>,
) -> Result<()> {
    let store = Store::open(store::default_path())?;
    let mut state = State {
        continuing: store.in_progress(CONTINUE_SHOWN)?,
        library: store.books(None, false)?,
        ..Default::default()
    };
    loop {
        while let Ok(update) = rx.try_recv() {
            match update {
                Update::Now(now, at) => {
                    state.now = now.map(|n| *n);
                    state.now_at = at;
                }
                Update::Message(m) => {
                    state.message = Some(m);
                    // a scan changes both lists
                    state.continuing = store.in_progress(CONTINUE_SHOWN)?;
                    state.library = store.books(None, false)?;
                    state.clamp();
                }
            }
        }
        terminal.draw(|frame| draw::draw(frame, &state, chrono::Utc::now().timestamp_millis()))?;
        if !event::poll(FRAME)? {
            continue;
        }
        let Event::Key(key) = event::read()? else {
            continue;
        };
        if key.kind != KeyEventKind::Press {
            continue;
        }
        state.message = None;
        let effect = state.key(key);
        if effect == Effect::Quit {
            return Ok(());
        }
        if let Err(e) = apply(effect, &mut state, &store, tx) {
            state.message = Some(format!("{e:#}"));
        }
        state.clamp();
    }
}

// Carry out one effect
fn apply(effect: Effect, state: &mut State, store: &Store, tx: &Sender<Update>) -> Result<()> {
    match effect {
        Effect::None | Effect::Quit => {}
        Effect::LoadContinue => state.continuing = store.in_progress(CONTINUE_SHOWN)?,
        Effect::LoadLibrary => state.library = store.books(None, false)?,
        Effect::Scan => {
            let tx = tx.clone();
            state.message = Some("looking through the book folders\u{2026}".into());
            std::thread::spawn(move || {
                let said = Store::open(store::default_path())
                    .and_then(|s| {
                        scan::scan(&s, &scan::default_roots(), &scan::default_cover_dir())
                    })
                    .map_or_else(
                        |e| format!("scan failed: {e:#}"),
                        |r| format!("{} books: {} new, {} missing", r.found, r.added, r.missing),
                    );
                let _ = tx.send(Update::Message(said));
            });
        }
        Effect::Play(id) => {
            let plan = listen::plan_for(store, id)?;
            listen::play(&plan, &listen::session_command(id)?)?;
            state.message = Some(format!("playing {}", plan.book.title));
        }
        // the reader window is its own program; it is not written yet
        Effect::Read(id) => {
            let book = store.book(id)?;
            state.message = Some(format!("{} opens in the reader window", book.title));
        }
        Effect::Listen(control) => match control {
            Control::Toggle => {
                listen::pause(None)?;
            }
            Control::Seek(words) => listen::seek(words)?,
            Control::Speed(speed) => {
                listen::speed(speed)?;
            }
            Control::Chapter(which) => listen::chapter(which)?,
            Control::Sleep(words) => {
                let sleep = listen::sleep(&words)?;
                state.message = Some(match sleep {
                    Sleep::Off => "sleep timer off".into(),
                    Sleep::EndOfChapter => "stopping at the end of this chapter".into(),
                    Sleep::At(_) => format!("stopping in {words} minutes"),
                });
            }
            Control::Stop => listen::stop()?,
        },
    }
    Ok(())
}
