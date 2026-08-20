import unittest

from weather import fahrenheit, status


class WeatherStatusTest(unittest.TestCase):
    def test_warm_temperature(self):
        self.assertEqual(status(24), "warm")

    def test_cold_temperature(self):
        self.assertEqual(status(12), "cold")

    def test_fahrenheit_conversion(self):
        self.assertEqual(fahrenheit(0), 32)


if __name__ == "__main__":
    unittest.main()
