# Database conventions (routed contract for the demo service)

All database reads go through `src/store.py::with_retry`. Direct
cursor or connection reads elsewhere in `src/` bypass the
backoff-and-log policy and are a contract violation, even when the
SQL itself is correct.
