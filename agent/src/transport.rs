//! OpenRouter transport for `agent_core` model calls (FRG-16).
//!
//! `agent_core` is pull-based: the host services
//! `AgentAction::CallModel(ModelRequest)` and answers with either
//! `Engine::submit_model_response` or `Engine::submit_model_turn_failure`.
//! This module is the host component between those two calls:
//! [`OpenRouterTransport::complete_model_turn`] renders a `ModelRequest`
//! into an OpenRouter (OpenAI-shaped) chat-completions body, performs the
//! HTTP round-trip, and maps the response back into a `ModelResponse`.
//! Failures are typed [`TransportError`]s — never panics, never silent
//! drops — and [`TransportError::to_failure_report`] converts them into
//! the engine's `ModelTurnFailureReport`, whose retry policy the engine
//! owns (the transport itself never retries).
//!
//! Rendering and response mapping are pure functions
//! ([`render_request_body`], [`map_response`]) so they are unit-tested
//! without any HTTP.

use std::env;
use std::fmt;
use std::time::Duration;

use agent_core::message::{AgentMessage, AgentRole};
use agent_core::model_api_retry::{ModelApiErrorClass, ModelTurnFailureReport};
use agent_core::tool_schema::SchemaEntry;
use agent_core::turn_io::{FinishReason, ModelRequest, ModelResponse, RawToolCall};
use serde_json::{json, Map, Value};

/// Model used when `FORGE_MODEL` is unset (the 1.x convention from
/// `.forge/openrouter_client.py`, with the `openrouter/` prefix stripped).
pub const DEFAULT_MODEL: &str = "meta/muse-spark-1.3-contributor";

/// OpenRouter API root; `OPENROUTER_BASE_URL` overrides it (the test seam
/// for the stub-server loop tests).
pub const DEFAULT_BASE_URL: &str = "https://openrouter.ai/api/v1";

/// Sampling constants from the 1.x client: `ModelRequest` carries no
/// sampling parameters, so the transport owns them.
const MAX_TOKENS: u64 = 16_000;
const TEMPERATURE: f64 = 0.1;

/// HTTP timeout, matching the 1.x client's 300 s.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(300);

/// OpenRouter attribution headers.
const HTTP_REFERER: &str = "https://github.com/shippopotamuslabs/forge-reviewer";
const X_TITLE: &str = "Chomp";

/// Typed transport failures. Every malformed-input and provider-failure
/// path lands in one of these variants.
#[derive(Debug)]
pub enum TransportError {
    /// `OPENROUTER_API_KEY` was not set (or was blank) in `from_env`.
    MissingApiKey,
    /// The rendered request body could not be serialized for the wire.
    Render(String),
    /// The provider answered with a non-2xx status; the body is kept for
    /// the failure-report message.
    Http {
        status: u16,
        body: String,
        retry_after_ms: Option<u64>,
    },
    /// The HTTP round-trip itself failed (DNS, connect, read, ...).
    Network(String),
    /// A 2xx response whose body could not be mapped onto a
    /// `ModelResponse` (not JSON, no choices, an `error` object, ...).
    MalformedResponse(String),
}

impl fmt::Display for TransportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TransportError::MissingApiKey => {
                write!(f, "OPENROUTER_API_KEY is not set")
            }
            TransportError::Render(detail) => {
                write!(f, "cannot render model request: {detail}")
            }
            TransportError::Http { status, body, .. } => {
                write!(f, "OpenRouter returned HTTP {status}: {body}")
            }
            TransportError::Network(detail) => {
                write!(f, "OpenRouter request failed: {detail}")
            }
            TransportError::MalformedResponse(detail) => {
                write!(f, "malformed OpenRouter response: {detail}")
            }
        }
    }
}

impl std::error::Error for TransportError {}

