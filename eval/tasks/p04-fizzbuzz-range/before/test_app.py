import unittest

from app import fizzbuzz


class FizzBuzzTest(unittest.TestCase):
    def test_includes_n(self):
        self.assertEqual(fizzbuzz(5), ["1", "2", "Fizz", "4", "Buzz"])
        self.assertEqual(fizzbuzz(15)[-1], "FizzBuzz")


if __name__ == "__main__":
    unittest.main()
