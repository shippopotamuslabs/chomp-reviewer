# FRG-16 — status

- State: implemented, all gates green (2026-10-28). Not committed — left
  in the working tree for review/PR by the parent flow.
- Branch: shippopotamus/frg-16-openrouter-transport (forge-reviewer)

## What was built

- `agent/src/transport.rs` (replaces the FRG-15 skeleton):
  `OpenRouterTransport { api_key, model, base_url, agent }` with
  `new(...)` and `from_env()` (OPENROUTER_API_KEY required →
  `TransportError::MissingApiKey`; FORGE_MODEL with `openrouter/`
  prefix stripped, default `meta/muse-spark-1.3-contributor`;
  OPENROUTER_BASE_URL override, default `https://openrouter.ai/api/v1`).
  Pure `render_request_body` and `map_response`; `complete_model_turn`
  does the one POST; `TransportError::to_failure_report()` maps to the
  engine's `ModelTurnFailureReport` (Transient: 429/5xx/Network/
  MalformedResponse; Permanent: other 4xx/MissingApiKey/Render;
  Retry-After delta-seconds honored). No panics anywhere in the module.
- `agent/src/lib.rs` (new): lib target exposing `pub mod transport;` so
  integration tests can drive the transport. `main.rs` untouched — it
  declares its own module tree as before.
- `agent/tests/transport_loop_test.rs` (new): real engine + session
  driven through the real transport against a std-only TcpListener stub
  server that records request bodies. Three scenarios, all passing:
  native tool-call path (asserts the rendered schema on request 1 and
  the replayed call/result pair on request 2), envelope path
  (BracketArray text call extracted by agent_core ingest →
  ExecuteTools → Finished), and the 500 path (first failure schedules a
  retry CallModel; four scripted 500s = initial attempt + 3 retries →
  terminal Failed with `model_api_retries_exhausted`).

## Deviations from plan.md (with reasons)

1. **Tool messages without a `call_id` render as user turns, not a
   `Render` error.** The plan assumed such messages are corrupt
   history. Verified false against agent_core 05eafa8: every genuine
   tool result carries a `call_id` (engine invariant in
   model_intake/tool_results), and the only Tool-role messages without
   one are engine-authored model-facing feedback — the envelope
   parse-failure error and the tools-blocked nudge, payloads named
   `*_for_model`. Erroring would permanently brick a session on the
   first malformed envelope, the exact event the fallback exists to
   recover from; rendering as `tool` would be an orphan the provider
   rejects. User-role rendering preserves the content and the recovery
   flow. Unit test renamed to match
   (`tool_message_without_call_id_renders_as_user_feedback`).
2. **Request body sent via `send(bytes)`, not `send_json`.** ureq
   3.5.0's `send_json` sits behind its `json` cargo feature, which is
   not enabled and `agent/Cargo.toml` was out of scope. The body is
   serialized with serde_json (already a dependency) and the
   `Content-Type: application/json` header is set explicitly, as the
   plan's header list requires.
3. **ureq configuration** uses `Agent::new_with_config(
   Agent::config_builder().http_status_as_error(false)
   .timeout_global(300 s).build())` — verified against the vendored
   ureq 3.5.0 sources, not from memory.
4. **Envelope e2e session declares the tool in the semantics catalog**,
   not only in the schema channel: envelope recovery validates against
   `config.tool_semantics.catalog` (an empty catalog yields
   `Unsupported tool: web_search`, verified by probe). The test builds
   a minimal catalog with `ToolSemanticsConfig::agnostic()` plus one
   `ToolSpec`, mirroring agent_core's own enclave-fixture pattern.
   Plan's session-shape expectation otherwise followed exactly.
5. **`#[allow(dead_code)]` on the transport's public items** until the
   FRG-19 host driver wires `complete_model_turn` into the binary; the
   bin target otherwise fails clippy `-D warnings` on never-used. The
   allows are commented as temporary and come off in FRG-19.

## Gates (2026-10-28)

