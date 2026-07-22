"""Shared rate limiting and concurrency helpers for CLI proxies (P1-11).

TokenBucket: simple per-IP token bucket.
create_ip_buckets / create_executor for the proxies.
"""

from collections import defaultdict
import threading
import time
from concurrent.futures import ThreadPoolExecutor


class TokenBucket:
    """Per-IP token bucket rate limiter."""

    def __init__(self, capacity: int, rate: float):
        self.capacity = capacity
        self.rate = rate  # tokens per second
        self.tokens = float(capacity)
        self.last = time.time()
        self.lock = threading.Lock()

    def allow(self, tokens: int = 1) -> bool:
        with self.lock:
            now = time.time()
            elapsed = now - self.last
            self.tokens = min(self.capacity, self.tokens + elapsed * self.rate)
            self.last = now
            if self.tokens >= tokens:
                self.tokens -= tokens
                return True
            return False


def create_ip_buckets(capacity: int = 5, rate: float = 10.0 / 60):
    """Factory for per-IP buckets (e.g. 5 burst, 10/min)."""
    return defaultdict(lambda: TokenBucket(capacity, rate))


def create_executor(max_workers: int = 3):
    """Bounded executor to cap concurrent CLI processes."""
    return ThreadPoolExecutor(max_workers=max_workers)
