import unittest
from decimal import Decimal

from binance_oi_collector import CollectorError, build_payload


class BuildPayloadTests(unittest.TestCase):
    def test_preserves_raw_decimal_strings_and_upstream_timestamp(self):
        payload = build_payload(
            {"openInterest": "87654.32100000", "time": 1_700_000_012_345},
            {"markPrice": "50000.12500000", "time": 1_700_000_012_400},
        )

        self.assertEqual(payload["raw_open_interest"], "87654.32100000")
        self.assertEqual(payload["mark_price"], "50000.12500000")
        self.assertEqual(payload["observed_at_ms"], 1_700_000_012_345)
        self.assertNotIn("usd_open_interest", payload)

    def test_rejects_non_finite_or_non_positive_values(self):
        cases = [
            ({"openInterest": "NaN", "time": 1}, {"markPrice": "1"}),
            ({"openInterest": "1", "time": 1}, {"markPrice": "0"}),
            ({"openInterest": "-1", "time": 1}, {"markPrice": "1"}),
        ]
        for open_interest, mark in cases:
            with self.subTest(open_interest=open_interest, mark=mark):
                with self.assertRaises(CollectorError):
                    build_payload(open_interest, mark)

    def test_decimal_math_reference_is_exact(self):
        payload = build_payload(
            {"openInterest": "100.25", "time": 1},
            {"markPrice": "50.5"},
        )
        expected = Decimal(payload["raw_open_interest"]) * Decimal(payload["mark_price"])
        self.assertEqual(expected, Decimal("5062.625"))


if __name__ == "__main__":
    unittest.main()
