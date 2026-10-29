# FRG-20 status

- 2026-10-29: picked up at Sean's word ("move on to the next ticket").
  Plan written from the ticket + code state (config half already
  landed in FRG-15; resolution/wiring is the work).
- 2026-10-29: implemented and PR'd. Config gained entry_max_lines
  (default 2,000, D3). resolve_contract reads the entry from the
  base ref via read-only git (rev-parse validates the ref — a bad
  ref is a ContractError/exit 2; cat-file/show fetch the file;
  missing or non-UTF-8 degrades to Absent). Sessions take the
  ContractResolution; the contract section renders the governing
  base copy with the truncation note; the executor entry read is
  gone. Decision log takes host events: entry_absent /
  entry_truncated recorded from main.
- Deviations worth naming: (1) FRG-19's entry-redaction test was
  replaced, not extended — redaction guards executor-served HEAD
  content; the base governing copy is verbatim by ticket design.
  The new channel test scopes its redaction assertions to the
  changed-files section, because the diff section is the caller's
  own artifact and carries PR text verbatim (pre-existing FRG-19
  boundary, now pinned by an explicit comment in the test).
  (2) pr_agent_context_files resolution rule is implemented and
  tested here; its consumer is FRG-23, so the method carries an
  allow(dead_code) naming that.
  (3) Test fixtures now pin `git init -b main`: FRG-20 is the first
  ticket to actually resolve --base-ref against git, and the
  default branch name is not portable (cli.rs + live_smoke.rs).
- Gates: 153 cargo tests green (112 lib, 29 bin, 6 CLI, 3
  transport, live smoke ignored), clippy -D warnings clean, fmt
  clean, Python suite OK.
