// Author: Jeff
// Date: 2026-09-19
// Description: Paint the books TUI — tabs, the tab's body, one line of keys or news at the bottom
// Notes: Named ANSI colours only, so the terminal theme decides the shades

use crate::terminal_text::sanitize_terminal_text;
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Gauge, List, ListItem, ListState, Paragraph, Tabs};

use super::state::{KIND_FILTERS, State, TABS, Tab};
use crate::store::Book;

const ACCENT: Color = Color::Magenta;
const DIM: Color = Color::DarkGray;

// 3725 s → "1:02:05"
pub fn clock(seconds: f64) -> String {
    let s = seconds.max(0.0) as u64;
    format!("{}:{:02}:{:02}", s / 3600, s % 3600 / 60, s % 60)
}

pub fn draw(frame: &mut Frame, state: &State, now_ms: i64) {
    let [top, body, bottom] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(3),
        Constraint::Length(1),
    ])
    .areas(frame.area());
    let titles = TABS
        .iter()
        .enumerate()
        .map(|(i, t)| format!("{} {}", i + 1, t.title()));
    let selected = TABS.iter().position(|t| *t == state.tab).unwrap_or(0);
    frame.render_widget(
        Tabs::new(titles)
            .select(selected)
            .highlight_style(Style::new().fg(ACCENT).bold())
            .divider("\u{2502}"),
        top,
    );
    match state.tab {
        Tab::Continue => draw_list(
            frame,
            body,
            " reading and listening ",
            state.continuing.iter().map(book_line).collect(),
            state.cursor(),
        ),
        Tab::Library => {
            let kind = KIND_FILTERS[state.kind];
            let title = if kind.is_empty() {
                " library ".to_string()
            } else {
                format!(" library \u{b7} {kind} ")
            };
            let rows = state.shown().into_iter().map(book_line).collect();
            draw_list(frame, body, &title, rows, state.cursor())
        }
        Tab::Listening => draw_listening(frame, body, state, now_ms),
    }
    let keys = match state.tab {
        _ if state.message.is_some() => state.message.clone().unwrap_or_default(),
        Tab::Listening => "space play/pause \u{b7} \u{2190}/\u{2192} 15s \u{b7} n/p chapter \u{b7} [/] speed \u{b7} z sleep \u{b7} x stop \u{b7} q quit".into(),
        _ => "enter play/read \u{b7} f kind \u{b7} s rescan \u{b7} space play/pause \u{b7} z sleep \u{b7} tab/1-3 \u{b7} q quit".into(),
    };
    frame.render_widget(
        Paragraph::new(sanitize_terminal_text(&keys)).fg(DIM),
        bottom,
    );
}

// "Title — Author   62%" with the kind ahead of it
fn book_line(book: &Book) -> Line<'static> {
    let kind = match book.kind.as_str() {
        "audio" | "audio-folder" => "audio",
        other => other,
    };
    let percent = match (book.percent, book.finished) {
        (_, true) => "  finished".to_string(),
        (Some(p), _) if p > 0.0 => format!("  {p:.0}%"),
        _ => String::new(),
    };
    let mut spans = vec![
        Span::styled(format!("{kind:<6}"), Style::new().fg(DIM)),
        Span::raw(sanitize_terminal_text(&book.title)),
    ];
    if let Some(author) = &book.author {
        spans.push(Span::styled(
            format!(" — {}", sanitize_terminal_text(author)),
            Style::new().fg(ACCENT),
        ));
    }
    spans.push(Span::styled(percent, Style::new().fg(DIM)));
    Line::from(spans)
}

fn draw_list(frame: &mut Frame, area: Rect, title: &str, rows: Vec<Line<'static>>, cursor: usize) {
    let empty = rows.is_empty();
    let list = List::new(rows.into_iter().map(ListItem::new))
        .block(
            Block::new()
                .borders(Borders::TOP)
                .title(sanitize_terminal_text(title))
                .border_style(Style::new().fg(DIM)),
        )
        .highlight_style(Style::new().add_modifier(Modifier::REVERSED));
    let mut list_state = ListState::default().with_selected((!empty).then_some(cursor));
    frame.render_stateful_widget(list, area, &mut list_state);
}

