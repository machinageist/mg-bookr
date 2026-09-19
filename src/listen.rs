// Author: Jeff
// Date: 2026-09-19
// Description: Audiobooks through mpv — a listening session, and the controls that steer it
// Notes: `listen play` starts a session: a small mg-bookr process that owns one mpv and exits
//        with it, so nothing runs while nothing plays. The session saves the place every 10 s,
//        on pause, on every track change and at the end. It also remembers the book's speed,
//        runs the sleep timer (fading out over its last 10 s), and backs up 10 s when you
//        resume after 5 minutes away.
//        The controls (pause, seek, speed, chapter, sleep, stop, now) talk to mpv's socket
//        directly. The book id and the sleep request live in mpv's own user-data, so the
//        session and the controls share state without another file.
//        mpv starts paused on the right track and is seeked once the file has loaded: a
//        per-file --start would stick to that track and replay the offset on every return.
//        mpv: $MG_BOOKR_MPV, else `mpv` on PATH. Socket: $XDG_RUNTIME_DIR/mg-bookr/mpv.sock.
//        Starting a book pauses mpd music through mg-streamr, when it is there

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::{Value, json};

use crate::mpv::Mpv;
use crate::reader::Plan;
use crate::store::{Book, Store};

pub const MIN_SPEED: f64 = 0.75;
pub const MAX_SPEED: f64 = 3.0;
const SAVE_EVERY: Duration = Duration::from_secs(10);
const TICK: Duration = Duration::from_secs(1);
// while fading, the volume steps four times a second
const FADE_TICK: Duration = Duration::from_millis(250);
// the sleep timer fades out over this many seconds of real time
const FADE_SECONDS: f64 = 10.0;
// resuming after this long away backs up BACKUP_SECONDS, to pick the thread up again
const AWAY_SECONDS: i64 = 300;
const BACKUP_SECONDS: f64 = 10.0;
const START_WAIT: Duration = Duration::from_secs(5);
const POLL: Duration = Duration::from_millis(50);
const MAX_SLEEP_MINUTES: i64 = 600;
const BOOK_KEY: &str = "user-data/mg-bookr/book";
const SLEEP_KEY: &str = "user-data/mg-bookr/sleep";
const MUSIC_TIMEOUT: Duration = Duration::from_secs(3);
// property-change ids the session observes
const OBSERVE_PAUSE: u64 = 1;
const OBSERVE_TRACK: u64 = 2;
const OBSERVE_CHAPTER: u64 = 3;
const OBSERVE_SPEED: u64 = 4;
const OBSERVE_SLEEP: u64 = 5;

// ── Rules ──

// "2:1234.5" → track 2, 1234.5 s into it
pub fn parse_location(location: &str) -> Option<(usize, f64)> {
    let (track, seconds) = location.split_once(':')?;
    let seconds: f64 = seconds.parse().ok()?;
    (seconds.is_finite() && seconds >= 0.0).then_some((track.parse().ok()?, seconds))
}

// track 2, 1234.5 s → "2:1234.5"
pub fn location(track: usize, seconds: f64) -> String {
    format!("{track}:{:.1}", seconds.max(0.0))
}

// How far through the whole book: the tracks before this one, plus the time into it
pub fn percent(durations: &[Option<f64>], track: usize, seconds: f64) -> f64 {
    let total: f64 = durations.iter().map(|d| d.unwrap_or(0.0)).sum();
    if total <= 0.0 {
        return 0.0;
    }
    let before: f64 = durations.iter().take(track).map(|d| d.unwrap_or(0.0)).sum();
    ((before + seconds) / total * 100.0).clamp(0.0, 100.0)
}

