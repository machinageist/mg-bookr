<!--
Author: Jeff
Date: 2026-09-19
Description: mg-bookr MVP — ebooks and audiobooks, Apple Books without the store
Notes: Geistos cycle 02, slice BK. Decided with Jeff 2026-09-18/19; the cycle SPEC in dotfiles
       (AGENTS/changes/geistos-luna-cycle-02/SPEC.md, "BK") holds the full decision list
-->

# mg-bookr MVP

One library for ebooks and audiobooks, with clear progress, collections, a clean themed
reader, and highlights that land in the vault. Local folders only, DRM-free, no store.

## Decisions (Jeff)

- Formats: EPUB, PDF, comics (CBZ/CBR), audiobooks (a single M4B/M4A, or a folder of audio
  files as one book). Folders: `$MG_BOOKR_BOOKS` (default `~/books`) and `$MG_BOOKR_AUDIOBOOKS`
  (default `~/audiobooks`).
- Audiobooks play through **mpv** (JSON IPC): speed 0.75–3× with natural pitch, chapters, and a
  sleep timer. mpv-mpris gives media keys.
- Reading happens in **its own process**: a small Quickshell program sharing the desktop Theme.
  EPUB is drawn by WebEngine with the theme injected, PDF by QtQuick.Pdf, comics as images.
  Pages or continuous scroll, switchable per book.
- Highlights and notes go to **mg-vault** as one Markdown note per book, regenerated from
  mg-bookr's records on each change (read the revision, then write with `--expected`).
- Same grammar as the suite: Rust core, CLI with `--json`, ratatui TUI (library and
  audiobooks, not reading), SQLite in `$XDG_DATA_HOME/mg-bookr/`, teaching comments.

## Behaviour

- `scan` walks both folders (depth-limited, symlinks not followed) and records each book by
  its kind and path. A book whose file disappears is marked missing, never deleted, so its
  progress and highlights survive a move and a rescan. Metadata:
  - EPUB: OPF title, author, series, and the cover image.
  - PDF: `pdfinfo` title, author and pages; the first page as the cover (`pdftoppm`).
  - Comics: first image by natural order as the cover; page count. CBR is read through `bsdtar`.
  - Audiobooks: `ffprobe` tags, duration and chapters; the embedded or folder cover.
  External tools always get argv lists and fixed flags, never a shell.
- Covers are cached under `$XDG_CACHE_HOME/mg-bookr/covers/`.
- Progress is one location per book: EPUB `spine:fraction`, PDF and comics `page`, audio
  `track:seconds`. Plus a percent and when it was last touched: "continue reading" is the most
  recently touched unfinished books.
- Collections: named lists of books.

## Acceptance

- Gates: `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test`.
- Tests: fixture EPUB/CBZ built in the tests; scan, metadata, missing-and-back, progress,
  highlights, collections, vault note text, the mpv IPC protocol against a fake socket.
- Live: a real EPUB, PDF, comic and audiobook in the folders; reading, highlighting into the
  vault, and audiobook speed/chapter/sleep.