impl TransportError {
    /// Map onto the engine's failure report. Transient (the engine may
    /// retry the turn): 429, 5xx, network failures, and malformed
    /// responses. Permanent: other 4xx statuses, a missing API key, and
    /// unrenderable requests — retrying those cannot help.
    pub fn to_failure_report(&self) -> ModelTurnFailureReport {
        let (error_class, http_status, retry_after_ms) = match self {
            TransportError::Http {
                status,
                retry_after_ms,
                ..
            } => {
                let class = if *status == 429 || *status >= 500 {
                    ModelApiErrorClass::Transient
                } else {
                    ModelApiErrorClass::Permanent
                };
                (class, Some(*status), *retry_after_ms)
            }
            TransportError::Network(_) | TransportError::MalformedResponse(_) => {
                (ModelApiErrorClass::Transient, None, None)
            }
            TransportError::MissingApiKey | TransportError::Render(_) => {
                (ModelApiErrorClass::Permanent, None, None)
            }
        };
        ModelTurnFailureReport {
            error_class,
            http_status,
            provider_code: http_status.map(|status| status.to_string()),
            message: self.to_string(),
            had_partial_output: false,
            partial_output_chars: 0,
            retry_after_ms,
        }
    }
}

/// The OpenRouter transport: one configured client, one job.
pub struct OpenRouterTransport {
    api_key: String,
    model: String,
    base_url: String,
    agent: ureq::Agent,
}

impl OpenRouterTransport {
    /// Build a transport from explicit parts (the constructor tests use).
    pub fn new(
        api_key: impl Into<String>,
        model: impl Into<String>,
        base_url: impl Into<String>,
    ) -> Self {
        // http_status_as_error(false): the transport branches on the
        // status itself so non-2xx bodies and Retry-After survive to the
        // failure report instead of collapsing into a ureq error.
        let agent = ureq::Agent::new_with_config(
            ureq::Agent::config_builder()
                .http_status_as_error(false)
                .timeout_global(Some(REQUEST_TIMEOUT))
                .build(),
        );
        Self {
            api_key: api_key.into(),
            model: model.into(),
            base_url: base_url.into().trim_end_matches('/').to_string(),
            agent,
        }
    }

    /// Build a transport from the environment:
    /// `OPENROUTER_API_KEY` (required), `FORGE_MODEL` (optional,
    /// `openrouter/` prefix stripped, 1.x convention), and
    /// `OPENROUTER_BASE_URL` (optional override / test seam).
    pub fn from_env() -> Result<Self, TransportError> {
        let api_key = env::var("OPENROUTER_API_KEY")
            .ok()
            .filter(|key| !key.trim().is_empty())
            .ok_or(TransportError::MissingApiKey)?;
        let model = resolve_model(env::var("FORGE_MODEL").ok());
        let base_url = env::var("OPENROUTER_BASE_URL")
            .ok()
            .filter(|url| !url.trim().is_empty())
            .unwrap_or_else(|| DEFAULT_BASE_URL.to_string());
        Ok(Self::new(api_key, model, base_url))
    }

    /// The model every request from this transport names.
    pub fn model(&self) -> &str {
        &self.model
    }

    /// Service one `CallModel` request: render, POST, map.
    pub fn complete_model_turn(
        &self,
        request: &ModelRequest,
    ) -> Result<ModelResponse, TransportError> {
        let body = render_request_body(request, &self.model)?;
        // Serialized by hand: ureq's `json` feature (send_json) is not
        // enabled in this crate, and serde_json is already a dependency.
        let body = serde_json::to_string(&body).map_err(|error| {
            TransportError::Render(format!("rendered body does not serialize: {error}"))
        })?;
        let url = format!("{}/chat/completions", self.base_url);
        let mut response = self
            .agent
            .post(&url)
            .header("Authorization", format!("Bearer {}", self.api_key))
            .header("Content-Type", "application/json")
            .header("HTTP-Referer", HTTP_REFERER)
            .header("X-Title", X_TITLE)
            .send(body.as_bytes())
            .map_err(|error| TransportError::Network(error.to_string()))?;
        let status = response.status().as_u16();
        let retry_after_ms = response
            .headers()
            .get("retry-after")
            .and_then(|value| value.to_str().ok())
            .and_then(parse_retry_after_ms);
        // The status was captured above, before the body is read: a
        // body-read failure on a non-2xx response must not mask that
        // status behind a Network error — the status drives the failure
        // report's retry classification. The body is best-effort there
        // (partial-or-empty); on a 2xx, an unreadable body is a genuine
        // transport failure.
        let text = match response.body_mut().read_to_string() {
            Ok(text) => text,
            Err(error) => {
                if !(200..300).contains(&status) {
                    return Err(TransportError::Http {
                        status,
                        body: String::new(),
                        retry_after_ms,
                    });
                }
                return Err(TransportError::Network(error.to_string()));
            }
        };
        if !(200..300).contains(&status) {
            return Err(TransportError::Http {
                status,
                body: truncate_error_body(text),
                retry_after_ms,
            });
        }
        let value: Value = serde_json::from_str(&text).map_err(|error| {
            TransportError::MalformedResponse(format!("body is not valid JSON: {error}"))
        })?;
        map_response(&value)
    }
}

