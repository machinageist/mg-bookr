#!/usr/bin/env python3
"""The reader window: one book, in the desktop's colours (version 1).

  reader.py --book <id> [--bookr /path/to/mg-bookr]

A separate program on purpose. EPUB is drawn by WebEngine, which needs the argument list Qt
gives a real application: inside Quickshell it dies at once ("the program name is not passed
to QCoreApplication"). PDFs are drawn by QtQuick.Pdf and comics as plain images.

This file is only the host: it starts WebEngine, hands the window one object to talk through,
and runs mg-bookr for everything it knows. mg-bookr stays the authority for the library, the
place in a book and the highlights; the window never touches the database or the files.
Every command is an argv list with a time limit, never a shell.

Colours come from the running shell (`qs -c mgeist ipc call theme json`). With no shell, the
saved theme name and the shell's palettes.json are read instead, and failing that the window
falls back to its own dark palette.
"""
from __future__ import annotations

import argparse
import glob
import json
from pathlib import Path
import subprocess
import sys

from PySide6.QtCore import QObject, QUrl, Slot
from PySide6.QtGui import QGuiApplication
from PySide6.QtQml import QQmlApplicationEngine
from PySide6.QtWebEngineQuick import QtWebEngineQuick

TIMEOUT_SECONDS = 20.0
MAX_OUTPUT = 4 * 1024 * 1024
MAX_QUOTE = 2000
MAX_LOCATION = 200
SHELL_THEME = ["qs", "-c", "mgeist", "ipc", "call", "theme", "json"]
PALETTES = Path.home() / "dotfiles/config/quickshell/mgeist/Theme/palettes.json"
THEME_STATE = str(Path.home() / ".local/state/quickshell/by-shell/*/theme.json")
# palettes.json names its colours for the web site as well as the shell
PALETTE_KEYS = {"bg": "base", "surface": "surface", "text": "fg", "textMuted": "muted",
                "textFaint": "faint", "border": "borderColor", "accent": "accent", "code": "green"}


# Run one program and return what it said on stdout, or None when it could not
def run(argv: list[str]) -> str | None:
    try:
        done = subprocess.run(argv, stdin=subprocess.DEVNULL, capture_output=True,
                              timeout=TIMEOUT_SECONDS)
    except (OSError, subprocess.TimeoutExpired):
        return None
    if len(done.stdout) > MAX_OUTPUT:
        return None
    # mg-bookr prints its JSON envelope on stdout even when it refuses
    return done.stdout.decode("utf-8", "replace")


# The shell's palette, else the saved theme from palettes.json, else nothing and the
# window uses its own colours
def palette() -> str:
    said = run(SHELL_THEME)
    if said and said.strip().startswith("{"):
        return said
    try:
        name = ""
        for state in sorted(glob.glob(THEME_STATE), key=lambda p: Path(p).stat().st_mtime, reverse=True):
            name = json.loads(Path(state).read_text()).get("theme", "")
            if name:
                break
        entries = json.loads(PALETTES.read_text())
        entry = next((e for e in entries if e.get("name") == name), None)
        if entry is None:
            return "{}"
        theme = {ours: entry[theirs] for theirs, ours in PALETTE_KEYS.items() if theirs in entry}
        theme["name"] = name
        theme["dark"] = bool(entry.get("dark", True))
        return json.dumps(theme)
    except (OSError, ValueError, KeyError):
        return "{}"


# What the window is allowed to ask for; every answer comes from mg-bookr
class Library(QObject):
    def __init__(self, book: int, bookr: str) -> None:
        super().__init__()
        self.book, self.bookr = book, bookr

    def _bookr(self, *args: str) -> str:
        return run([self.bookr, "--json", *args]) or ""

    # Everything needed to open the book: its files, spine, chapters and where it was left
    @Slot(result=str)
    def plan(self) -> str:
        return self._bookr("read", str(self.book))

    @Slot(result=str)
    def theme(self) -> str:
        return palette()

    # pages or scroll, as this book was last read
    @Slot(result=str)
    def mode(self) -> str:
        return self._bookr("mode", str(self.book))

    @Slot(str, result=str)
    def setMode(self, mode: str) -> str:
        if mode not in ("pages", "scroll"):
            return ""
        return self._bookr("mode", str(self.book), mode)

    # Where we are now; mg-bookr decides what counts as finished
    @Slot(str, float, result=str)
    def saveProgress(self, location: str, percent: float) -> str:
        if not location or len(location) > MAX_LOCATION:
            return ""
        return self._bookr("progress", str(self.book), location, f"{percent:.4f}")

    # A highlight, which mg-bookr also writes into the vault note
    @Slot(str, str, str, result=str)
    def addHighlight(self, location: str, quote: str, chapter: str) -> str:
        quote = " ".join(quote.split())[:MAX_QUOTE]
        if not quote or not location or len(location) > MAX_LOCATION:
            return ""
        args = ["highlight", "add", str(self.book), location, quote]
        if chapter:
            args += ["--chapter", chapter[:200]]
        return self._bookr(*args)


def main() -> int:
    parser = argparse.ArgumentParser(description="Read one book")
    parser.add_argument("--book", required=True, type=int)
    parser.add_argument("--bookr", default="mg-bookr")
    args = parser.parse_args()
    # WebEngine has to be started before the application object exists
    QtWebEngineQuick.initialize()
    app = QGuiApplication(sys.argv)
    app.setApplicationName("mg-bookr")
    engine = QQmlApplicationEngine()
    library = Library(args.book, args.bookr)
    engine.rootContext().setContextProperty("library", library)
    engine.load(QUrl.fromLocalFile(str(Path(__file__).resolve().parent / "reader.qml")))
    if not engine.rootObjects():
        print("mg-bookr reader: the window could not be built", file=sys.stderr)
        return 1
    return app.exec()


if __name__ == "__main__":
    raise SystemExit(main())
