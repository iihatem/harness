import unittest

from main import greet
from util import shout


class GreetTest(unittest.TestCase):
    def test_shout(self):
        self.assertEqual(shout("hi"), "HI!")

    def test_greet(self):
        self.assertEqual(greet(" ann "), "HELLO ANN!")


if __name__ == "__main__":
    unittest.main()