- `cargo fmt --check`: clean (after `cargo fmt`).
- `cargo clippy --all-targets -- -D warnings`: clean.
- `cargo test --locked` in agent/: 52 passed, 0 failed — lib unit 16
  (all in transport.rs), bin unit 28 (12 pre-existing + the 16 transport
  unit tests compiled into the bin's own module tree), cli integration
  5, transport loop integration 3, doc-tests 0.
- `cargo tree | grep -i uniffi`: empty.
- Repo Python suite (`python3 -m unittest discover -s tests`): 159
  tests, OK (4 skipped) — unchanged.

## Pending

- Live smoke against real OpenRouter: no OPENROUTER_API_KEY in this
  environment; per the ticket this stays pending CI-secret/manual
  verification.

## Review round 1 fixes (2026-10-28)

Applied from a fresh-context review; still uncommitted in the worktree.

1. **Synthesized ids in `map_response`** — a native tool call whose id
   is missing or empty now gets `Some("call_<index>")` (its position in
   the calls array), matching the render-side synthesis, so repair,
   replay, pairing, and rendering share one id instead of the call
   carrying `None` through ingest. The stale assertion in
   `maps_object_arguments_by_stringifying` was updated to expect
   `call_0`; new test `idless_tool_calls_get_deterministic_synthesized_ids`
   covers two id-less calls (missing and empty) → `call_0`, `call_1`,
   with a present id passing through untouched.
2. **Envelope loop replay assertions** —
   `envelope_tool_call_path` now asserts on the second recorded
   request, mirroring the native scenario: the replayed assistant/tool
   pair carries the envelope-extracted call under id `text-tool-1`
   (name `web_search`, arguments containing `chomp`) and the tool
   result with matching `tool_call_id` and result content.
3. **Header pinning** — the stub server exposes recorded request
   headers (`recorded_headers()`), and the native scenario asserts
   `Authorization: Bearer test-key`, `Content-Type: application/json`,
   `HTTP-Referer: https://github.com/shippopotamuslabs/forge-reviewer`,
   and `X-Title: Chomp` on every recorded request.
4. **`from_env` model resolution** — factored into the pure
   `resolve_model(Option<String>)`: the `openrouter/` prefix is
   stripped *before* the emptiness check, so `FORGE_MODEL="openrouter/"`
   falls back to the default instead of resolving to an empty model.
   `model_resolution_table` covers unset→default, plain→itself,
   prefixed→stripped, prefix-only→default, blank→default.
5. **Error-body truncation** — `TransportError::Http` bodies are
   capped at 2,000 chars (`MAX_ERROR_BODY_CHARS`, char-boundary safe
   via `truncate_error_body`) at construction, so a full provider
   error page cannot flood the failure-report message. Covered by
   `error_body_truncation_caps_at_the_limit`.
6. **Body-read failure no longer masks HTTP status** —
   `complete_model_turn` captures the status before reading the body;
   a read error on a non-2xx response returns `Http { status, body:
   "" }` (partial-or-empty), and only a 2xx read failure is a
   `Network` error. The status drives retry classification, so it
   must survive a broken body stream.
7. **Array-shaped `message.content`** — `map_response` concatenates
   `{"type":"text","text":...}` parts (non-text parts skipped). If
   the mapped response then has no text, no tool calls, and no
   reasoning, it returns a typed `MalformedResponse` instead of a
   silent empty `ModelResponse`. Covered by
   `maps_array_shaped_content_by_concatenating_text_parts` and
   `empty_mapped_response_is_a_typed_error`.
8. **Lib/bin collapse (supersedes deviation 5 above)** — `mod
   transport;` removed from `src/main.rs` (verified: nothing in the
   bin referenced `crate::transport`; the declaration alone was
   compiling the module into the bin). The transport now lives only
   in the lib target (`pub mod transport;` in `src/lib.rs`), so its
   unit tests run once, and all temporary `#[allow(dead_code)]`
   attributes added for the duplicated bin compilation are removed
   (the FRG-19 driver note is gone with them; the lib surface comment
   records that FRG-19 consumes the transport from there).

### Gates (review round 1, 2026-10-28)

- `cargo fmt` then `cargo fmt --check`: clean.
- `cargo clippy --all-targets -- -D warnings`: clean.
- `cargo test --locked` in agent/: 41 passed, 0 failed — lib unit 21
  (16 prior transport unit tests + 5 new: synthesized ids, array
  content, empty-response error, model resolution, body truncation),
  bin unit 12 (pre-existing only — the 16 duplicated transport tests
  from the bin target are gone, which is fix 8's point), cli
  integration 5, transport loop integration 3, doc-tests 0.
- `cargo tree | grep -i uniffi`: empty.
- Repo Python suite (`python3 -m unittest discover -s tests`): 159
  tests, OK (4 skipped) — unchanged.

## Port follow-up (2026-10-28, Sean's direction)

Sean's review of the shipped FRG-16: the OpenRouter code is infrastructure,
so it must sit behind an application port — and the "end-to-end" test should
orchestrate through that port, not call the concrete struct. Correct on both
counts (the loop test's driver called OpenRouterTransport directly).

- New `agent/src/model_transport.rs`: the application port
  (`trait ModelTransport`) plus `TransportError` + `to_failure_report()`
  moved there — the error vocabulary is part of the port contract.
- `OpenRouterTransport` now implements the port; its inherent
  `complete_model_turn` is a private helper behind the trait impl, so the
  trait method is the only public door.
- The loop test's driver takes `&dyn ModelTransport`; the remaining direct
  call sites resolve through the trait (no inherent public method exists).
- Gates after the change: cargo 41/0 (unchanged counts), fmt + clippy
  clean, Python 159 OK.
