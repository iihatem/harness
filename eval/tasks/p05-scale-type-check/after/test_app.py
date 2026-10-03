import unittest

from app import scale


class ScaleTest(unittest.TestCase):
    def test_numbers(self):
        self.assertEqual(scale(3, 2), 6)
        self.assertEqual(scale(1.5, 2), 3.0)

    def test_rejects_other_types(self):
        with self.assertRaises(TypeError):
            scale("ab", 3)


if __name__ == "__main__":
    unittest.main()
