"""Tests for the Ki mode plugin's selection logic: python3 -m unittest test_ki_mode"""

import unittest

from ki_mode import next_unit, to_offset, to_position, unit_at

TEXT = "let total = price * 2;\nprint(total)\n"


class KiModeTest(unittest.TestCase):
    def test_words(self):
        self.assertEqual(unit_at(TEXT, "word", 5), (4, 9))
        self.assertEqual(next_unit(TEXT, "word", (4, 9), True), (10, 11))
        self.assertEqual(next_unit(TEXT, "word", (4, 9), False), (0, 3))
        self.assertIsNone(next_unit(TEXT, "word", (0, 3), False))

    def test_lines_and_chars(self):
        self.assertEqual(unit_at(TEXT, "line", 3), (0, 22))
        self.assertEqual(next_unit(TEXT, "line", (0, 22), True), (23, 35))
        self.assertEqual(next_unit(TEXT, "char", (0, 1), True), (1, 2))

    def test_positions_round_trip(self):
        for offset in (0, 5, 22, 23, 30):
            self.assertEqual(to_offset(TEXT, to_position(TEXT, offset)), offset)
        self.assertEqual(to_position(TEXT, 23), {"line": 2, "column": 1})


if __name__ == "__main__":
    unittest.main()