// Where to start: the saved place, backed up a little after a long time away; a finished
// book (or a place that no longer fits it) starts again from the top
pub fn start_point(book: &Book, tracks: usize, now: DateTime<Utc>) -> (usize, f64) {
    if book.finished {
        return (0, 0.0);
    }
    let Some((track, seconds)) = book.location.as_deref().and_then(parse_location) else {
        return (0, 0.0);
    };
    if track >= tracks {
        return (0, 0.0);
    }
    let away = book
        .updated_at
        .as_deref()
        .and_then(|t| DateTime::parse_from_rfc3339(t).ok())
        .is_some_and(|t| (now - t.with_timezone(&Utc)).num_seconds() >= AWAY_SECONDS);
    let seconds = if away {
        (seconds - BACKUP_SECONDS).max(0.0)
    } else {
        seconds
    };
    (track, seconds)
}

// A speed mg-bookr plays at: 0.75× to 3×
pub fn check_speed(speed: f64) -> Result<f64> {
    if !(MIN_SPEED..=MAX_SPEED).contains(&speed) {
        bail!("a speed is {MIN_SPEED} to {MAX_SPEED}")
    }
    Ok(speed)
}

// "+30" and "-10" move from here; "90", "1:30" and "1:02:03" go to that time in the track
pub fn parse_seek(words: &str) -> Result<(f64, bool)> {
    let words = words.trim();
    let relative = words.starts_with(['+', '-']);
    let mut seconds = 0.0;
    for part in words.trim_start_matches(['+', '-']).split(':') {
        let n: f64 = part
            .parse()
            .map_err(|_| anyhow!("a time is 90, 1:30, 1:02:03, +30 or -10"))?;
        seconds = seconds * 60.0 + n;
    }
    if !seconds.is_finite() {
        bail!("a time is 90, 1:30, 1:02:03, +30 or -10")
    }
    Ok((
        if words.starts_with('-') {
            -seconds
        } else {
            seconds
        },
        relative,
    ))
}

// When the sleep timer stops play
#[derive(Debug, Clone, PartialEq)]
pub enum Sleep {
    Off,
    // a moment, in unix milliseconds
    At(i64),
    EndOfChapter,
}

impl Sleep {
    // Read mpv's user-data text: "", "at:<unix ms>" or "chapter"
    pub fn parse(text: &str) -> Sleep {
        match text {
            "chapter" => Sleep::EndOfChapter,
            _ => text
                .strip_prefix("at:")
                .and_then(|ms| ms.parse().ok())
                .map_or(Sleep::Off, Sleep::At),
        }
    }

    // The user-data text for this timer
    pub fn text(&self) -> String {
        match self {
            Sleep::Off => String::new(),
            Sleep::At(ms) => format!("at:{ms}"),
            Sleep::EndOfChapter => "chapter".into(),
        }
    }

    // What you asked for: minutes, "chapter" or "off"
    pub fn request(words: &str, now_ms: i64) -> Result<Sleep> {
        match words.trim() {
            "off" => Ok(Sleep::Off),
            "chapter" => Ok(Sleep::EndOfChapter),
            minutes => match minutes.parse::<i64>() {
                Ok(m) if (1..=MAX_SLEEP_MINUTES).contains(&m) => Ok(Sleep::At(now_ms + m * 60_000)),
                _ => bail!("sleep takes 1–{MAX_SLEEP_MINUTES} minutes, chapter or off"),
            },
        }
    }
}

// Where the current chapter ends, in seconds into the file: the next chapter's start, else the
// file's end (a file without chapters is one chapter)
pub fn chapter_end(starts: &[f64], chapter: Option<usize>, file_duration: f64) -> f64 {
    chapter
        .and_then(|c| starts.get(c + 1))
        .copied()
        .unwrap_or(file_duration)
}

// Real seconds before the sleep timer stops play, or None when it is off
pub fn sleep_left(
    sleep: &Sleep,
    now_ms: i64,
    position: f64,
    chapter_end: f64,
    speed: f64,
) -> Option<f64> {
    match sleep {
        Sleep::Off => None,
        Sleep::At(ms) => Some((ms - now_ms) as f64 / 1000.0),
        Sleep::EndOfChapter => Some((chapter_end - position) / speed.max(MIN_SPEED)),
    }
}

