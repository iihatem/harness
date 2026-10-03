import unittest

from app import average


class AverageTest(unittest.TestCase):
    def test_average(self):
        self.assertEqual(average([1, 2, 3]), 2)

    def test_empty(self):
        self.assertEqual(average([]), 0.0)


if __name__ == "__main__":
    unittest.main()
