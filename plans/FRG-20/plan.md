# FRG-20 — Contract resolution: base-branch entry file, size guard, absence degradation

Ticket: FRG-20 (FG-6). Proposal §8.7; decisions D1 (no fallback chain),
D2 ([forge] table in .pr_agent.toml), D3 (ceiling 2,000 lines) ratified.

## State found

FRG-15 already landed the config half: ForgeConfig { entry_file,
pr_agent_context_files } with parse/load precedence and tests. FRG-19
wires sessions to read the entry file from the HEAD checkout through
the FRG-18 executor. This ticket adds entry_max_lines, replaces the
head/executor read with base-branch resolution behind a separate
contract channel, and records guard/absence events in the decision
log.

## Design

### Resolution (contract.rs, binary module)

    resolve_contract(repo, base_ref, config) -> Result<ContractResolution, ContractError>

    enum ContractResolution {
        Governing { entry_file, content, truncated_from_lines: Option<usize> },
        Absent { entry_file, reason: AbsentReason::{Missing, NotText} },
    }

Mechanics, all read-only git against the caller's checkout:
- `git rev-parse --verify {base_ref}^{commit}` validates the ref. A
  bad ref is a caller error -> ContractError -> exit 2. (Absence is a
  property of the FILE, never of the ref.)
- `git cat-file -e {base_ref}:{entry_file}` distinguishes missing
  (Absent/Missing) from present; `git show` fetches the bytes;
  non-UTF-8 -> Absent/NotText (degrade, do not fail, do not lossy-
  decode someone's constitution).
- The entry path is only ever an argument to `git show ref:path`;
  it is never opened on the filesystem, so a hostile or odd
  entry_file cannot escape the tree — git itself rejects it, which
  lands on Absent.
- Size guard: content past entry_max_lines lines is truncated to the
  ceiling; truncated_from_lines records the original total.

Derived surfaces on the resolution:
- decision_entry(): Governing+truncated and Absent each produce one
  host event for the decision log; a clean Governing produces none.
- pr_agent_context_files(config): the configured list when
  Governing, [] when Absent (ticket rule 4). Consumer is FRG-23;
  the resolution rule and its tests land here.

### Decision log (outputs.rs)

Entries stay serde_json::Value (FRG-25/26 will add their own kinds),
gaining a constructor for host events:
{ "kind": "contract", "event": "entry_truncated" | "entry_absent",
  "detail": "<human-readable specifics>" }.
write_outputs takes the entries slice; version stays 1.

### Sessions (sessions.rs)

run_correctness_session takes &ContractResolution instead of
&entry_file. The opening contract section renders the GOVERNING
(base) content under a header naming it as the base-branch copy,
plus the truncation note when truncated. Absent renders no section.
The executor read of the entry file is removed — the channel
separation is the point: FRG-18 tools are rooted at the head
snapshot and cannot reach the base tree (acceptance test asserts
the executor returns HEAD content for the same path the channel
serves from BASE).

FRG-19's entry-redaction test is superseded by design: redaction
guards untrusted HEAD content; the base branch is the adopter's own
governing document, loaded verbatim by the host (PR-Agent's
repo_context_from_default_branch precedent). Head copies of the
entry file still flow through the executor path (changed-file
section) redacted as before.

### main.rs

resolve_contract after Snapshot::capture, before session
construction. ContractError -> exit 2. Resolution decision entries
flow into write_outputs.

## Tests (acceptance mapping)

- contract.rs: base/head divergence (governing == base bytes);
  truncation at a configured small ceiling + default == 2000;
  missing -> Absent/Missing; binary content -> Absent/NotText;
  bad ref -> ContractError; pr_agent_context_files rule both ways.
- sessions.rs: Governing renders base content + truncation note;
  Absent renders no contract section; executor-vs-channel test
  (same path, head bytes from tools, base bytes in the opening);
  class-1 proof unchanged.
- tests/cli.rs: no AGENTS.md anywhere -> exit 0 and decisions carry
  the entry_absent event; configured tiny ceiling + long AGENTS.md
  -> decisions carry entry_truncated.