// The volume while fading: unchanged until the last FADE_SECONDS, then down to nothing
pub fn faded(volume: f64, left: f64) -> f64 {
    volume * (left / FADE_SECONDS).clamp(0.0, 1.0)
}

// ── Where things are ──

// $XDG_RUNTIME_DIR/mg-bookr, owner-only: the socket lives here
fn runtime_dir() -> Result<PathBuf> {
    let dir =
        PathBuf::from(std::env::var_os("XDG_RUNTIME_DIR").context("XDG_RUNTIME_DIR is not set")?)
            .join("mg-bookr");
    fs::create_dir_all(&dir)?;
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o700))?;
    Ok(dir)
}

pub fn socket_path() -> Result<PathBuf> {
    Ok(runtime_dir()?.join("mpv.sock"))
}

fn mpv_binary() -> PathBuf {
    std::env::var_os("MG_BOOKR_MPV").map_or_else(|| PathBuf::from("mpv"), PathBuf::from)
}

fn now_ms() -> i64 {
    Utc::now().timestamp_millis()
}

// ── Controls ──

// The session's mpv, or a plain "nothing is playing"
fn connect() -> Result<Mpv> {
    Mpv::connect(&socket_path()?).map_err(|_| anyhow!("nothing is playing"))
}

// What is playing now, for the CLI and the shell
#[derive(Debug, Serialize)]
pub struct Now {
    pub book: Book,
    pub track: usize,
    pub tracks: usize,
    // seconds into the current track, and its length
    pub position: f64,
    pub duration: f64,
    pub percent: f64,
    pub chapter: Option<String>,
    pub speed: f64,
    pub paused: bool,
    // real seconds until the sleep timer stops play (a timed sleep only)
    pub sleep_left: Option<f64>,
    pub sleep_chapter: bool,
}

// What is playing now; None when nothing is
pub fn now(store: &Store) -> Result<Option<Now>> {
    let Ok(mut mpv) = connect() else {
        return Ok(None);
    };
    let Some(id) = mpv.get(BOOK_KEY)?.as_i64() else {
        return Ok(None);
    };
    let track = mpv.get("playlist-pos")?.as_u64().unwrap_or(0) as usize;
    let position = mpv.get("time-pos")?.as_f64().unwrap_or(0.0);
    let durations: Vec<Option<f64>> = store.tracks(id)?.into_iter().map(|(_, d)| d).collect();
    let chapter = match mpv.get("chapter")?.as_i64() {
        Some(c) if c >= 0 => mpv
            .get(&format!("chapter-list/{c}/title"))?
            .as_str()
            .map(String::from),
        _ => None,
    };
    let sleep = Sleep::parse(mpv.get(SLEEP_KEY)?.as_str().unwrap_or(""));
    Ok(Some(Now {
        book: store.book(id)?,
        track,
        tracks: mpv.get("playlist-count")?.as_u64().unwrap_or(0) as usize,
        position,
        duration: mpv.get("duration")?.as_f64().unwrap_or(0.0),
        percent: percent(&durations, track, position),
        chapter,
        speed: mpv.get("speed")?.as_f64().unwrap_or(1.0),
        paused: mpv.get("pause")?.as_bool().unwrap_or(true),
        sleep_left: match sleep {
            Sleep::At(ms) => Some(((ms - now_ms()) as f64 / 1000.0).max(0.0)),
            _ => None,
        },
        sleep_chapter: sleep == Sleep::EndOfChapter,
    }))
}

// Pause, resume, or (None) flip; returns whether it is paused now
pub fn pause(on: Option<bool>) -> Result<bool> {
    let mut mpv = connect()?;
    let paused = match on {
        Some(on) => on,
        None => !mpv.get("pause")?.as_bool().unwrap_or(false),
    };
    mpv.set("pause", json!(paused))?;
    Ok(paused)
}