/// Resolve the effective model from a raw `FORGE_MODEL` value: trim,
/// strip the `openrouter/` prefix (1.x convention), and fall back to
/// [`DEFAULT_MODEL`] when nothing usable remains — including the
/// prefix-only value `"openrouter/"`, which strips to empty.
fn resolve_model(raw: Option<String>) -> String {
    let Some(raw) = raw else {
        return DEFAULT_MODEL.to_string();
    };
    let trimmed = raw.trim();
    let stripped = trimmed.strip_prefix("openrouter/").unwrap_or(trimmed);
    if stripped.is_empty() {
        DEFAULT_MODEL.to_string()
    } else {
        stripped.to_string()
    }
}

/// Cap on the provider error body kept on
/// [`TransportError::Http`]: enough to diagnose from the failure report,
/// not enough to flood it with a full error page.
const MAX_ERROR_BODY_CHARS: usize = 2_000;

/// Truncate an error body to [`MAX_ERROR_BODY_CHARS`] characters
/// (char-boundary safe).
fn truncate_error_body(body: String) -> String {
    if body.chars().count() <= MAX_ERROR_BODY_CHARS {
        return body;
    }
    body.chars().take(MAX_ERROR_BODY_CHARS).collect()
}

/// `Retry-After` in its delta-seconds form → milliseconds. The HTTP-date
/// form is not honored (returns `None`); the engine's own backoff applies.
fn parse_retry_after_ms(value: &str) -> Option<u64> {
    value
        .trim()
        .parse::<u64>()
        .ok()
        .map(|seconds| seconds.saturating_mul(1_000))
}

/// Render a `ModelRequest` into an OpenRouter chat-completions body.
///
/// - `system_prompt` (when non-empty) leads as a system message, then the
///   session messages in order.
/// - Assistant tool calls are rebuilt from `MessageMeta::raw_tool_calls`
///   into the OpenAI shape; a call without an id gets a deterministic
///   `call_<index>` (its position within the message).
/// - A tool message with a `call_id` renders as an OpenAI `tool`
///   message. One without a `call_id` is never a genuine result in this
///   engine — it is engine-authored model-facing feedback (envelope
///   parse failure, tools-blocked nudge) and renders as a user turn so
///   the feedback reaches the model instead of being dropped or
///   bricking the session (deviation from the FRG-16 plan, which assumed
///   such messages could only be corrupt history; see plan status).
/// - Tools are sent only when the request allows tool calls. The source
///   is `rendered_runtime_tools` when non-empty (the active channel
///   through the runtime gates) and `tool_schemas` otherwise; each entry
///   value is wrapped as `{"type": "function", "function": <value>}`.
pub fn render_request_body(request: &ModelRequest, model: &str) -> Result<Value, TransportError> {
    let mut messages = Vec::new();
    if !request.system_prompt.is_empty() {
        messages.push(json!({ "role": "system", "content": request.system_prompt }));
    }
    for message in &request.session_messages {
        messages.push(render_message(message)?);
    }

    let mut body = Map::new();
    body.insert("model".to_string(), json!(model));
    body.insert("messages".to_string(), Value::Array(messages));
    body.insert("max_tokens".to_string(), json!(MAX_TOKENS));
    body.insert("temperature".to_string(), json!(TEMPERATURE));

    if request.allow_tool_call {
        let source: &[SchemaEntry] = if !request.rendered_runtime_tools.is_empty() {
            &request.rendered_runtime_tools
        } else {
            &request.tool_schemas
        };
        if !source.is_empty() {
            let tools: Vec<Value> = source
                .iter()
                .map(|entry| json!({ "type": "function", "function": entry.value }))
                .collect();
            body.insert("tools".to_string(), Value::Array(tools));
            body.insert("tool_choice".to_string(), json!("auto"));
        }
    }

    Ok(Value::Object(body))
}

