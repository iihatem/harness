import unittest

from app import dedupe


class DedupeTest(unittest.TestCase):
    def test_keeps_first_appearance_order(self):
        self.assertEqual(dedupe([3, 1, 3, 2, 1]), [3, 1, 2])
        self.assertEqual(dedupe(["b", "a", "b"]), ["b", "a"])


if __name__ == "__main__":
    unittest.main()
