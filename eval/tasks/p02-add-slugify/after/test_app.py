import unittest

from app import slugify


class SlugifyTest(unittest.TestCase):
    def test_slugify(self):
        self.assertEqual(slugify("Hello, World!"), "hello-world")
        self.assertEqual(slugify("  a  b  "), "a-b")
        self.assertEqual(slugify("--x--"), "x")


if __name__ == "__main__":
    unittest.main()
