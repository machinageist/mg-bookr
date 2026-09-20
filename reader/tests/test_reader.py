#!/usr/bin/env python3
"""Regression tests for the reader's host: what it asks mg-bookr, and where colours come from."""

from __future__ import annotations

import importlib.machinery
import importlib.util
import json
import os
import pathlib
import sys
import tempfile
import unittest
from unittest import mock

SCRIPT = pathlib.Path(__file__).resolve().parents[1] / "reader.py"
SPEC = importlib.util.spec_from_file_location(
    "mg_bookr_reader",
    SCRIPT,
    loader=importlib.machinery.SourceFileLoader("mg_bookr_reader", str(SCRIPT)),
)
assert SPEC is not None and SPEC.loader is not None
READER = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(READER)


class AskTests(unittest.TestCase):
    def setUp(self) -> None:
        self.asked: list[list[str]] = []
        patch = mock.patch.object(READER, "run", lambda argv: self.asked.append(argv) or '{"ok":true}')
        patch.start()
        self.addCleanup(patch.stop)
        self.library = READER.Library(7, "/opt/mg-bookr")

    def test_every_question_goes_to_mg_bookr_as_json(self) -> None:
        self.library.plan()
        self.library.mode()
        self.library.setMode("scroll")
        self.library.saveProgress("3:0.5000", 42.125)
        self.assertEqual(self.asked, [
            ["/opt/mg-bookr", "--json", "read", "7"],
            ["/opt/mg-bookr", "--json", "mode", "7"],
            ["/opt/mg-bookr", "--json", "mode", "7", "scroll"],
            ["/opt/mg-bookr", "--json", "progress", "7", "3:0.5000", "42.1250"],
        ])

    def test_a_highlight_carries_its_chapter_and_is_tidied_first(self) -> None:
        self.library.addHighlight("2:0.1", "  truth is  a matter\nof the imagination ", "One")
        self.assertEqual(self.asked[0][2:], [
            "highlight", "add", "7", "2:0.1", "truth is a matter of the imagination", "--chapter", "One",
        ])
        self.library.addHighlight("2:0.1", "x" * (READER.MAX_QUOTE + 500), "")
        self.assertEqual(len(self.asked[1][6]), READER.MAX_QUOTE, "a quote has a limit")
        self.assertNotIn("--chapter", self.asked[1])

    def test_nothing_useless_is_sent(self) -> None:
        self.assertEqual(self.library.addHighlight("2:0.1", "   ", ""), "")
        self.assertEqual(self.library.saveProgress("", 10.0), "")
        self.assertEqual(self.library.saveProgress("x" * 500, 10.0), "")
        self.assertEqual(self.library.setMode("sideways"), "")
        self.assertEqual(self.asked, [])


class PaletteTests(unittest.TestCase):
    def test_the_shell_is_asked_first(self) -> None:
        with mock.patch.object(READER, "run", lambda argv: '{"name":"fog","base":"#1d2124"}'):
            self.assertEqual(json.loads(READER.palette())["name"], "fog")

    def test_without_a_shell_the_saved_theme_is_read_from_palettes_json(self) -> None:
        temp = tempfile.TemporaryDirectory(prefix="mg-bookr-reader-")
        self.addCleanup(temp.cleanup)
        home = pathlib.Path(temp.name)
        palettes = home / "palettes.json"
        palettes.write_text(json.dumps([
            {"name": "fog", "bg": "#1d2124", "surface": "#24292d", "text": "#dde2e5",
             "textMuted": "#a2abb1", "textFaint": "#869096", "border": "#384047",
             "accent": "#9bb3bf", "code": "#a3b89c", "dark": True},
            {"name": "other", "bg": "#000000"},
        ]))
        state = home / "state"
        state.mkdir()
        (state / "theme.json").write_text('{"theme": "fog"}')
        with mock.patch.object(READER, "run", lambda argv: None), \
                mock.patch.object(READER, "PALETTES", palettes), \
                mock.patch.object(READER, "THEME_STATE", str(state / "*.json")):
            theme = json.loads(READER.palette())
        self.assertEqual(theme["name"], "fog")
        self.assertEqual(theme["base"], "#1d2124", "palettes.json calls it bg")
        self.assertEqual(theme["fg"], "#dde2e5")
        self.assertEqual(theme["borderColor"], "#384047")
        self.assertTrue(theme["dark"])

    def test_with_nothing_to_read_the_window_uses_its_own_colours(self) -> None:
        with mock.patch.object(READER, "run", lambda argv: None), \
                mock.patch.object(READER, "PALETTES", pathlib.Path("/nowhere/palettes.json")), \
                mock.patch.object(READER, "THEME_STATE", "/nowhere/*.json"):
            self.assertEqual(READER.palette(), "{}")


class RunTests(unittest.TestCase):
    def test_a_program_that_is_not_there_is_not_fatal(self) -> None:
        self.assertIsNone(READER.run(["mg-bookr-no-such-program"]))

    def test_output_comes_back_whether_it_succeeded_or_refused(self) -> None:
        self.assertEqual(READER.run([sys.executable, "-c", "print('hi')"]), "hi\n")
        said = READER.run([sys.executable, "-c", "import sys; print('{\"ok\":false}'); sys.exit(1)"])
        self.assertEqual(said, '{"ok":false}\n')

    def test_a_flood_is_refused(self) -> None:
        flood = f"print('x' * {READER.MAX_OUTPUT + 10})"
        self.assertIsNone(READER.run([sys.executable, "-c", flood]))


if __name__ == "__main__":
    os.environ.setdefault("QT_QPA_PLATFORM", "offscreen")
    unittest.main()
