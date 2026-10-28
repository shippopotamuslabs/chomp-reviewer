//! FRG-16 end-to-end: a real `agent_core` engine + session driven
//! through the real `OpenRouterTransport` against a stub HTTP server.
//!
//! Modeled on agent_core's `examples/mock_desktop_loop.rs`: the test
//! plays the host pull loop — `CallModel` goes through the transport,
//! `ExecuteTools` is serviced with a canned result, failures are fed
//! back via `submit_model_turn_failure` with the transport's failure
//! report. The stub server (std-only `TcpListener` thread) answers from
//! a script and records every request body, so the tests assert on the
//! exact bytes the transport put on the wire.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use agent_core::action::AgentAction;
use agent_core::catalog::ToolSpec;
use agent_core::engine::{AgentEngine, SessionHandle, SessionIoParams};
use agent_core::message::{AgentMessage, AgentRole, MessageMeta, ToolResultRecord};
use agent_core::session::{AgentConfig, SessionCapabilities};
use agent_core::tool_call_format::ToolCallFormat;
use agent_core::tool_name::ToolName;
use agent_core::tool_semantics::{ToolCatalogConfig, ToolSemanticsConfig};
use chomp::transport::OpenRouterTransport;
use serde_json::Value;

const TOOL_SCHEMA: &str = r#"[{"name":"web_search","description":"Search the web","parameters":{"type":"object","properties":{"query":{"type":"string"}}}}]"#;

const NATIVE_TOOL_CALL_RESPONSE: &str = r#"{
    "choices": [{
        "message": {
            "role": "assistant",
            "content": null,
            "tool_calls": [{
                "id": "call_1",
                "type": "function",
                "function": {"name": "web_search", "arguments": "{\"query\":\"chomp\"}"}
            }]
        },
        "finish_reason": "tool_calls"
    }]
}"#;

const ENVELOPE_TOOL_CALL_RESPONSE: &str = r#"{
    "choices": [{
        "message": {
            "role": "assistant",
            "content": "Searching now.\n[TOOL_CALLS] [{\"name\": \"web_search\", \"arguments\": {\"query\": \"chomp\"}}]"
        },
        "finish_reason": "stop"
    }]
}"#;

const TEXT_RESPONSE: &str = r#"{
    "choices": [{
        "message": {"role": "assistant", "content": "The answer."},
        "finish_reason": "stop"
    }]
}"#;

// ---------------------------------------------------------------------------
// Stub HTTP server
// ---------------------------------------------------------------------------

struct StubServer {
    base_url: String,
    requests: Arc<Mutex<Vec<String>>>,
    handle: Option<thread::JoinHandle<()>>,
}

impl StubServer {
    /// Serve each scripted `(status, body)` response in order, one per
    /// connection, then stop listening.
    fn start(script: Vec<(u16, String)>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind stub server");
        let addr = listener.local_addr().expect("stub server addr");
        let requests = Arc::new(Mutex::new(Vec::new()));
        let recorded = Arc::clone(&requests);
        let handle = thread::spawn(move || {
            for (status, body) in script {
                let Ok((mut stream, _)) = listener.accept() else {
                    return;
                };
                let request = read_request(&mut stream);
                recorded.lock().expect("request log lock").push(request);
                let reason = match status {
                    200 => "OK",
                    500 => "Internal Server Error",
                    _ => "Status",
                };
                let response = format!(
                    "HTTP/1.1 {status} {reason}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len(),
                );
                let _ = stream.write_all(response.as_bytes());
                let _ = stream.flush();
            }
        });
        Self {
            base_url: format!("http://{addr}"),
            requests,
            handle: Some(handle),
        }
    }

    /// The headers of each recorded request, in order, as
    /// `(name, value)` pairs (the request line is not a header).
    fn recorded_headers(&self) -> Vec<Vec<(String, String)>> {
        self.requests
            .lock()
            .expect("request log lock")
            .iter()
            .map(|request| {
                let head = request
                    .split("\r\n\r\n")
                    .next()
                    .expect("request has a header block");
                head.lines()
                    .skip(1)
                    .filter_map(|line| {
                        line.split_once(':').map(|(name, value)| {
                            (name.trim().to_string(), value.trim().to_string())
                        })
                    })
                    .collect()
            })
            .collect()
    }