fn draw_listening(frame: &mut Frame, area: Rect, state: &State, now_ms: i64) {
    let Some(now) = &state.now else {
        frame.render_widget(
            Paragraph::new("nothing is playing \u{b7} enter on an audiobook starts one").fg(DIM),
            area,
        );
        return;
    };
    let [title, author, gauge, info] = Layout::vertical([
        Constraint::Length(2),
        Constraint::Length(2),
        Constraint::Length(1),
        Constraint::Min(1),
    ])
    .areas(area);
    frame.render_widget(
        Paragraph::new(sanitize_terminal_text(&now.book.title)).bold(),
        title,
    );
    let by = [
        now.book.author.as_deref().map(sanitize_terminal_text),
        now.chapter.as_deref().map(sanitize_terminal_text),
        (now.tracks > 1).then(|| format!("track {} of {}", now.track + 1, now.tracks)),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>()
    .join(" \u{b7} ");
    frame.render_widget(Paragraph::new(by).fg(ACCENT), author);
    let position = state.position(now_ms);
    let ratio = if now.duration > 0.0 {
        (position / now.duration).clamp(0.0, 1.0)
    } else {
        0.0
    };
    frame.render_widget(
        Gauge::default()
            .ratio(ratio)
            .label(format!("{} / {}", clock(position), clock(now.duration)))
            .gauge_style(Style::new().fg(ACCENT)),
        gauge,
    );
    let sleep = match (now.sleep_left, now.sleep_chapter) {
        (Some(left), _) => format!(" \u{b7} sleep in {} min", (left / 60.0).ceil()),
        (_, true) => " \u{b7} sleep at chapter end".to_string(),
        _ => String::new(),
    };
    frame.render_widget(
        Paragraph::new(format!(
            "{} \u{b7} {}\u{d7} \u{b7} {:.0}% of the book{sleep}",
            if now.paused { "paused" } else { "playing" },
            now.speed,
            now.percent
        ))
        .fg(DIM),
        info,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::state::tests::{book, now};

    fn screen(state: &State, w: u16, h: u16) -> String {
        let mut t = ratatui::Terminal::new(ratatui::backend::TestBackend::new(w, h)).unwrap();
        t.draw(|f| draw(f, state, 5_000)).unwrap();
        t.backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect()
    }

    #[test]
    fn every_tab_draws_and_the_listening_clock_advances() {
        let mut listening = now(false, 2.0, Some(15.0 * 60.0), false);
        listening.book.author = Some("Ursula K. Le Guin".into());
        let mut s = State {
            continuing: vec![book(1, "epub")],
            library: vec![book(1, "epub"), book(2, "audio")],
            now: Some(listening),
            now_at: 1_000,
            ..Default::default()
        };
        assert!(screen(&s, 100, 12).contains("Book 1"));
        s.tab = Tab::Listening;
        let text = screen(&s, 100, 12);
        assert!(text.contains("Book 7") && text.contains("Ursula K. Le Guin"));
        assert!(text.contains("0:00:18 / 0:01:40"), "4 s at 2x from 10 s");
        assert!(text.contains("sleep in 15 min") && text.contains("2\u{d7}"));
        for tab in TABS {
            s.tab = tab;
            screen(&s, 100, 12);
            // cramped must not panic
            screen(&s, 12, 3);
        }
        s.message = Some("scanning…".into());
        assert!(screen(&s, 100, 12).contains("scanning"));
    }

    fn terminal_active_control(c: char) -> bool {
        matches!(c, '\u{0000}'..='\u{001f}' | '\u{007f}'..='\u{009f}')
    }

    fn hostile_terminal_text() -> String {
        let esc = char::from_u32(27).expect("ESC");
        let bel = char::from_u32(7).expect("BEL");
        format!("unsafe{esc}]8;;https://evil.test{bel}{esc}[2J")
    }

    #[test]
    fn book_metadata_chapters_and_status_never_reach_the_backend_as_controls() {
        let hostile = hostile_terminal_text();
        let mut book = book(1, "epub");
        book.title = hostile.clone();
        book.author = Some(hostile.clone());
        let mut listening = now(false, 1.0, None, false);
        listening.book = book.clone();
        listening.chapter = Some(hostile.clone());
        let mut state = State {
            continuing: vec![book.clone()],
            library: vec![book],
            now: Some(listening),
            now_at: 1_000,
            message: Some(hostile),
            ..Default::default()
        };
        let mut rendered = String::new();
        for tab in TABS {
            state.tab = tab;
            rendered.push_str(&screen(&state, 180, 12));
        }
        assert!(
            !rendered.chars().any(terminal_active_control),
            "TestBackend must not receive ESC, BEL, or another C0/C1 control"
        );
        assert!(rendered.contains("]8;;https://evil.test") && rendered.contains("[2J"));
    }
}
