# FRG-16 — OpenRouter transport for the chomp agent (plan)

## Reconciliation (read first)

The ticket text predates the agent_core refactor program (ENC-285..309):
`AgentTransport` / `src/host.rs` no longer exist. The pull-based equivalents,
verified on agent_core main (05eafa8):

- The host services `AgentAction::CallModel(ModelRequest)` and answers with
  `Engine::submit_model_response(handle, ModelResponse)`.
- Failures go to `Engine::submit_model_turn_failure(handle,
  ModelTurnFailureReport)` (`src/model_api_retry.rs`; the report carries
  `error_class: Transient|Permanent`, `http_status: Option<u16>`,
  `provider_code`, `message`, `retry_after_ms`).
- `ModelRequest { session_messages: Vec<AgentMessage>, system_prompt,
  tool_schemas: Vec<SchemaEntry>, rendered_runtime_tools: Vec<SchemaEntry>,
  allow_tool_call, .. }`; `ModelResponse { assistant_text: Option<String>,
  tool_calls: Vec<RawToolCall>, finish_reason, provider_reasoning_content }`.
- Message tool metadata lives on `MessageMeta` accessors: `call_id()`,
  `tool_name()`, `raw_tool_calls()`.

So the transport is a host component with one job:
`complete_model_turn(&ModelRequest) -> Result<ModelResponse, TransportError>`,
plus a `TransportError::to_failure_report()` mapping. No traits from
agent_core are implemented (none exist); no provider code enters agent_core.

## Design — agent/src/transport.rs (replaces the FRG-15 skeleton)

- `OpenRouterTransport { api_key, model, base_url, agent: ureq::Agent }`.
  - `from_env()`: `OPENROUTER_API_KEY` required (missing = typed
    `TransportError::MissingApiKey`, never a panic). Model: `FORGE_MODEL`
    env with the `openrouter/` prefix stripped, default
    `meta/muse-spark-1.3-contributor` (the 1.x convention,
    `.forge/openrouter_client.py`). Base URL: `OPENROUTER_BASE_URL` env,
    default `https://openrouter.ai/api/v1` (also the test seam).
  - `new(api_key, model, base_url)` for tests.
  - ureq agent configured `http_status_as_error(false)` so the transport
    branches on status itself (full body + Retry-After access); 300 s
    timeout matching the 1.x client.
- Rendering (pure fn `render_request_body(&ModelRequest, model) ->
  Result<Value, TransportError>`, unit-tested):
  - `system_prompt` (if non-empty) first as a system message; then
    `session_messages`: System→system, User→user, Assistant→assistant with
    `tool_calls` rebuilt from `meta.raw_tool_calls()` as OpenAI
    `{id, type:"function", function:{name, arguments}}` (missing id →
    deterministic `call_<index>`), Tool→`{role:"tool", tool_call_id:
    meta.call_id()}` — a tool message without a call_id is corrupt history:
    typed `TransportError::Render`, never a silent drop.
  - Tools: only when `allow_tool_call`; source = `rendered_runtime_tools`
    when non-empty else `tool_schemas` (the rendered list is the active
    channel through the runtime gates, ENC-297); each `SchemaEntry.value`
    wrapped as `{"type":"function","function": <value>}`; `tool_choice:
    "auto"` when tools are present.
  - Sampling constants from the 1.x client (ModelRequest carries none):
    `max_tokens: 16000`, `temperature: 0.1`, named consts.
- Response mapping (pure fn `map_response(&Value) ->
  Result<ModelResponse, TransportError>`, unit-tested):
  - `choices[0].message`: `content` (empty → None) → `assistant_text`;
    native `tool_calls[]` → `RawToolCall { id, name: function.name,
    arguments_json: function.arguments (stringified if not a string) }`;
    `reasoning` → `provider_reasoning_content`.
  - `finish_reason`: tool calls present or `"tool_calls"` → ToolUse,
    `"stop"` → Stop, `"length"` → MaxTokens, anything else →
    Other(value). Missing choices / an `error` object in the body →
    typed errors, never panics or silent empties.
- HTTP: POST `{base_url}/chat/completions`, headers `Authorization:
  Bearer <key>`, `Content-Type: application/json`, `HTTP-Referer:
  https://github.com/shippopotamuslabs/forge-reviewer`, `X-Title: Chomp`.
  Non-2xx → `TransportError::Http { status, body }` (body kept for the
  failure report message); transport-level failures →
  `TransportError::Network`.
- `TransportError::to_failure_report()`: `http_status` set for Http;
  class Transient for 429 / 5xx / Network / MalformedResponse, Permanent
  for other 4xx; `retry_after_ms` from the Retry-After header when
  present; `had_partial_output: false`, `partial_output_chars: 0`.

## Tests

- Unit (in transport.rs): rendering (system-first, assistant tool-call
  replay incl. missing-id synthesis, tool message call_id, missing-call_id
  Render error, tools wrapping + allow_tool_call=false omission,
  rendered-vs-catalog preference) and mapping (native tool calls, text
  only, reasoning capture, finish-reason table, error body, empty
  choices), failure-report classification table.
- End-to-end (`agent/tests/transport_loop_test.rs`): a real engine +
  session driven through the transport against a std-only stub HTTP
  server (TcpListener thread, scripted responses, records request
  bodies), modeled on agent_core's `examples/mock_desktop_loop.rs`:
  1. Native path: stub returns a native tool call → engine emits
     ExecuteTools → test services it → stub returns a text answer →
     session Finished. Assert the recorded request bodies carry the
     rendered tool schema and the replayed tool call/result pair.
  2. Envelope path: stub returns text containing an envelope tool call →
     agent_core's ingest extracts it → ExecuteTools arrives (proves the
     non-native fallback end to end).
  3. Failure path: stub returns 500 → transport error → host submits
     `to_failure_report()` via `submit_model_turn_failure` → engine
     retries or fails terminally per its policy (assert: no panic, the
     loop reaches CallModel again or Failed).
- Live smoke against real OpenRouter: no OPENROUTER_API_KEY exists in
  this environment; per the ticket, record as pending CI-secret/manual
  verification on the ticket.

## Non-goals

- The session driver / lens wiring (FRG-19+): host.rs stays a skeleton;
  the loop lives only in tests for now.
- Streaming, retries inside the transport (engine policy owns retries),
  reasoning-model special cases beyond capturing `reasoning`.
- No CHANGELOG / tag (FRG-32 owns release handling).