    fn recorded_bodies(&self) -> Vec<Value> {
        self.requests
            .lock()
            .expect("request log lock")
            .iter()
            .map(|request| {
                let body = request
                    .split("\r\n\r\n")
                    .nth(1)
                    .expect("request has a body separator");
                serde_json::from_str(body).expect("request body is JSON")
            })
            .collect()
    }

    fn request_count(&self) -> usize {
        self.requests.lock().expect("request log lock").len()
    }
}

impl Drop for StubServer {
    fn drop(&mut self) {
        if let Some(handle) = self.handle.take() {
            // The script may legitimately have unplayed responses left
            // (e.g. the engine goes terminal early); never block on it.
            drop(handle);
        }
    }
}

fn read_request(stream: &mut TcpStream) -> String {
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .expect("set read timeout");
    let mut raw = Vec::new();
    let mut chunk = [0u8; 4096];
    let mut expected_len: Option<usize> = None;
    loop {
        match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                raw.extend_from_slice(&chunk[..n]);
                if expected_len.is_none() {
                    if let Some(pos) = find_subslice(&raw, b"\r\n\r\n") {
                        let headers = String::from_utf8_lossy(&raw[..pos]).to_lowercase();
                        let content_length = headers
                            .lines()
                            .find_map(|line| line.strip_prefix("content-length:"))
                            .and_then(|value| value.trim().parse::<usize>().ok())
                            .unwrap_or(0);
                        expected_len = Some(pos + 4 + content_length);
                    }
                }
                if let Some(total) = expected_len {
                    if raw.len() >= total {
                        break;
                    }
                }
            }
            Err(_) => break,
        }
    }
    String::from_utf8_lossy(&raw).into_owned()
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

// ---------------------------------------------------------------------------
// Host pull loop
// ---------------------------------------------------------------------------

fn new_session(engine: &AgentEngine, config: AgentConfig) -> SessionHandle {
    engine
        .create_session(
            vec![AgentMessage {
                id: "u1".to_string(),
                role: AgentRole::User,
                content: "search for chomp".to_string(),
                metadata: MessageMeta::new(),
            }],
            config,
            SessionCapabilities {
                summarization_allowed: false,
                kv_fixed: false,
                tool_result_budget_tokens: None,
                ..Default::default()
            },
            Value::Null,
            SessionIoParams::new(
                "openrouter".to_string(),
                200_000,
                TOOL_SCHEMA.to_string(),
                "You review code.".to_string(),
            )
            .expect("valid io params"),
        )
        .expect("valid config creates session")
}

enum LoopOutcome {
    Finished,
    Failed(String),
}

/// Drive the engine through the transport until a terminal action.
/// Tool executions are serviced with one canned result per call.
fn drive(
    engine: &AgentEngine,
    handle: &SessionHandle,
    transport: &OpenRouterTransport,
) -> LoopOutcome {
    for _ in 0..32 {
        match engine.next_action(handle) {
            AgentAction::CallModel(request) => match transport.complete_model_turn(&request) {
                Ok(response) => engine.submit_model_response(handle, response),
                Err(error) => engine.submit_model_turn_failure(handle, error.to_failure_report()),
            },
            AgentAction::ExecuteTools(calls) => {
                let results = calls
                    .iter()
                    .map(|call| ToolResultRecord {
                        call_id: call.call_id.clone(),
                        result_json: "{\"results\":[\"hit\"]}".to_string(),
                        truncated: false,
                        soft_failure: false,
                        activated_tool_names: Vec::new(),
                        result_metadata: None,
                    })
                    .collect();
                engine.submit_tool_results(handle, results);
            }
            AgentAction::Finished => return LoopOutcome::Finished,
            AgentAction::Failed(error) => return LoopOutcome::Failed(format!("{error:?}")),
            AgentAction::RequestSummary(_) => {
                panic!("summarization is disabled in these sessions")
            }
            AgentAction::ManualCompactCompleted(_) => {
                panic!("manual compaction is not expected in these sessions")
            }
        }
    }
    panic!("loop did not reach a terminal action within 32 steps");
}

fn transport_for(stub: &StubServer) -> OpenRouterTransport {
    OpenRouterTransport::new("test-key", "test/model", stub.base_url.clone())
}