// Move within the current track
pub fn seek(words: &str) -> Result<()> {
    let (seconds, relative) = parse_seek(words)?;
    let flags = if relative {
        "relative+exact"
    } else {
        "absolute+exact"
    };
    connect()?.command(json!(["seek", seconds, flags]))?;
    Ok(())
}

// Change speed; the session remembers it for the book
pub fn speed(speed: f64) -> Result<f64> {
    connect()?.set("speed", json!(check_speed(speed)?))?;
    Ok(speed)
}

// "next", "prev" or a number from 1: chapters when the file has them, else tracks
pub fn chapter(words: &str) -> Result<()> {
    let mut mpv = connect()?;
    let chapters = mpv.get("chapter-list/count")?.as_i64().unwrap_or(0);
    let command = match (words, chapters > 0) {
        ("next", true) => json!(["add", "chapter", 1]),
        ("prev", true) => json!(["add", "chapter", -1]),
        ("next", false) => json!(["playlist-next"]),
        ("prev", false) => json!(["playlist-prev"]),
        (n, has_chapters) => {
            let n: i64 = n
                .parse()
                .map_err(|_| anyhow!("a chapter is next, prev or a number"))?;
            let count = if has_chapters {
                chapters
            } else {
                mpv.get("playlist-count")?.as_i64().unwrap_or(0)
            };
            if !(1..=count).contains(&n) {
                bail!(
                    "there are {count} {}",
                    if has_chapters { "chapters" } else { "tracks" }
                )
            }
            if has_chapters {
                json!(["set_property", "chapter", n - 1])
            } else {
                json!(["playlist-play-index", n - 1])
            }
        }
    };
    mpv.command(command)?;
    Ok(())
}

// Set the sleep timer: minutes, "chapter" or "off"
pub fn sleep(words: &str) -> Result<Sleep> {
    let sleep = Sleep::request(words, now_ms())?;
    connect()?.set(SLEEP_KEY, json!(sleep.text()))?;
    Ok(sleep)
}

// Stop listening; the session saves the place as mpv goes
pub fn stop() -> Result<()> {
    connect()?.command(json!(["quit"]))?;
    wait_gone()
}

// Wait for the session's mpv to be gone
fn wait_gone() -> Result<()> {
    let started = Instant::now();
    while connect().is_ok() {
        if started.elapsed() > START_WAIT {
            bail!("mpv did not stop")
        }
        std::thread::sleep(POLL);
    }
    Ok(())
}

// Starting a book pauses the music (mpd, through mg-streamr); no music, or no mg-streamr, is fine
fn pause_music() {
    let binary = crate::tools::suite_binary("MG_STREAMR_BIN", "mg-streamr", "mg-streamr");
    if let Some(binary) = binary.to_str() {
        let _ = crate::tools::run_status(&[binary, "pause"], MUSIC_TIMEOUT, 64 * 1024);
    }
}

// Play a book: carry on if it is the one playing, else start a session for it.
// `session` is the argv that runs `mg-bookr listen session …` (with --at/--speed when asked)
pub fn play(plan: &Plan, session: &[String]) -> Result<()> {
    if plan.tracks.is_empty() {
        bail!("{} is not an audiobook", plan.book.title)
    }
    if let Ok(mut mpv) = connect() {
        if mpv.get(BOOK_KEY)?.as_i64() == Some(plan.book.id) {
            return mpv.set("pause", json!(false));
        }
        mpv.command(json!(["quit"]))?;
        wait_gone()?;
    }
    pause_music();
    let (program, args) = session.split_first().context("no session command")?;
    let log = fs::File::create(runtime_dir()?.join("session.log"))?;
    Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(log)
        // its own process group: closing the terminal that started it does not stop the book
        .process_group(0)
        .spawn()
        .context("could not start the listening session")?;
    let started = Instant::now();
    loop {
        if let Ok(Some(now)) = now_playing_id()
            && now == plan.book.id
        {
            return Ok(());
        }
        if started.elapsed() > START_WAIT * 2 {
            bail!("the book did not start; see $XDG_RUNTIME_DIR/mg-bookr/session.log")
        }
        std::thread::sleep(POLL);
    }
}

