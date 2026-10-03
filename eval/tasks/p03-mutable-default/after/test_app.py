import unittest

from app import add_item


class AddItemTest(unittest.TestCase):
    def test_calls_do_not_share(self):
        self.assertEqual(add_item(1), [1])
        self.assertEqual(add_item(2), [2])

    def test_given_list_is_used(self):
        xs = [0]
        self.assertEqual(add_item(1, xs), [0, 1])


if __name__ == "__main__":
    unittest.main()
