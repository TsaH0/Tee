"""Tests for the multicursor plugin: python3 -m unittest test_multicursor"""

import unittest

from multicursor import (
    add_line, add_match, all_matches, clear, skip_match, to_offset, to_position,
)

TEXT = "foo bar foo\nfoobar foo\n"


def at(*offsets):
    """Selections at (start, end) offset pairs, as Fool sends them."""
    return [{"start": to_position(TEXT, start), "end": to_position(TEXT, end)}
            for start, end in offsets]


class MulticursorTest(unittest.TestCase):
    def test_add_next_selects_the_word_then_its_matches(self):
        # The first press selects the word under the cursor.
        self.assertEqual(add_match(TEXT, at((1, 1)), True), [(0, 3)])
        # Then whole-word matches, so `foobar` is skipped.
        self.assertEqual(add_match(TEXT, at((0, 3)), True), [(0, 3), (8, 11)])
        self.assertEqual(add_match(TEXT, at((0, 3), (8, 11)), True),
                         [(0, 3), (8, 11), (19, 22)])
        # It wraps around, and stops when every match is selected.
        self.assertEqual(add_match(TEXT, at((0, 3), (8, 11), (19, 22)), True),
                         [(0, 3), (8, 11), (19, 22)])

    def test_selected_text_matches_anywhere(self):
        self.assertEqual(add_match(TEXT, at((0, 3), (8, 11)), True), [(0, 3), (8, 11), (19, 22)])
        # A selection that isn't a whole word matches inside words too.
        self.assertEqual(add_match(TEXT, at((1, 3)), True), [(1, 3), (9, 11)])

    def test_add_previous_and_skip(self):
        self.assertEqual(add_match(TEXT, at((19, 22)), False), [(8, 11), (19, 22)])
        self.assertEqual(skip_match(TEXT, at((0, 3), (8, 11))), [(0, 3), (19, 22)])

    def test_all(self):
        self.assertEqual(all_matches(TEXT, at((4, 4))), [(4, 7)])
        self.assertEqual(all_matches(TEXT, at((0, 0))), [(0, 3), (8, 11), (19, 22)])
        # Between spaces there's no word to match.
        blank = [{"start": {"line": 1, "column": 3}, "end": {"line": 1, "column": 3}}]
        self.assertIsNone(all_matches("a   b", blank))

    def test_lines_and_clear(self):
        self.assertEqual(add_line(TEXT, at((5, 5)), True), [(5, 5), (17, 17)])
        self.assertEqual(add_line(TEXT, at((17, 17)), False), [(5, 5), (17, 17)])
        # Short lines take the cursor to their end.
        short = [{"start": {"line": 1, "column": 6}, "end": {"line": 1, "column": 6}}]
        self.assertEqual(add_line("abcdef\nab", short, True), [(5, 5), (9, 9)])
        self.assertEqual(clear(TEXT, at((4, 7), (8, 11))), [(4, 4)])

    def test_positions_round_trip(self):
        for offset in (0, 5, 11, 12, 20):
            self.assertEqual(to_offset(TEXT, to_position(TEXT, offset)), offset)


if __name__ == "__main__":
    unittest.main()
