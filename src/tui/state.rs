// Author: Jeff
// Date: 2026-09-19
// Description: Everything the books TUI knows and what each key does — no terminal involved
// Notes: Keys become an Effect the loop carries out (load a list, rescan, play a book, steer
//        the listening session). Pure, so tests drive it with plain key events.
//        The listening keys work on every tab. The clock: `now.position` is where mpv was at
//        `now_at`; while playing, the screen adds the time since (times the speed), so it moves
//        every frame without asking mpv

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::listen::{MAX_SPEED, MIN_SPEED, Now};
use crate::store::Book;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Tab {
    #[default]
    Continue,
    Library,
    Listening,
}

pub const TABS: [Tab; 3] = [Tab::Continue, Tab::Library, Tab::Listening];

impl Tab {
    pub fn title(self) -> &'static str {
        match self {
            Tab::Continue => "Continue",
            Tab::Library => "Library",
            Tab::Listening => "Listening",
        }
    }
    fn index(self) -> usize {
        TABS.iter().position(|t| *t == self).unwrap_or(0)
    }
}

// the library's kind filter, stepped through with f ("" is every kind)
pub const KIND_FILTERS: [&str; 5] = ["", "epub", "pdf", "comic", "audio"];

// One thing to tell the listening session
#[derive(Debug, Clone, PartialEq)]
pub enum Control {
    Toggle,
    // "+15" or "-15"
    Seek(&'static str),
    Speed(f64),
    // "next" or "prev"
    Chapter(&'static str),
    // minutes, "chapter" or "off"
    Sleep(String),
    Stop,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Effect {
    None,
    Quit,
    LoadContinue,
    LoadLibrary,
    Scan,
    Play(i64),
    Read(i64),
    Listen(Control),
}

#[derive(Default)]
pub struct State {
    pub tab: Tab,
    pub continuing: Vec<Book>,
    pub library: Vec<Book>,
    // index into KIND_FILTERS
    pub kind: usize,
    pub now: Option<Now>,
    // when `now` was read, unix ms
    pub now_at: i64,
    pub cursor: [usize; 3],
    pub message: Option<String>,
}

// one arrow press moves this far, as audiobook apps do
const SEEK_BACK: &str = "-15";
const SEEK_ON: &str = "+15";
const SPEED_STEP: f64 = 0.25;
// z steps through these minutes, then the end of the chapter, then off
const SLEEP_STEPS: [f64; 4] = [15.0, 30.0, 45.0, 60.0];
// a timer just set to 15 reads as a little under it, so the next step needs slack
const SLEEP_SLACK: f64 = 60.0;

impl State {
    // Where the book is this moment: mpv's last word, plus the time since if it is playing
    pub fn position(&self, now_ms: i64) -> f64 {
        let Some(n) = &self.now else { return 0.0 };
        let moved = if n.paused {
            0.0
        } else {
            (now_ms - self.now_at).max(0) as f64 / 1000.0 * n.speed
        };
        let at = n.position + moved;
        if n.duration > 0.0 {
            at.min(n.duration)
        } else {
            at
        }
    }

    // The library as filtered now
    pub fn shown(&self) -> Vec<&Book> {
        let kind = KIND_FILTERS[self.kind];
        self.library
            .iter()
            .filter(|b| kind.is_empty() || b.kind.starts_with(kind))
            .collect()
    }

    fn rows(&self, tab: Tab) -> usize {
        match tab {
            Tab::Continue => self.continuing.len(),
            Tab::Library => self.shown().len(),
            Tab::Listening => 0,
        }
    }

    pub fn cursor(&self) -> usize {
        self.cursor[self.tab.index()]
    }

    // Keep each cursor inside its list after the list changed
    pub fn clamp(&mut self) {
        for tab in TABS {
            let rows = self.rows(tab);
            let c = &mut self.cursor[tab.index()];
            *c = (*c).min(rows.saturating_sub(1));
        }
    }

    fn step(&mut self, delta: isize) {
        let last = self.rows(self.tab).saturating_sub(1) as isize;
        let c = &mut self.cursor[self.tab.index()];
        *c = (*c as isize + delta).clamp(0, last.max(0)) as usize;
    }

    fn switch(&mut self, tab: Tab) -> Effect {
        self.tab = tab;
        match tab {
            Tab::Continue => Effect::LoadContinue,
            Tab::Library => Effect::LoadLibrary,
            Tab::Listening => Effect::None,
        }
    }

    // The book under the cursor on a list tab
    fn selected(&self) -> Option<&Book> {
        match self.tab {
            Tab::Continue => self.continuing.get(self.cursor()),
            Tab::Library => self.shown().get(self.cursor()).copied(),
            Tab::Listening => None,
        }
    }

    // The next sleep setting after the current one
    fn next_sleep(&self) -> String {
        let Some(n) = &self.now else {
            return SLEEP_STEPS[0].to_string();
        };
        if n.sleep_chapter {
            return "off".into();
        }
        match n.sleep_left {
            None => SLEEP_STEPS[0].to_string(),
            Some(left) => SLEEP_STEPS
                .iter()
                .find(|m| **m * 60.0 > left + SLEEP_SLACK)
                .map_or("chapter".into(), |m| m.to_string()),
        }
    }

    pub fn key(&mut self, key: KeyEvent) -> Effect {
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            return Effect::Quit;
        }
        let listen = |c| Effect::Listen(c);
        match key.code {
            KeyCode::Char('q') => return Effect::Quit,
            KeyCode::Tab => return self.switch(TABS[(self.tab.index() + 1) % TABS.len()]),
            KeyCode::BackTab => {
                return self.switch(TABS[(self.tab.index() + TABS.len() - 1) % TABS.len()]);
            }
            KeyCode::Char(c @ '1'..='3') => return self.switch(TABS[c as usize - '1' as usize]),
            // listening works on every tab
            KeyCode::Char(' ') => return listen(Control::Toggle),
            KeyCode::Left => return listen(Control::Seek(SEEK_BACK)),
            KeyCode::Right => return listen(Control::Seek(SEEK_ON)),
            KeyCode::Char('n') => return listen(Control::Chapter("next")),
            KeyCode::Char('p') => return listen(Control::Chapter("prev")),
            KeyCode::Char(c @ ('[' | ']')) => {
                let Some(n) = &self.now else {
                    self.message = Some("nothing is playing".into());
                    return Effect::None;
                };
                let step = if c == ']' { SPEED_STEP } else { -SPEED_STEP };
                return listen(Control::Speed((n.speed + step).clamp(MIN_SPEED, MAX_SPEED)));
            }
            KeyCode::Char('z') => return listen(Control::Sleep(self.next_sleep())),
            KeyCode::Char('x') => return listen(Control::Stop),
            KeyCode::Char('s') => return Effect::Scan,
            KeyCode::Down | KeyCode::Char('j') => self.step(1),
            KeyCode::Up | KeyCode::Char('k') => self.step(-1),
            KeyCode::Home | KeyCode::Char('g') => self.step(isize::MIN / 2),
            KeyCode::End | KeyCode::Char('G') => self.step(isize::MAX / 2),
            KeyCode::Char('f') if self.tab == Tab::Library => {
                self.kind = (self.kind + 1) % KIND_FILTERS.len();
                self.cursor[Tab::Library.index()] = 0;
            }
            KeyCode::Enter => {
                if let Some(book) = self.selected() {
                    return if book.kind.starts_with("audio") {
                        Effect::Play(book.id)
                    } else {
                        Effect::Read(book.id)
                    };
                }
            }
            _ => {}
        }
        Effect::None
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use ratatui::crossterm::event::{KeyEventKind, KeyEventState};

    pub fn key(code: KeyCode) -> KeyEvent {
        KeyEvent {
            code,
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        }
    }

    pub fn book(id: i64, kind: &str) -> Book {
        Book {
            id,
            kind: kind.into(),
            root: "books".into(),
            path: format!("{id}"),
            title: format!("Book {id}"),
            author: None,
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

    pub fn now(paused: bool, speed: f64, sleep_left: Option<f64>, sleep_chapter: bool) -> Now {
        Now {
            book: book(7, "audio"),
            track: 0,
            tracks: 1,
            position: 10.0,
            duration: 100.0,
            percent: 10.0,
            chapter: None,
            speed,
            paused,
            sleep_left,
            sleep_chapter,
        }
    }

    #[test]
    fn the_clock_runs_at_the_books_speed_only_while_playing() {
        let mut s = State {
            now: Some(now(false, 2.0, None, false)),
            now_at: 1_000,
            ..Default::default()
        };
        assert_eq!(s.position(4_000), 16.0, "3 s at 2x");
        assert_eq!(s.position(1_000_000), 100.0, "never past the end");
        s.now = Some(now(true, 2.0, None, false));
        assert_eq!(s.position(4_000), 10.0);
    }

    #[test]
    fn enter_plays_an_audiobook_and_opens_anything_else() {
        let mut s = State {
            continuing: vec![book(1, "audio-folder"), book(2, "epub")],
            ..Default::default()
        };
        assert_eq!(s.key(key(KeyCode::Enter)), Effect::Play(1));
        s.key(key(KeyCode::Down));
        assert_eq!(s.key(key(KeyCode::Enter)), Effect::Read(2));
        s.key(key(KeyCode::Down));
        assert_eq!(s.cursor(), 1, "the cursor stops at the last row");
    }

    #[test]
    fn the_library_filter_steps_through_kinds() {
        let mut s = State {
            tab: Tab::Library,
            library: vec![book(1, "epub"), book(2, "audio"), book(3, "audio-folder")],
            ..Default::default()
        };
        assert_eq!(s.shown().len(), 3);
        s.key(key(KeyCode::Char('f')));
        assert_eq!(s.shown().len(), 1, "epub");
        for _ in 0..3 {
            s.key(key(KeyCode::Char('f')));
        }
        assert_eq!(s.shown().len(), 2, "both kinds of audiobook");
        s.key(key(KeyCode::Char('f')));
        assert_eq!(s.kind, 0, "back to every kind");
    }

    #[test]
    fn speed_steps_by_a_quarter_inside_its_range() {
        let mut s = State::default();
        assert_eq!(s.key(key(KeyCode::Char(']'))), Effect::None);
        assert_eq!(s.message.as_deref(), Some("nothing is playing"));
        s.now = Some(now(false, 2.9, None, false));
        assert_eq!(
            s.key(key(KeyCode::Char(']'))),
            Effect::Listen(Control::Speed(3.0))
        );
        s.now = Some(now(false, 1.0, None, false));
        assert_eq!(
            s.key(key(KeyCode::Char('['))),
            Effect::Listen(Control::Speed(0.75))
        );
    }

    #[test]
    fn z_steps_the_sleep_timer_through_its_settings() {
        let mut s = State {
            now: Some(now(false, 1.0, None, false)),
            ..Default::default()
        };
        let sleep = |s: &mut State| match s.key(key(KeyCode::Char('z'))) {
            Effect::Listen(Control::Sleep(w)) => w,
            other => panic!("{other:?}"),
        };
        assert_eq!(sleep(&mut s), "15");
        s.now = Some(now(false, 1.0, Some(15.0 * 60.0 - 3.0), false));
        assert_eq!(sleep(&mut s), "30", "a running 15 goes up to 30");
        s.now = Some(now(false, 1.0, Some(45.0 * 60.0 - 3.0), false));
        assert_eq!(sleep(&mut s), "60");
        s.now = Some(now(false, 1.0, Some(120.0), false));
        assert_eq!(sleep(&mut s), "15", "nearly over, so 15 again");
        s.now = Some(now(false, 1.0, Some(3600.0), false));
        assert_eq!(sleep(&mut s), "chapter");
        s.now = Some(now(false, 1.0, None, true));
        assert_eq!(sleep(&mut s), "off");
    }

    #[test]
    fn tabs_load_their_lists_and_listening_keys_work_everywhere() {
        let mut s = State::default();
        assert_eq!(s.key(key(KeyCode::Char('2'))), Effect::LoadLibrary);
        assert_eq!(s.key(key(KeyCode::Tab)), Effect::None);
        assert_eq!(s.tab, Tab::Listening);
        assert_eq!(s.key(key(KeyCode::Tab)), Effect::LoadContinue);
        assert_eq!(
            s.key(key(KeyCode::Char(' '))),
            Effect::Listen(Control::Toggle)
        );
        assert_eq!(
            s.key(key(KeyCode::Left)),
            Effect::Listen(Control::Seek("-15"))
        );
        assert_eq!(
            s.key(key(KeyCode::Char('n'))),
            Effect::Listen(Control::Chapter("next"))
        );
        assert_eq!(
            s.key(key(KeyCode::Char('x'))),
            Effect::Listen(Control::Stop)
        );
        assert_eq!(s.key(key(KeyCode::Char('q'))), Effect::Quit);
    }
}