// The id of the book playing and unpaused, if any
fn now_playing_id() -> Result<Option<i64>> {
    let mut mpv = connect()?;
    let playing = mpv.get("pause")? == json!(false);
    Ok(mpv.get(BOOK_KEY)?.as_i64().filter(|_| playing))
}

// ── The session ──

// Run one listening session: start mpv on the book, keep the place until mpv exits
pub fn session(
    store: &Store,
    plan: &Plan,
    at: Option<(usize, f64)>,
    speed: Option<f64>,
) -> Result<()> {
    let id = plan.book.id;
    let (track, seconds) =
        at.unwrap_or_else(|| start_point(&plan.book, plan.tracks.len(), Utc::now()));
    if track >= plan.tracks.len() {
        bail!("{} has {} tracks", plan.book.title, plan.tracks.len())
    }
    let speed = check_speed(speed.or(store.speed(id)?).unwrap_or(1.0))?;
    let socket = socket_path()?;
    let _ = fs::remove_file(&socket);
    let mut child = Command::new(mpv_binary())
        .args([
            "--no-video",
            "--no-terminal",
            "--idle=no",
            "--pause",
            "--title=mg-bookr",
            &format!("--input-ipc-server={}", socket.display()),
            &format!("--speed={speed}"),
            &format!("--playlist-start={track}"),
            "--",
        ])
        .args(&plan.tracks)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("mpv is not available")?;
    let kept = keep(store, plan, &mut child, track, seconds, speed);
    // on a failure mpv may still be running; never leave it behind
    if kept.is_err() {
        let _ = child.kill();
    }
    let _ = child.wait();
    kept
}

// What the session knows about the book as it plays
struct Keeper<'a> {
    store: &'a Store,
    id: i64,
    durations: Vec<Option<f64>>,
    track: usize,
    position: f64,
    duration: f64,
    // chapter start times in the current file
    starts: Vec<f64>,
    chapter: Option<usize>,
    speed: f64,
    paused_at: Option<Instant>,
    sleep: Sleep,
    // the chapter (or track) that was playing when "sleep until the chapter ends" was asked
    sleep_mark: (usize, Option<usize>),
    // the volume before the fade began
    fade_from: Option<f64>,
    finished: bool,
    last_save: Instant,
}