/// Case-insensitive header lookup in one recorded request's headers.
fn header<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(header_name, _)| header_name.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.as_str())
}

// ---------------------------------------------------------------------------
// Scenarios
// ---------------------------------------------------------------------------

#[test]
fn native_tool_call_path() {
    let stub = StubServer::start(vec![
        (200, NATIVE_TOOL_CALL_RESPONSE.to_string()),
        (200, TEXT_RESPONSE.to_string()),
    ]);
    let engine = AgentEngine::new();
    let handle = new_session(&engine, AgentConfig::default());
    let transport = transport_for(&stub);

    let outcome = drive(&engine, &handle, &transport);
    assert!(
        matches!(outcome, LoopOutcome::Finished),
        "native path must finish"
    );

    let bodies = stub.recorded_bodies();
    assert_eq!(bodies.len(), 2, "two model turns went over the wire");

    // The wire requests carry the transport's pinned headers: the
    // bearer key, the JSON content type, and the OpenRouter
    // attribution pair.
    let headers = stub.recorded_headers();
    assert_eq!(headers.len(), 2);
    for request_headers in &headers {
        assert_eq!(
            header(request_headers, "Authorization"),
            Some("Bearer test-key")
        );
        assert_eq!(
            header(request_headers, "Content-Type"),
            Some("application/json")
        );
        assert_eq!(
            header(request_headers, "HTTP-Referer"),
            Some("https://github.com/shippopotamuslabs/forge-reviewer")
        );
        assert_eq!(header(request_headers, "X-Title"), Some("Chomp"));
    }

    // First request carries the rendered tool schema.
    let tools = bodies[0]["tools"].as_array().expect("tools rendered");
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0]["type"], "function");
    assert_eq!(tools[0]["function"]["name"], "web_search");
    assert_eq!(bodies[0]["messages"][0]["role"], "system");
    assert_eq!(bodies[0]["messages"][1]["role"], "user");

    // Second request replays the assistant tool call and the tool result
    // as a matched pair.
    let replay = bodies[1]["messages"].as_array().expect("messages");
    let assistant = replay
        .iter()
        .find(|message| message["role"] == "assistant")
        .expect("assistant replay present");
    assert_eq!(assistant["tool_calls"][0]["id"], "call_1");
    assert_eq!(assistant["tool_calls"][0]["function"]["name"], "web_search");
    assert_eq!(
        assistant["tool_calls"][0]["function"]["arguments"],
        "{\"query\":\"chomp\"}"
    );
    let tool = replay
        .iter()
        .find(|message| message["role"] == "tool")
        .expect("tool result present");
    assert_eq!(tool["tool_call_id"], "call_1");
    assert!(
        tool["content"]
            .as_str()
            .expect("tool content")
            .contains("hit"),
        "tool content carries the result: {tool}"
    );
}

