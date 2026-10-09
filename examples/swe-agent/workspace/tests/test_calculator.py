import unittest

from calculator import mean


class MeanTests(unittest.TestCase):
    def test_integer_average(self):
        self.assertEqual(mean([1, 2, 3]), 2)

    def test_fractional_average(self):
        self.assertEqual(mean([1, 2]), 1.5)

    def test_negative_fractional_average(self):
        self.assertEqual(mean([-1, -2]), -1.5)

    def test_single_value(self):
        self.assertEqual(mean([7]), 7)

    def test_empty_sequence(self):
        with self.assertRaises(ValueError):
            mean([])

    def test_does_not_modify_input(self):
        values = [1, 2, 3]
        mean(values)
        self.assertEqual(values, [1, 2, 3])


if __name__ == "__main__":
    unittest.main()