// Connect to the new mpv, start it at the saved place, then keep that place until it exits
fn keep(
    store: &Store,
    plan: &Plan,
    child: &mut Child,
    track: usize,
    seconds: f64,
    speed: f64,
) -> Result<()> {
    let socket = socket_path()?;
    let started = Instant::now();
    let mut mpv = loop {
        if let Ok(mpv) = Mpv::connect(&socket) {
            break mpv;
        }
        if child.try_wait()?.is_some() || started.elapsed() > START_WAIT {
            bail!("mpv did not start")
        }
        std::thread::sleep(POLL);
    };
    mpv.set(BOOK_KEY, json!(plan.book.id))?;
    // wait for the track to load, then go to the place and play
    while mpv.get("duration")?.as_f64().is_none()
        || mpv.get("playlist-pos")?.as_u64() != Some(track as u64)
    {
        if started.elapsed() > START_WAIT {
            bail!("mpv did not load {}", plan.tracks[track])
        }
        std::thread::sleep(POLL);
    }
    if seconds > 0.0 {
        mpv.command(json!(["seek", seconds, "absolute+exact"]))?;
    }
    let mut keeper = Keeper {
        store,
        id: plan.book.id,
        durations: store
            .tracks(plan.book.id)?
            .into_iter()
            .map(|(_, d)| d)
            .collect(),
        track,
        position: seconds,
        duration: 0.0,
        starts: Vec::new(),
        chapter: None,
        speed,
        paused_at: None,
        sleep: Sleep::Off,
        sleep_mark: (track, None),
        fade_from: None,
        finished: false,
        last_save: Instant::now(),
    };
    keeper.load_file(&mut mpv)?;
    for (id, name) in [
        (OBSERVE_PAUSE, "pause"),
        (OBSERVE_TRACK, "playlist-pos"),
        (OBSERVE_CHAPTER, "chapter"),
        (OBSERVE_SPEED, "speed"),
        (OBSERVE_SLEEP, SLEEP_KEY),
    ] {
        mpv.observe(id, name)?;
    }
    mpv.set("pause", json!(false))?;
    let mut last_tick = Instant::now();
    loop {
        let wait = if keeper.fade_from.is_some() {
            FADE_TICK
        } else {
            TICK
        };
        match mpv.next_event(wait) {
            // mpv has gone: the book was stopped or came to its end
            Err(_) => break,
            Ok(Some(event)) => {
                if keeper.event(&mut mpv, &event).is_err() {
                    break;
                }
            }
            Ok(None) => {}
        }
        if last_tick.elapsed() >= wait {
            last_tick = Instant::now();
            if keeper.tick(&mut mpv).is_err() {
                break;
            }
        }
    }
    keeper.save()
}