#[test]
fn envelope_tool_call_path() {
    let stub = StubServer::start(vec![
        (200, ENVELOPE_TOOL_CALL_RESPONSE.to_string()),
        (200, TEXT_RESPONSE.to_string()),
    ]);
    let engine = AgentEngine::new();
    // The host declares the bracket-array envelope format for this
    // session; agent_core's ingest — not the transport — extracts the
    // call from the assistant text. Envelope recovery validates the
    // tool against the semantics catalog (not the schema channel), so
    // the session config declares web_search there too — the same
    // pattern as agent_core's own envelope tests and their enclave
    // semantics fixture.
    let handle = new_session(
        &engine,
        AgentConfig {
            tool_call_format: ToolCallFormat::BracketArray,
            tool_semantics: ToolSemanticsConfig {
                catalog: ToolCatalogConfig {
                    tools: vec![ToolSpec {
                        name: ToolName::new("web_search").expect("valid tool name"),
                        aliases: vec![],
                        reproducible: true,
                        schema_hint: None,
                    }],
                },
                ..ToolSemanticsConfig::agnostic()
            },
            ..AgentConfig::default()
        },
    );
    let transport = transport_for(&stub);

    // Drive manually so the ExecuteTools step itself is asserted.
    let mut saw_execute_tools = false;
    let mut finished = false;
    for _ in 0..32 {
        match engine.next_action(&handle) {
            AgentAction::CallModel(request) => {
                let response = transport
                    .complete_model_turn(&request)
                    .expect("stub answers every scripted turn");
                engine.submit_model_response(&handle, response);
            }
            AgentAction::ExecuteTools(calls) => {
                saw_execute_tools = true;
                assert_eq!(calls.len(), 1);
                assert_eq!(calls[0].name, "web_search");
                assert!(
                    calls[0].arguments_json.contains("chomp"),
                    "envelope arguments extracted: {}",
                    calls[0].arguments_json
                );
                let results = calls
                    .iter()
                    .map(|call| ToolResultRecord {
                        call_id: call.call_id.clone(),
                        result_json: "{\"results\":[\"hit\"]}".to_string(),
                        truncated: false,
                        soft_failure: false,
                        activated_tool_names: Vec::new(),
                        result_metadata: None,
                    })
                    .collect();
                engine.submit_tool_results(&handle, results);
            }
            AgentAction::Finished => {
                finished = true;
                break;
            }
            other => panic!("unexpected action: {other:?}"),
        }
    }
    assert!(saw_execute_tools, "envelope call must reach ExecuteTools");
    assert!(finished, "envelope path must finish");
    assert_eq!(stub.request_count(), 2);

    // Second request replays the envelope-extracted call and the tool
    // result as a matched pair under the engine-assigned envelope id,
    // mirroring the native scenario's replay assertions.
    let bodies = stub.recorded_bodies();
    assert_eq!(bodies.len(), 2, "two model turns went over the wire");
    let replay = bodies[1]["messages"].as_array().expect("messages");
    let assistant = replay
        .iter()
        .find(|message| message["role"] == "assistant")
        .expect("assistant replay present");
    assert_eq!(assistant["tool_calls"][0]["id"], "text-tool-1");
    assert_eq!(assistant["tool_calls"][0]["function"]["name"], "web_search");
    assert!(
        assistant["tool_calls"][0]["function"]["arguments"]
            .as_str()
            .expect("arguments")
            .contains("chomp"),
        "envelope arguments replayed: {assistant}"
    );
    let tool = replay
        .iter()
        .find(|message| message["role"] == "tool")
        .expect("tool result present");
    assert_eq!(tool["tool_call_id"], "text-tool-1");
    assert!(
        tool["content"]
            .as_str()
            .expect("tool content")
            .contains("hit"),
        "tool content carries the result: {tool}"
    );
}

#[test]
fn http_500_failure_path_retries_then_fails_terminally() {
    // Default retry limit is 3: the initial attempt plus three retries
    // all meet a 500, then the engine goes terminally Failed.
    let stub = StubServer::start(vec![
        (
            500,
            "{\"error\":{\"message\":\"upstream boom\"}}".to_string(),
        ),
        (
            500,
            "{\"error\":{\"message\":\"upstream boom\"}}".to_string(),
        ),
        (
            500,
            "{\"error\":{\"message\":\"upstream boom\"}}".to_string(),
        ),
        (
            500,
            "{\"error\":{\"message\":\"upstream boom\"}}".to_string(),
        ),
    ]);
    let engine = AgentEngine::new();
    let handle = new_session(&engine, AgentConfig::default());
    let transport = transport_for(&stub);

    // First turn manually: the failure report must schedule a retry —
    // the next action is CallModel again, not a terminal action.
    let AgentAction::CallModel(request) = engine.next_action(&handle) else {
        panic!("first action must be CallModel");
    };
    let error = transport
        .complete_model_turn(&request)
        .expect_err("500 must be a transport error");
    let report = error.to_failure_report();
    assert_eq!(report.http_status, Some(500));
    engine.submit_model_turn_failure(&handle, report);
    assert!(
        matches!(engine.next_action(&handle), AgentAction::CallModel(_)),
        "a transient 500 must schedule a retry CallModel"
    );

    // Drive the rest: retries exhaust and the session fails terminally,
    // with no panic anywhere in the transport or the loop.
    let outcome = drive(&engine, &handle, &transport);
    match outcome {
        LoopOutcome::Failed(detail) => {
            assert!(
                detail.contains("model_api_retries_exhausted"),
                "terminal failure should be the retry guardrail: {detail}"
            );
        }
        LoopOutcome::Finished => panic!("exhausted retries must not finish"),
    }
    assert_eq!(
        stub.request_count(),
        4,
        "initial attempt plus the engine's three retries"
    );
}