fn render_message(message: &AgentMessage) -> Result<Value, TransportError> {
    match message.role {
        // A compaction summary stands in for elided history; like the
        // system prompt it is engine-authored context, not a user turn.
        AgentRole::System | AgentRole::CompactSummary => {
            Ok(json!({ "role": "system", "content": message.content }))
        }
        AgentRole::User => Ok(json!({ "role": "user", "content": message.content })),
        AgentRole::Assistant => {
            let mut rendered = Map::new();
            rendered.insert("role".to_string(), json!("assistant"));
            if message.content.is_empty() {
                rendered.insert("content".to_string(), Value::Null);
            } else {
                rendered.insert("content".to_string(), json!(message.content));
            }
            let raw_calls = message.metadata.raw_tool_calls();
            if !raw_calls.is_empty() {
                let calls: Vec<Value> = raw_calls
                    .iter()
                    .enumerate()
                    .map(|(index, call)| {
                        json!({
                            "id": call
                                .id
                                .clone()
                                .unwrap_or_else(|| format!("call_{index}")),
                            "type": "function",
                            "function": {
                                "name": call.name,
                                "arguments": call.arguments_json,
                            },
                        })
                    })
                    .collect();
                rendered.insert("tool_calls".to_string(), Value::Array(calls));
            }
            Ok(Value::Object(rendered))
        }
        AgentRole::Tool => match message.metadata.call_id() {
            Some(call_id) => Ok(json!({
                "role": "tool",
                "tool_call_id": call_id,
                "content": message.content,
            })),
            // Engine invariant (agent_core model_intake/tool_results):
            // every genuine tool result carries a call_id. The only
            // Tool-role messages without one are engine-authored
            // feedback for the model — the envelope parse-failure error
            // and the tools-blocked nudge, whose payloads are literally
            // named `*_for_model`. They answer no call, so a `tool`
            // rendering would be an orphan the provider rejects, and
            // erroring would brick the session on the very event the
            // feedback exists to recover from. Rendered as a user turn:
            // the content reaches the model, nothing is dropped.
            None => Ok(json!({ "role": "user", "content": message.content })),
        },
    }
}

