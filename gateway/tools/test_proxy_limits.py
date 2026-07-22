#!/usr/bin/env python3
"""
Unit tests for proxy_limits.py (P1-11).

Focus on TokenBucket rate limiting behavior used by codex-proxy and claude-proxy.
Run:
    python3 -m unittest tools.test_proxy_limits
    # or from tools dir:
    python3 -m unittest test_proxy_limits
"""

import os
import sys
import time
import unittest
from unittest.mock import patch

# Allow running as `python3 -m unittest tools.test_proxy_limits`
sys.path.insert(0, os.path.dirname(__file__))

from proxy_limits import TokenBucket, create_ip_buckets, create_executor


class TestTokenBucket(unittest.TestCase):
    def test_initial_full_capacity_allows_burst(self):
        b = TokenBucket(capacity=5, rate=1.0)
        results = [b.allow() for _ in range(5)]
        self.assertEqual(results, [True] * 5)
        self.assertFalse(b.allow())

    def test_deny_when_exhausted(self):
        b = TokenBucket(capacity=2, rate=0.0)  # no refill
        self.assertTrue(b.allow())
        self.assertTrue(b.allow())
        self.assertFalse(b.allow())
        self.assertFalse(b.allow(1))

    def test_refill_over_time(self):
        # High rate for fast deterministic test with small sleep
        b = TokenBucket(capacity=2, rate=10.0)  # 10 tokens/sec
        self.assertTrue(b.allow())
        self.assertTrue(b.allow())
        self.assertFalse(b.allow())

        time.sleep(0.25)  # ~2.5 tokens should have accrued
        self.assertTrue(b.allow())
        self.assertTrue(b.allow())
        self.assertFalse(b.allow())

    def test_allow_multiple_tokens(self):
        b = TokenBucket(capacity=10, rate=0.0)
        self.assertTrue(b.allow(3))
        self.assertTrue(b.allow(4))
        self.assertFalse(b.allow(5))  # only 3 left

    def test_independent_per_ip_buckets(self):
        buckets = create_ip_buckets(capacity=1, rate=0.0)
        self.assertTrue(buckets["10.0.0.1"].allow())
        self.assertFalse(buckets["10.0.0.1"].allow())

        # Different IP unaffected
        self.assertTrue(buckets["10.0.0.2"].allow())

    def test_create_ip_buckets_factory_defaults(self):
        buckets = create_ip_buckets()
        b = buckets["any"]
        self.assertEqual(b.capacity, 5)
        self.assertAlmostEqual(b.rate, 10.0 / 60)

    def test_create_executor_factory(self):
        ex = create_executor(max_workers=3)
        self.assertIsNotNone(ex)
        # Basic smoke: submit and collect
        fut = ex.submit(lambda: 42)
        self.assertEqual(fut.result(timeout=1), 42)
        ex.shutdown(wait=True, cancel_futures=True)

    @patch("time.time")
    def test_deterministic_with_time_mock(self, mock_time):
        """White-box style test using time patch for zero-sleep determinism."""
        mock_time.return_value = 1000.0
        b = TokenBucket(capacity=3, rate=1.0)

        # All at same instant: burst exactly capacity, no accrual yet
        self.assertTrue(b.allow())
        self.assertTrue(b.allow())
        self.assertTrue(b.allow())
        self.assertFalse(b.allow())

        # Advance time to accrue
        mock_time.return_value = 1001.5
        self.assertTrue(b.allow())
        self.assertFalse(b.allow())

    def test_tokens_never_exceed_capacity(self):
        b = TokenBucket(capacity=1, rate=100.0)
        b.allow()  # exhaust
        time.sleep(0.05)
        self.assertTrue(b.allow())
        self.assertFalse(b.allow())  # still capped at 1


if __name__ == "__main__":
    unittest.main()
