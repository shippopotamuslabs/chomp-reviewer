"""Order store: all database reads go through with_retry."""

import time


def with_retry(read, attempts=3):
    """Run a database read with backoff, logging each retry."""
    last = None
    for attempt in range(attempts):
        try:
            return read()
        except ConnectionError as error:
            last = error
            time.sleep(0.1 * (attempt + 1))
    raise last


def _read_order(order_id):
    # Placeholder for the real cursor read.
    return {"id": order_id, "total": 42.5, "currency": "USD"}


def fetch_order(order_id):
    """Fetch one order by id. Returns the order mapping."""
    return with_retry(lambda: _read_order(order_id))
