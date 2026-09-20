<!--
Author: Jeff
Date: 2026-09-19
Description: mg-bookr — ebooks and audiobooks for the Geist suite
-->

# mg-bookr

One library for what you read and what you listen to: EPUB, PDF, comics and audiobooks, kept
in `~/books` and `~/audiobooks`. Local files only — no store, no account, no DRM. Progress,
collections and highlights live in one SQLite file; highlights are also written into mg-vault
as a Markdown note per book.

```sh
mg-bookr scan                              # look through both folders and record what is there
mg-bookr list [--kind epub|pdf|comic|audio] [--missing]
mg-bookr continue [--limit 12]             # started and unfinished, most recent first
mg-bookr show <id>                         # tracks, chapters and highlights
mg-bookr read <id>                         # everything the reader needs (unpacks EPUBs, comics)
mg-bookr open <id>                         # open it in the reader window
mg-bookr mode <id> [pages|scroll]          # how this book is read
mg-bookr progress <id> <location> <percent> [--finished|--unfinished]
mg-bookr highlight add <book> <location> <quote> [--chapter t] [--note n] [--color yellow]
mg-bookr highlight list <book> | note <id> <text> | remove <id>
mg-bookr collection list | show <name> | add <name> <book> | remove <name> <book>
mg-bookr export <id>                       # rewrite the book's vault note
mg-bookr listen play <id> [--at t:secs] [--speed 1.5]
mg-bookr listen now | pause | resume | toggle | stop
mg-bookr listen seek 90|1:02:03|+30|-10    speed 0.75..3   chapter next|prev|<n>
mg-bookr listen sleep 30|chapter|off
mg-bookr tui                               # Continue, Library, Listening
```

`--json` works everywhere, and failures print `{"ok":false,"error":…}` with exit 1.

## What it knows

- **Books** live in `$MG_BOOKR_BOOKS` (default `~/books`) and `$MG_BOOKR_AUDIOBOOKS` (default
  `~/audiobooks`). A scan walks both, eight levels deep, and never follows a symlink. An
  audiobook is one M4B/M4A/MP3 file, or a folder of files read in natural order as one book.
- **Metadata** comes from the file: the OPF for EPUB, `pdfinfo` for PDF, the first image for a
  comic, `ffprobe` tags and chapters for audio. Covers are cached in
  `$XDG_CACHE_HOME/mg-bookr/covers/`. Every helper program is run as an argv list with a time
  limit and an output cap, never through a shell.
- **A book that disappears is marked missing, never deleted**, so progress, highlights and
  collections survive a move or an unplugged drive and come back on the next scan.
- **Where you are** is one location per book: EPUB `spine:fraction`, PDF and comics `page`,
  audio `track:seconds`, plus a percentage. "Continue" is the unfinished books, most recently
  touched first.
- **The library** is `$MG_BOOKR_DB`, else `$XDG_DATA_HOME/mg-bookr/bookr.sqlite`: WAL, foreign
  keys on, and an append-only migration ledger.

## Reading

`mg-bookr open <id>` starts the reader window (`reader/reader.py`) and leaves it running in its
own process group, so closing the terminal does not close the book.

It is a separate Qt program (PySide6), not part of the shell: WebEngine needs the argument list
a real application has, and inside Quickshell it dies at once. The window takes the desktop's
palette as data — it asks the running shell (`qs -c mgeist ipc call theme json`), else reads the
saved theme name with the shell's `palettes.json`, else uses its own dark palette.

- EPUB is drawn by WebEngine with our stylesheet and helpers injected into an isolated world, so
  a book's own scripts can never see them. A page is one screenful: the column and its gap add
  up to the window's width, and the side margin grows so a line never runs past a comfortable
  measure. PDFs come from QtQuick.Pdf; comics are images.
- Keys: `←/→` turn, `t` switches pages and scroll (remembered per book), `+/-` change the size,
  `h` keeps the selected words as a highlight, `n/p` move a chapter, `q` closes.
- The place is saved on a debounce and again as the window closes. The window never touches the
  database: it runs mg-bookr, which stays the authority.

## Listening

`mg-bookr listen play <id>` starts a session: one small mg-bookr process that owns an mpv and
exits with it, so nothing runs while nothing plays. Starting a book pauses mpd music through
mg-streamr. Install `mpv-mpris` for media keys.

The session saves the place every 10 s, on pause, on each track change and at the end. It keeps
each book's own speed (0.75× to 3×, pitch corrected), backs up 10 s when you resume after five
minutes away, and runs the sleep timer — a number of minutes or the end of the chapter — fading
out over its last ten seconds before pausing.

## Highlights in the vault

Every highlight change rewrites one Markdown note, `Books/<slug>-<id>.md`, through mg-vault's
CLI: front matter, then the highlights grouped by chapter in reading order. The note is made
fresh from mg-bookr's records each time and says so, so edits made in the vault are replaced.
Each write names the revision just read (`--expected`), so a change in between is a conflict,
retried once. It needs a registered vault; without one, mg-bookr says so and the highlight is
still saved.

## The shell side

In dotfiles: `Services/Books.qml` with its rules in `BooksState.js`, the Books panel
(`qs -c mgeist ipc call books toggle`), and the launcher's Books, Continue reading and Books TUI
entries.

## Gates

```sh
cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test
node --test reader/tests/rules.test.js
QT_QPA_PLATFORM=offscreen python3 -m unittest reader.tests.test_reader
```

External tools: `bsdtar` (CBZ/CBR), `pdfinfo` and `pdftoppm` (PDF), `ffprobe` and `ffmpeg`
(audio), `mpv` (listening), `python3` with PySide6 and QtWebEngine (the reader window).
