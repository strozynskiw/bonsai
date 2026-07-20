import sys
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).parents[1] / "src"))

from pricing import apply_discount  # noqa: E402


class PricingTests(unittest.TestCase):
    def test_applies_percentage_discount(self) -> None:
        self.assertEqual(apply_discount(2_000, 25), 1_500)

    def test_zero_discount_keeps_subtotal(self) -> None:
        self.assertEqual(apply_discount(750, 0), 750)


if __name__ == "__main__":
    unittest.main()