impl Keeper<'_> {
    // A new file is playing: learn its length and chapters
    fn load_file(&mut self, mpv: &mut Mpv) -> Result<()> {
        self.duration = mpv.get("duration")?.as_f64().unwrap_or(0.0);
        let count = mpv.get("chapter-list/count")?.as_u64().unwrap_or(0);
        self.starts = (0..count)
            .map(|c| {
                mpv.get(&format!("chapter-list/{c}/time"))
                    .map(|t| t.as_f64().unwrap_or(0.0))
            })
            .collect::<Result<_>>()?;
        Ok(())
    }

    // Save the place (as finished when the last track ran out)
    fn save(&self) -> Result<()> {
        if self.finished {
            let last = self.durations.len().saturating_sub(1);
            let end = self
                .durations
                .get(last)
                .copied()
                .flatten()
                .unwrap_or(self.position);
            return self
                .store
                .set_progress(self.id, &location(last, end), 100.0, Some(true));
        }
        let percent = percent(&self.durations, self.track, self.position);
        self.store.set_progress(
            self.id,
            &location(self.track, self.position),
            percent,
            Some(false),
        )
    }

    // One event from mpv
    fn event(&mut self, mpv: &mut Mpv, event: &Value) -> Result<()> {
        match event["event"].as_str() {
            Some("property-change") => {}
            Some("end-file") => {
                let last = self.track + 1 >= self.durations.len();
                if event["reason"] == "eof" && last {
                    self.finished = true;
                }
                return Ok(());
            }
            _ => return Ok(()),
        }
        let data = &event["data"];
        match event["id"].as_u64() {
            Some(OBSERVE_PAUSE) => {
                if data.as_bool() == Some(true) {
                    self.paused_at = Some(Instant::now());
                    self.read_position(mpv)?;
                    self.save()?;
                } else if let Some(at) = self.paused_at.take()
                    && at.elapsed().as_secs() as i64 >= AWAY_SECONDS
                {
                    mpv.command(json!(["seek", -BACKUP_SECONDS, "relative+exact"]))?;
                }
            }
            Some(OBSERVE_TRACK) => {
                let Some(track) = data.as_u64().map(|t| t as usize) else {
                    return Ok(());
                };
                if track != self.track {
                    // the chapter the timer was waiting for has ended: a file is a chapter too
                    if self.sleep == Sleep::EndOfChapter {
                        self.stop_for_sleep(mpv)?;
                    }
                    self.track = track;
                    self.position = 0.0;
                    self.load_file(mpv)?;
                    self.save()?;
                }
            }
            Some(OBSERVE_CHAPTER) => {
                let chapter = data.as_i64().and_then(|c| usize::try_from(c).ok());
                let moved_on = (self.track, chapter) != self.sleep_mark;
                if self.sleep == Sleep::EndOfChapter && moved_on && self.sleep_mark.1.is_some() {
                    self.stop_for_sleep(mpv)?;
                }
                self.chapter = chapter;
            }
            Some(OBSERVE_SPEED) => {
                if let Some(speed) = data.as_f64() {
                    self.speed = speed;
                    self.store.set_speed(self.id, speed)?;
                }
            }
            Some(OBSERVE_SLEEP) => {
                self.sleep = Sleep::parse(data.as_str().unwrap_or(""));
                self.sleep_mark = (self.track, self.chapter);
                // switched off mid-fade: put the volume back
                if self.sleep == Sleep::Off {
                    self.restore_volume(mpv)?;
                }
            }
            _ => {}
        }
        Ok(())
    }

    // Every second (four times a second while fading): the place, the timer, the save
    fn tick(&mut self, mpv: &mut Mpv) -> Result<()> {
        self.read_position(mpv)?;
        let end = chapter_end(&self.starts, self.chapter, self.duration);
        if let Some(left) = sleep_left(&self.sleep, now_ms(), self.position, end, self.speed) {
            if left <= FADE_SECONDS {
                let from = match self.fade_from {
                    Some(v) => v,
                    None => {
                        let v = mpv.get("volume")?.as_f64().unwrap_or(100.0);
                        self.fade_from = Some(v);
                        v
                    }
                };
                mpv.set("volume", json!(faded(from, left)))?;
            }
            if left <= 0.0 {
                self.stop_for_sleep(mpv)?;
            }
        }
        if self.paused_at.is_none() && self.last_save.elapsed() >= SAVE_EVERY {
            self.save()?;
            self.last_save = Instant::now();
        }
        Ok(())
    }

    fn read_position(&mut self, mpv: &mut Mpv) -> Result<()> {
        if let Some(position) = mpv.get("time-pos")?.as_f64() {
            self.position = position;
        }
        Ok(())
    }

    // The timer is up: pause, put the volume back for next time, clear the timer
    fn stop_for_sleep(&mut self, mpv: &mut Mpv) -> Result<()> {
        self.sleep = Sleep::Off;
        mpv.set("pause", json!(true))?;
        self.restore_volume(mpv)?;
        mpv.set(SLEEP_KEY, json!(""))
    }

    fn restore_volume(&mut self, mpv: &mut Mpv) -> Result<()> {
        if let Some(volume) = self.fade_from.take() {
            mpv.set("volume", json!(volume))?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn book(location: Option<&str>, updated: Option<&str>, finished: bool) -> Book {
        Book {
            id: 1,
            kind: "audio-folder".into(),
            root: "audiobooks".into(),
            path: "b".into(),
            title: "B".into(),
            author: None,
            series: None,
            series_index: None,
            cover: None,
            pages: None,
            duration_seconds: None,
            missing: false,
            location: location.map(String::from),
            percent: None,
            finished,
            updated_at: updated.map(String::from),
        }
    }

    #[test]
    fn locations_round_trip_and_percent_spans_the_tracks() {
        assert_eq!(parse_location("2:1234.5"), Some((2, 1234.5)));
        assert_eq!(parse_location(&location(3, 61.26)), Some((3, 61.3)));
        assert_eq!(parse_location("x:1"), None);
        assert_eq!(parse_location("1:-4"), None);
        assert_eq!(parse_location("1:NaN"), None);
        let durations = [Some(100.0), Some(300.0), None];
        assert_eq!(percent(&durations, 1, 100.0), 50.0);
        assert_eq!(percent(&durations, 2, 1e9), 100.0);
        assert_eq!(
            percent(&[None], 0, 10.0),
            0.0,
            "unknown lengths give no percent"
        );
    }

    #[test]
    fn a_book_starts_where_it_was_left_backed_up_after_time_away() {
        let now = Utc.with_ymd_and_hms(2026, 9, 19, 12, 0, 0).unwrap();
        let recent = "2026-09-19T11:58:00+00:00";
        let long_ago = "2026-09-19T11:00:00+00:00";
        assert_eq!(
            start_point(&book(Some("1:95"), Some(recent), false), 3, now),
            (1, 95.0)
        );
        assert_eq!(
            start_point(&book(Some("1:95"), Some(long_ago), false), 3, now),
            (1, 85.0)
        );
        assert_eq!(
            start_point(&book(Some("1:4"), Some(long_ago), false), 3, now),
            (1, 0.0)
        );
        assert_eq!(
            start_point(&book(Some("1:95"), Some(recent), true), 3, now),
            (0, 0.0),
            "finished starts over"
        );
        assert_eq!(
            start_point(&book(Some("7:95"), Some(recent), false), 3, now),
            (0, 0.0),
            "the book changed"
        );
        assert_eq!(start_point(&book(None, None, false), 3, now), (0, 0.0));
    }

    #[test]
    fn speeds_and_seeks() {
        assert!(check_speed(0.75).is_ok() && check_speed(3.0).is_ok());
        assert!(check_speed(0.5).is_err() && check_speed(f64::NAN).is_err());
        assert_eq!(parse_seek("+30").unwrap(), (30.0, true));
        assert_eq!(parse_seek("-10").unwrap(), (-10.0, true));
        assert_eq!(parse_seek("90").unwrap(), (90.0, false));
        assert_eq!(parse_seek("1:02:03").unwrap(), (3723.0, false));
        assert_eq!(parse_seek("-1:30").unwrap(), (-90.0, true));
        assert!(
            parse_seek("soon").is_err() && parse_seek("").is_err() && parse_seek("1e999").is_err()
        );
    }

    #[test]
    fn the_sleep_timer_reads_its_words_and_counts_down() {
        let now = 1_000_000;
        assert_eq!(
            Sleep::request("30", now).unwrap(),
            Sleep::At(now + 1_800_000)
        );
        assert_eq!(Sleep::request("chapter", now).unwrap(), Sleep::EndOfChapter);
        assert_eq!(Sleep::request("off", now).unwrap(), Sleep::Off);
        assert!(Sleep::request("0", now).is_err() && Sleep::request("601", now).is_err());
        for s in [Sleep::Off, Sleep::At(5), Sleep::EndOfChapter] {
            assert_eq!(Sleep::parse(&s.text()), s);
        }
        assert_eq!(Sleep::parse("at:soon"), Sleep::Off);
        assert_eq!(
            sleep_left(&Sleep::At(now + 4000), now, 0.0, 0.0, 1.0),
            Some(4.0)
        );
        // 30 s of chapter left at 1.5× is 20 s of real time
        assert_eq!(
            sleep_left(&Sleep::EndOfChapter, now, 70.0, 100.0, 1.5),
            Some(20.0)
        );
        assert_eq!(sleep_left(&Sleep::Off, now, 0.0, 0.0, 1.0), None);
    }

    #[test]
    fn chapters_end_at_the_next_start_and_the_fade_runs_out_with_the_timer() {
        let starts = [0.0, 120.0, 300.0];
        assert_eq!(chapter_end(&starts, Some(0), 400.0), 120.0);
        assert_eq!(
            chapter_end(&starts, Some(2), 400.0),
            400.0,
            "the last chapter ends with the file"
        );
        assert_eq!(
            chapter_end(&[], None, 400.0),
            400.0,
            "a file without chapters is one"
        );
        assert_eq!(faded(80.0, 60.0), 80.0);
        assert_eq!(faded(80.0, 5.0), 40.0);
        assert_eq!(faded(80.0, -1.0), 0.0);
    }
}
