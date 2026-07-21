import unittest

from weather import status


class WeatherStatusTest(unittest.TestCase):
    def test_warm_temperature(self):
        self.assertEqual(status(24), "warm")

    def test_cold_temperature(self):
        self.assertEqual(status(12), "cold")


if __name__ == "__main__":
    unittest.main()
