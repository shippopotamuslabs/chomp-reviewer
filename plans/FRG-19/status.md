# FRG-19 status

- Implementation complete; PR opened from `tensor/frg-19-correctness-session`.
- Decisions live in plan.md (executor-path opening reads, binary-target
  driver, embedded prompt, LensOutcome mapping).
- Verification: 141 cargo tests green locally (112 lib, 21 bin, 5 CLI,
  3 transport-loop), clippy -D warnings clean, fmt clean, Python suite
  green. Live OpenRouter run remains unverifiable in this environment
  (no API key) — the session is proven against a scripted transport
  and a stub HTTP endpoint instead.
- Acceptance coverage: class-1 fixture pulls + cites the caller;
  opening context carries full contents past the first read window
  with the entry secret redacted; clean diff -> []; prose output ->
  producer failure; tiny turn budget -> budget outcome.