/// Map an OpenRouter chat-completions response body onto a
/// `ModelResponse`. An `error` object in the body, a missing/empty
/// `choices` array, or a tool call with no function name are typed
/// `MalformedResponse` errors — never panics or silent empties.
pub fn map_response(value: &Value) -> Result<ModelResponse, TransportError> {
    if let Some(error) = value.get("error").filter(|error| !error.is_null()) {
        let detail = error
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("provider returned an error object");
        return Err(TransportError::MalformedResponse(format!(
            "provider error: {detail}"
        )));
    }
    let choices = value
        .get("choices")
        .and_then(Value::as_array)
        .filter(|choices| !choices.is_empty())
        .ok_or_else(|| TransportError::MalformedResponse("response has no choices".to_string()))?;
    let choice = &choices[0];
    let message = choice.get("message").ok_or_else(|| {
        TransportError::MalformedResponse("first choice has no message".to_string())
    })?;

    let assistant_text = match message.get("content") {
        Some(Value::String(content)) if !content.is_empty() => Some(content.clone()),
        // Array-shaped content (the multipart form some providers emit):
        // concatenate the text parts; other part types carry no text.
        Some(Value::Array(parts)) => {
            let text: String = parts
                .iter()
                .filter(|part| part.get("type").and_then(Value::as_str) == Some("text"))
                .filter_map(|part| part.get("text").and_then(Value::as_str))
                .collect();
            if text.is_empty() {
                None
            } else {
                Some(text)
            }
        }
        _ => None,
    };

    let mut tool_calls = Vec::new();
    if let Some(calls) = message.get("tool_calls").and_then(Value::as_array) {
        for (index, call) in calls.iter().enumerate() {
            let function = call.get("function");
            let name = function
                .and_then(|function| function.get("name"))
                .and_then(Value::as_str)
                .filter(|name| !name.is_empty())
                .ok_or_else(|| {
                    TransportError::MalformedResponse(
                        "tool_call is missing function.name".to_string(),
                    )
                })?;
            let arguments_json = match function.and_then(|function| function.get("arguments")) {
                Some(Value::String(arguments)) => arguments.clone(),
                Some(other) => other.to_string(),
                None => "{}".to_string(),
            };
            // A call without an id (missing or empty) gets a
            // deterministic synthesized one — `call_<index>`, its
            // position in the calls array — so repair, replay,
            // pairing, and rendering all share one id instead of the
            // call carrying `None` through ingest.
            let id = call
                .get("id")
                .and_then(Value::as_str)
                .filter(|id| !id.is_empty())
                .map(str::to_string)
                .unwrap_or_else(|| format!("call_{index}"));
            tool_calls.push(RawToolCall {
                id: Some(id),
                name: name.to_string(),
                arguments_json,
            });
        }
    }

    let provider_reasoning_content = message
        .get("reasoning")
        .and_then(Value::as_str)
        .filter(|reasoning| !reasoning.is_empty())
        .map(str::to_string);

    // A mapped response with no text, no tool calls, and no reasoning
    // carries nothing the engine can act on; handing it back as a
    // silent empty ModelResponse would strand the turn. It is a
    // malformed response, typed as such.
    if assistant_text.is_none() && tool_calls.is_empty() && provider_reasoning_content.is_none() {
        return Err(TransportError::MalformedResponse(
            "response message has no text, tool calls, or reasoning".to_string(),
        ));
    }

    let raw_finish = choice
        .get("finish_reason")
        .and_then(Value::as_str)
        .unwrap_or("");
    let finish_reason = if !tool_calls.is_empty() || raw_finish == "tool_calls" {
        FinishReason::ToolUse
    } else {
        match raw_finish {
            "stop" => FinishReason::Stop,
            "length" => FinishReason::MaxTokens,
            other => FinishReason::Other(other.to_string()),
        }
    };

    Ok(ModelResponse {
        assistant_text,
        tool_calls,
        finish_reason,
        provider_reasoning_content,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_core::message::MessageMeta;

    fn message(id: &str, role: AgentRole, content: &str) -> AgentMessage {
        AgentMessage {
            id: id.to_string(),
            role,
            content: content.to_string(),
            metadata: MessageMeta::new(),
        }
    }

    fn request(messages: Vec<AgentMessage>) -> ModelRequest {
        ModelRequest {
            session_messages: messages,
            system_prompt: "You review code.".to_string(),
            ..ModelRequest::default()
        }
    }

    fn schema(name: &str) -> SchemaEntry {
        SchemaEntry::from(json!({
            "name": name,
            "parameters": { "type": "object" },
        }))
    }

    // -- rendering ------------------------------------------------------

    #[test]
    fn system_prompt_renders_first() {
        let body = render_request_body(
            &request(vec![message("u1", AgentRole::User, "hi")]),
            "some/model",
        )
        .expect("render");
        assert_eq!(body["model"], "some/model");
        assert_eq!(body["max_tokens"], 16_000);
        assert_eq!(body["temperature"], 0.1);
        let messages = body["messages"].as_array().expect("messages array");
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0]["role"], "system");
        assert_eq!(messages[0]["content"], "You review code.");
        assert_eq!(messages[1]["role"], "user");
        assert_eq!(messages[1]["content"], "hi");
    }

    #[test]
    fn empty_system_prompt_is_omitted() {
        let mut req = request(vec![message("u1", AgentRole::User, "hi")]);
        req.system_prompt = String::new();
        let body = render_request_body(&req, "m").expect("render");
        let messages = body["messages"].as_array().expect("messages array");
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0]["role"], "user");
    }

    #[test]
    fn assistant_tool_calls_replay_with_synthesized_id() {
        let mut assistant = message("a1", AgentRole::Assistant, "");
        assistant.metadata = MessageMeta::new().with_raw_tool_calls(vec![
            RawToolCall {
                id: Some("call_9".to_string()),
                name: "web_search".to_string(),
                arguments_json: "{\"query\":\"x\"}".to_string(),
            },
            RawToolCall {
                id: None,
                name: "read_file".to_string(),
                arguments_json: "{}".to_string(),
            },
        ]);
        let body = render_request_body(&request(vec![assistant]), "m").expect("render");
        let rendered = &body["messages"][1];
        assert_eq!(rendered["role"], "assistant");
        assert_eq!(rendered["content"], Value::Null);
        let calls = rendered["tool_calls"].as_array().expect("tool_calls");
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0]["id"], "call_9");
        assert_eq!(calls[0]["type"], "function");
        assert_eq!(calls[0]["function"]["name"], "web_search");
        assert_eq!(calls[0]["function"]["arguments"], "{\"query\":\"x\"}");
        // Missing id → deterministic call_<index> from the position
        // within the message.
        assert_eq!(calls[1]["id"], "call_1");
        assert_eq!(calls[1]["function"]["name"], "read_file");
    }

    #[test]
    fn tool_message_renders_its_call_id() {
        let mut tool = message("t1", AgentRole::Tool, "{\"results\":[]}");
        tool.metadata = MessageMeta::new().with_call_id("call_9");
        let body = render_request_body(&request(vec![tool]), "m").expect("render");
        let rendered = &body["messages"][1];
        assert_eq!(rendered["role"], "tool");
        assert_eq!(rendered["tool_call_id"], "call_9");
        assert_eq!(rendered["content"], "{\"results\":[]}");
    }

    #[test]
    fn tool_message_without_call_id_renders_as_user_feedback() {
        // The engine's envelope parse-failure and tools-blocked feedback
        // messages are Tool-role with no call_id; they must reach the
        // model as a user turn, not vanish and not fail the render.
        let feedback = message("tool-err-envelope-a0", AgentRole::Tool, "{\"error\":\"x\"}");
        let body = render_request_body(&request(vec![feedback]), "m").expect("render");
        let rendered = &body["messages"][1];
        assert_eq!(rendered["role"], "user");
        assert_eq!(rendered["content"], "{\"error\":\"x\"}");
        assert!(rendered.get("tool_call_id").is_none());
    }

    #[test]
    fn tools_wrap_schema_entries_and_prefer_rendered_channel() {
        let mut req = request(vec![]);
        req.tool_schemas = vec![schema("catalog_tool")];
        req.rendered_runtime_tools = vec![schema("active_tool")];
        let body = render_request_body(&req, "m").expect("render");
        let tools = body["tools"].as_array().expect("tools array");
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0]["type"], "function");
        assert_eq!(tools[0]["function"]["name"], "active_tool");
        assert_eq!(body["tool_choice"], "auto");

        // Rendered channel empty → the catalog channel is the source.
        req.rendered_runtime_tools = vec![];
        let body = render_request_body(&req, "m").expect("render");
        let tools = body["tools"].as_array().expect("tools array");
        assert_eq!(tools[0]["function"]["name"], "catalog_tool");
    }

    #[test]
    fn tools_omitted_when_tool_calls_disallowed() {
        let mut req = request(vec![]);
        req.allow_tool_call = false;
        req.tool_schemas = vec![schema("web_search")];
        let body = render_request_body(&req, "m").expect("render");
        assert!(body.get("tools").is_none());
        assert!(body.get("tool_choice").is_none());
    }

    // -- response mapping -------------------------------------------------

    #[test]
    fn maps_native_tool_calls() {
        let response = map_response(&json!({
            "choices": [{
                "message": {
                    "role": "assistant",
                    "content": null,
                    "tool_calls": [{
                        "id": "call_1",
                        "type": "function",
                        "function": {
                            "name": "web_search",
                            "arguments": "{\"query\":\"chomp\"}",
                        },
                    }],
                },
                "finish_reason": "tool_calls",
            }],
        }))
        .expect("map");
        assert!(response.assistant_text.is_none());
        assert_eq!(response.tool_calls.len(), 1);
        assert_eq!(response.tool_calls[0].id.as_deref(), Some("call_1"));
        assert_eq!(response.tool_calls[0].name, "web_search");
        assert_eq!(
            response.tool_calls[0].arguments_json,
            "{\"query\":\"chomp\"}"
        );
        assert!(matches!(response.finish_reason, FinishReason::ToolUse));
    }

    #[test]
    fn maps_object_arguments_by_stringifying() {
        let response = map_response(&json!({
            "choices": [{
                "message": {
                    "tool_calls": [{
                        "function": {
                            "name": "web_search",
                            "arguments": { "query": "chomp" },
                        },
                    }],
                },
                "finish_reason": "tool_calls",
            }],
        }))
        .expect("map");
        assert_eq!(
            response.tool_calls[0].arguments_json,
            "{\"query\":\"chomp\"}"
        );
        // No id on the wire → the deterministic synthesized call_0.
        assert_eq!(response.tool_calls[0].id.as_deref(), Some("call_0"));
    }

    #[test]
    fn idless_tool_calls_get_deterministic_synthesized_ids() {
        let response = map_response(&json!({
            "choices": [{
                "message": {
                    "tool_calls": [
                        {
                            "type": "function",
                            "function": { "name": "web_search", "arguments": "{}" },
                        },
                        {
                            "id": "",
                            "type": "function",
                            "function": { "name": "read_file", "arguments": "{}" },
                        },
                        {
                            "id": "call_9",
                            "type": "function",
                            "function": { "name": "grep", "arguments": "{}" },
                        },
                    ],
                },
                "finish_reason": "tool_calls",
            }],
        }))
        .expect("map");
        let ids: Vec<Option<&str>> = response
            .tool_calls
            .iter()
            .map(|call| call.id.as_deref())
            .collect();
        // Missing and empty ids synthesize from the position in the
        // calls array; a present id passes through untouched.
        assert_eq!(ids, vec![Some("call_0"), Some("call_1"), Some("call_9")]);
    }

    #[test]
    fn maps_array_shaped_content_by_concatenating_text_parts() {
        let response = map_response(&json!({
            "choices": [{
                "message": {
                    "content": [
                        { "type": "text", "text": "Hello, " },
                        { "type": "image_url", "image_url": { "url": "https://x" } },
                        { "type": "text", "text": "world." },
                    ],
                },
                "finish_reason": "stop",
            }],
        }))
        .expect("map");
        assert_eq!(response.assistant_text.as_deref(), Some("Hello, world."));
        assert!(response.tool_calls.is_empty());
        assert!(matches!(response.finish_reason, FinishReason::Stop));
    }

    #[test]
    fn empty_mapped_response_is_a_typed_error() {
        // Content present but carrying no text parts, no tool calls,
        // no reasoning: a silent empty ModelResponse is not an option.
        let error = map_response(&json!({
            "choices": [{
                "message": {
                    "content": [{ "type": "image_url", "image_url": { "url": "https://x" } }],
                },
                "finish_reason": "stop",
            }],
        }))
        .expect_err("contentless response must fail");
        assert!(matches!(error, TransportError::MalformedResponse(_)));

        let error = map_response(&json!({
            "choices": [{
                "message": { "content": "" },
                "finish_reason": "stop",
            }],
        }))
        .expect_err("empty-text response must fail");
        assert!(matches!(error, TransportError::MalformedResponse(_)));
    }

    #[test]
    fn maps_text_only_and_captures_reasoning() {
        let response = map_response(&json!({
            "choices": [{
                "message": {
                    "content": "All clear.",
                    "reasoning": "checked the diff",
                },
                "finish_reason": "stop",
            }],
        }))
        .expect("map");
        assert_eq!(response.assistant_text.as_deref(), Some("All clear."));
        assert!(response.tool_calls.is_empty());
        assert!(matches!(response.finish_reason, FinishReason::Stop));
        assert_eq!(
            response.provider_reasoning_content.as_deref(),
            Some("checked the diff")
        );
    }

    #[test]
    fn finish_reason_table() {
        let with_finish = |finish: &str| {
            map_response(&json!({
                "choices": [{
                    "message": { "content": "x" },
                    "finish_reason": finish,
                }],
            }))
            .expect("map")
            .finish_reason
        };
        assert!(matches!(with_finish("stop"), FinishReason::Stop));
        assert!(matches!(with_finish("length"), FinishReason::MaxTokens));
        assert!(matches!(
            with_finish("content_filter"),
            FinishReason::Other(other) if other == "content_filter"
        ));
        // "tool_calls" maps to ToolUse even with no parsed calls.
        assert!(matches!(with_finish("tool_calls"), FinishReason::ToolUse));
    }

    #[test]
    fn error_body_is_a_typed_error() {
        let error = map_response(&json!({
            "error": { "message": "invalid api key", "code": 401 },
        }))
        .expect_err("error body must fail");
        assert!(
            matches!(error, TransportError::MalformedResponse(_)),
            "expected MalformedResponse, got {error:?}"
        );
    }

    #[test]
    fn empty_choices_is_a_typed_error() {
        let error = map_response(&json!({ "choices": [] })).expect_err("empty choices must fail");
        assert!(matches!(error, TransportError::MalformedResponse(_)));
        let error = map_response(&json!({})).expect_err("missing choices must fail");
        assert!(matches!(error, TransportError::MalformedResponse(_)));
    }

    // -- failure reports --------------------------------------------------

    #[test]
    fn failure_report_classification_table() {
        let http = |status: u16| TransportError::Http {
            status,
            body: "boom".to_string(),
            retry_after_ms: None,
        };
        let report = http(429).to_failure_report();
        assert_eq!(report.error_class, ModelApiErrorClass::Transient);
        assert_eq!(report.http_status, Some(429));
        assert_eq!(
            http(500).to_failure_report().error_class,
            ModelApiErrorClass::Transient
        );
        assert_eq!(
            http(503).to_failure_report().error_class,
            ModelApiErrorClass::Transient
        );
        assert_eq!(
            http(400).to_failure_report().error_class,
            ModelApiErrorClass::Permanent
        );
        assert_eq!(
            http(401).to_failure_report().error_class,
            ModelApiErrorClass::Permanent
        );
        assert_eq!(
            http(404).to_failure_report().error_class,
            ModelApiErrorClass::Permanent
        );

        let network = TransportError::Network("dns".to_string()).to_failure_report();
        assert_eq!(network.error_class, ModelApiErrorClass::Transient);
        assert_eq!(network.http_status, None);

        let malformed =
            TransportError::MalformedResponse("no choices".to_string()).to_failure_report();
        assert_eq!(malformed.error_class, ModelApiErrorClass::Transient);

        let missing = TransportError::MissingApiKey.to_failure_report();
        assert_eq!(missing.error_class, ModelApiErrorClass::Permanent);
        assert!(!missing.had_partial_output);
        assert_eq!(missing.partial_output_chars, 0);
    }

    #[test]
    fn failure_report_carries_retry_after() {
        let error = TransportError::Http {
            status: 429,
            body: "slow down".to_string(),
            retry_after_ms: Some(7_000),
        };
        let report = error.to_failure_report();
        assert_eq!(report.retry_after_ms, Some(7_000));
        assert!(report.message.contains("429"));
    }

    // -- model resolution -------------------------------------------------

    #[test]
    fn model_resolution_table() {
        // Unset → the default.
        assert_eq!(resolve_model(None), DEFAULT_MODEL);
        // A plain value passes through as itself.
        assert_eq!(
            resolve_model(Some("anthropic/claude-sonnet-4".to_string())),
            "anthropic/claude-sonnet-4"
        );
        // The openrouter/ prefix is stripped.
        assert_eq!(
            resolve_model(Some("openrouter/anthropic/claude-sonnet-4".to_string())),
            "anthropic/claude-sonnet-4"
        );
        // Prefix-only strips to empty → the default (the prefix is
        // stripped before the emptiness check).
        assert_eq!(
            resolve_model(Some("openrouter/".to_string())),
            DEFAULT_MODEL
        );
        // Blank values fall back too.
        assert_eq!(resolve_model(Some(String::new())), DEFAULT_MODEL);
        assert_eq!(resolve_model(Some("  ".to_string())), DEFAULT_MODEL);
    }

    #[test]
    fn error_body_truncation_caps_at_the_limit() {
        let short = "boom".to_string();
        assert_eq!(truncate_error_body(short.clone()), short);
        let long = "x".repeat(MAX_ERROR_BODY_CHARS + 500);
        let truncated = truncate_error_body(long);
        assert_eq!(truncated.chars().count(), MAX_ERROR_BODY_CHARS);
    }

    #[test]
    fn retry_after_parsing() {
        assert_eq!(parse_retry_after_ms("7"), Some(7_000));
        assert_eq!(parse_retry_after_ms(" 12 "), Some(12_000));
        assert_eq!(parse_retry_after_ms("Wed, 21 Oct 2015 07:28:00 GMT"), None);
        assert_eq!(parse_retry_after_ms("soon"), None);
    }
}
