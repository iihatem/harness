import unittest

from app import merge


class MergeTest(unittest.TestCase):
    def test_overrides_win(self):
        self.assertEqual(merge({"a": 1, "b": 2}, {"b": 3}), {"a": 1, "b": 3})


if __name__ == "__main__":
    unittest.main()
