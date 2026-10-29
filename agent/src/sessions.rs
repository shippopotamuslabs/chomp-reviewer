//! Lens session construction: prompts, budgets, and catalog/schema
//! JSON for each review lens.
//!
//! The correctness lens lands here (FRG-19); the structure and
//! contract lenses follow in FRG-21 and FRG-22 on the same
//! construction: one `agent_core` session per lens, built by
//! [`run_correctness_session`]'s shape — system prompt from
//! `agent/prompts/`, opening context assembled from the snapshot,
//! the FRG-18 tool catalog in both channels (schema JSON and typed
//! semantics), summarization disabled.
//!
//! Opening context (diff, full contents of the changed files, repo
//! map, contract entry) is read **through the FRG-18 executor**, not
//! by direct filesystem reads: the executor is the uniform,
//! SHA-stamped path, and it redacts — opening context goes to the
//! model verbatim, and direct reads would bypass the redaction layer
//! (decision recorded in `plans/FRG-19/plan.md`).

use std::sync::Arc;

use agent_core::engine::{AgentEngine, SessionIoParams};
use agent_core::message::{AgentMessage, AgentRole, MessageMeta, ToolCallRecord};
use agent_core::session::{AgentConfig, SessionCapabilities};
use agent_core::tool_semantics::{ToolCatalogConfig, ToolSemanticsConfig};
use chomp::model_transport::ModelTransport;
use chomp::snapshot::{ChangeKind, Snapshot};
use chomp::tools::{tool_specs, ToolExecutor, TOOLS_SCHEMA_JSON};
use serde_json::Value;

use crate::host::{drive_session, DriveOutcome};
use crate::outputs::Finding;

/// The correctness system prompt, authored at
/// `agent/prompts/correctness.md` and embedded at build time (a
/// shipped binary cannot rely on a source-tree-relative path).
const CORRECTNESS_PROMPT: &str = include_str!("../prompts/correctness.md");

/// Provider label and context window the sessions are constructed
/// with, per the ticket's construction sketch.
const PROVIDER: &str = "openrouter";
const CONTEXT_WINDOW_TOKENS: u32 = 200_000;

/// How a lens session ended, in the caller's vocabulary.
///
/// Only [`LensOutcome::Completed`] carries findings fit to write.
/// The other two are recorded outcomes, never findings: `main` maps
/// them to an orchestration failure with the reason on stderr, so a
/// broken or starved session never masquerades as an empty review.
#[derive(Debug, PartialEq, Eq)]
pub enum LensOutcome {
    Completed(Vec<Finding>),
    ProducerFailure { reason: String },
    BudgetExhausted { reason: String },
}

/// Run the correctness lens over one snapshot and diff.
///
/// `agent_config` carries the caller's budgets (the engine's turn
/// and retry guardrails); the tool semantics are filled in here from
/// the FRG-18 catalog.
pub fn run_correctness_session(
    snapshot: &Snapshot,
    diff_text: &str,
    entry_file: &str,
    transport: &dyn ModelTransport,
    executor: &Arc<dyn ToolExecutor + Send + Sync>,
    agent_config: AgentConfig,
) -> LensOutcome {
    let opening = assemble_opening_context(snapshot, diff_text, entry_file, executor);

    let mut config = agent_config;
    config.tool_semantics = ToolSemanticsConfig {
        catalog: ToolCatalogConfig {
            tools: tool_specs(),
        },
        ..ToolSemanticsConfig::agnostic()
    };

    let engine = AgentEngine::new();
    let io_params = match SessionIoParams::new(
        PROVIDER.to_string(),
        CONTEXT_WINDOW_TOKENS,
        TOOLS_SCHEMA_JSON.to_string(),
        system_prompt(),
    ) {
        Ok(params) => params,
        Err(error) => {
            return LensOutcome::ProducerFailure {
                reason: format!("invalid session io params: {error}"),
            }
        }
    };
    let handle = match engine.create_session(
        vec![AgentMessage {
            id: "u1".to_string(),
            role: AgentRole::User,
            content: opening,
            metadata: MessageMeta::new(),
        }],
        config,
        SessionCapabilities {
            summarization_allowed: false,
            ..Default::default()
        },
        Value::Null,
        io_params,
    ) {
        Ok(handle) => handle,
        Err(error) => {
            return LensOutcome::ProducerFailure {
                reason: format!("session construction failed: {error:?}"),
            }
        }
    };

    match drive_session(&engine, &handle, transport, executor) {
        DriveOutcome::Finished { budget_signaled } => {
            let snapshot = engine.export_snapshot(&handle);
            let Some(final_text) = snapshot
                .session
                .messages
                .iter()
                .rev()
                .find(|message| matches!(message.role, AgentRole::Assistant))
                .map(|message| message.content.clone())
            else {
                return LensOutcome::ProducerFailure {
                    reason: "session finished without producing any assistant output".to_string(),
                };
            };
            match parse_findings(&final_text) {
                Ok(findings) => LensOutcome::Completed(findings),
                Err(reason) if budget_signaled => LensOutcome::BudgetExhausted {
                    reason: format!(
                        "turn budget exhausted before the session produced findings ({reason})"
                    ),
                },
                Err(reason) => LensOutcome::ProducerFailure { reason },
            }
        }
        DriveOutcome::Failed {
            reason,
            budget_signaled,
        } => {
            if budget_signaled || reason.contains("Guardrail exceeded") {
                LensOutcome::BudgetExhausted { reason }
            } else {
                LensOutcome::ProducerFailure { reason }
            }
        }
    }
}

/// The authored prompt with its leading HTML provenance comment
/// stripped — the comment is for the repo, not the model.
fn system_prompt() -> String {
    let trimmed = CORRECTNESS_PROMPT.trim_start();
    let body = match trimmed.strip_prefix("<!--") {
        Some(rest) => match rest.find("-->") {
            Some(end) => &rest[end + 3..],
            None => trimmed,
        },
        None => trimmed,
    };
    body.trim_start().to_string()
}

/// Assemble the opening user message: the diff, the full contents of
/// every changed file, the repo map, and the contract entry — the
/// contents read through the executor (see the module docs for why).
fn assemble_opening_context(
    snapshot: &Snapshot,
    diff_text: &str,
    entry_file: &str,
    executor: &Arc<dyn ToolExecutor + Send + Sync>,
) -> String {
    let mut out = String::new();
    out.push_str("# Pull request diff\n\n```diff\n");
    out.push_str(diff_text);
    if !diff_text.ends_with('\n') {
        out.push('\n');
    }
    out.push_str("```\n\n# Changed files (full contents at the head commit)\n");
    for changed in &snapshot.changed_files {
        let kind = match &changed.change {
            ChangeKind::Added => "added".to_string(),
            ChangeKind::Modified => "modified".to_string(),
            ChangeKind::Deleted => "deleted".to_string(),
            ChangeKind::Renamed { from } => format!("renamed from {from}"),
        };
        out.push_str(&format!("\n## {} ({kind})\n\n", changed.path));
        match read_full_file(executor, &changed.path) {
            Some(contents) if contents.is_empty() => out.push_str("(empty file)\n"),
            Some(contents) => {
                out.push_str(&contents);
                out.push('\n');
            }
            None => out
                .push_str("(not present at head — deleted, binary, or unreadable; see the diff)\n"),
        }
    }
    out.push_str(&format!(
        "\n# Repository map (head {})\n\n",
        snapshot.head_sha
    ));
    out.push_str(&snapshot.repo_map.render());
    if let Some(entry) = read_full_file(executor, entry_file) {
        if !entry.is_empty() {
            out.push_str(&format!("\n# Contract entry ({entry_file})\n\n"));
            out.push_str(&entry);
            out.push('\n');
        }
    }
    out
}

/// Read one file's full contents through the executor, paging the
/// `read_file` window until it covers the file. The returned text
/// carries the executor's line-number prefixes (grounding for
/// citations) and its redaction. `None` on any soft failure.
fn read_full_file(executor: &Arc<dyn ToolExecutor + Send + Sync>, path: &str) -> Option<String> {
    let mut out = String::new();
    let mut start_line: u64 = 1;
    let mut serial = 0u64;
    loop {
        serial += 1;
        let call = ToolCallRecord {
            call_id: format!("open-{serial}"),
            name: "read_file".to_string(),
            arguments_json: serde_json::json!({"path": path, "start_line": start_line}).to_string(),
            attempt_metadata: None,
            repair_metadata: None,
        };
        let record = executor.execute(&call);
        if record.soft_failure {
            return None;
        }
        let payload: Value = serde_json::from_str(&record.result_json).ok()?;
        let content = payload.get("content")?.as_str()?;
        let total_lines = payload.get("total_lines")?.as_u64()?;
        let end_line = payload.get("end_line")?.as_u64()?;
        if !content.is_empty() {
            out.push_str(content);
            out.push('\n');
        }
        let next_line = end_line + 1;
        if end_line < start_line || next_line > total_lines {
            return Some(out);
        }
        start_line = next_line;
    }
}

/// Parse and validate a session's final text against the findings
/// output contract. Anything but a bare JSON array of well-formed
/// findings is an error — the caller turns it into a producer
/// failure, never a silent empty result.
fn parse_findings(text: &str) -> Result<Vec<Finding>, String> {
    let findings: Vec<Finding> = serde_json::from_str(text.trim())
        .map_err(|error| format!("session output is not a valid findings array: {error}"))?;
    for finding in &findings {
        if finding.line_start < 1 || finding.line_end < finding.line_start {
            return Err(format!(
                "finding '{}' has an invalid line range {}-{}",
                finding.title, finding.line_start, finding.line_end
            ));
        }
        for (label, value) in [
            ("file", &finding.file),
            ("category", &finding.category),
            ("title", &finding.title),
            ("description", &finding.description),
            ("evidence", &finding.evidence),
            ("recommended_fix", &finding.recommended_fix),
        ] {
            if value.trim().is_empty() {
                return Err(format!("finding '{}' has an empty {label}", finding.title));
            }
        }
        if let Some(citations) = &finding.citations {
            for citation in citations {
                let Some((path, line)) = citation.rsplit_once(':') else {
                    return Err(format!(
                        "finding '{}' has a malformed citation '{citation}' (want path:line)",
                        finding.title
                    ));
                };
                if path.is_empty() || line.parse::<u64>().is_err() {
                    return Err(format!(
                        "finding '{}' has a malformed citation '{citation}' (want path:line)",
                        finding.title
                    ));
                }
            }
        }
    }
    Ok(findings)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::fs;
    use std::path::PathBuf;
    use std::process::Command;
    use std::sync::Mutex;

    use agent_core::turn_io::{FinishReason, ModelRequest, ModelResponse, RawToolCall};
    use chomp::model_transport::TransportError;
    use chomp::tools::RepoToolExecutor;

    // -- scripted transport ---------------------------------------------------

    struct ScriptedTransport {
        script: Mutex<VecDeque<ModelResponse>>,
        fallback: ModelResponse,
        requests: Mutex<Vec<String>>,
    }

    impl ScriptedTransport {
        fn new(script: Vec<ModelResponse>, fallback: ModelResponse) -> Self {
            Self {
                script: Mutex::new(script.into()),
                fallback,
                requests: Mutex::new(Vec::new()),
            }
        }

        fn request_text(&self, index: usize) -> String {
            self.requests
                .lock()
                .expect("requests lock")
                .get(index)
                .cloned()
                .expect("request recorded")
        }
    }

    impl ModelTransport for ScriptedTransport {
        fn complete_model_turn(
            &self,
            request: &ModelRequest,
        ) -> Result<ModelResponse, TransportError> {
            let joined = request
                .session_messages
                .iter()
                .map(|message| message.content.clone())
                .collect::<Vec<_>>()
                .join("\n");
            self.requests.lock().expect("requests lock").push(joined);
            Ok(self
                .script
                .lock()
                .expect("script lock")
                .pop_front()
                .unwrap_or_else(|| self.fallback.clone()))
        }
    }

    fn text_response(text: &str) -> ModelResponse {
        ModelResponse {
            assistant_text: Some(text.to_string()),
            tool_calls: Vec::new(),
            finish_reason: FinishReason::Stop,
            provider_reasoning_content: None,
        }
    }

    fn tool_call_response(name: &str, arguments_json: &str) -> ModelResponse {
        ModelResponse {
            assistant_text: None,
            tool_calls: vec![RawToolCall {
                id: Some("call-1".to_string()),
                name: name.to_string(),
                arguments_json: arguments_json.to_string(),
            }],
            finish_reason: FinishReason::ToolUse,
            provider_reasoning_content: None,
        }
    }

    // -- git fixture ------------------------------------------------------------

    struct RepoFixture {
        base: PathBuf,
        repo: PathBuf,
        diff_path: PathBuf,
    }

    impl RepoFixture {
        fn new(label: &str) -> Self {
            let nonce = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos();
            let base = std::env::temp_dir().join(format!(
                "chomp-frg19-{label}-{}-{nonce}",
                std::process::id()
            ));
            let repo = base.join("repo");
            fs::create_dir_all(repo.join("src")).expect("create fixture repo");
            let fixture = Self {
                diff_path: base.join("mr.diff"),
                base,
                repo,
            };
            fixture.git(&["init", "-q"]);
            fixture
        }

        fn git(&self, args: &[&str]) {
            let status = Command::new("git")
                .arg("-C")
                .arg(&self.repo)
                .args([
                    "-c",
                    "user.email=fixture@example.com",
                    "-c",
                    "user.name=Fixture",
                ])
                .args(args)
                .status()
                .expect("git runs");
            assert!(status.success(), "git {args:?} failed");
        }

        fn write(&self, rel: &str, contents: &str) {
            let path = self.repo.join(rel);
            fs::create_dir_all(path.parent().expect("parent")).expect("dirs");
            fs::write(&path, contents).expect("write fixture file");
        }

        fn commit_all(&self, message: &str) {
            self.git(&["add", "-A"]);
            self.git(&["commit", "-q", "-m", message]);
        }

        fn write_diff(&self) {
            let output = Command::new("git")
                .arg("-C")
                .arg(&self.repo)
                .args(["diff", "--no-color"])
                .output()
                .expect("git diff runs");
            assert!(output.status.success());
            fs::write(&self.diff_path, &output.stdout).expect("write diff");
        }

        fn snapshot(&self) -> Snapshot {
            Snapshot::capture(&self.repo, &self.diff_path).expect("capture snapshot")
        }

        fn diff_text(&self) -> String {
            fs::read_to_string(&self.diff_path).expect("read diff")
        }
    }

    impl Drop for RepoFixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.base);
        }
    }

    fn executor_for(snapshot: &Snapshot) -> Arc<dyn ToolExecutor + Send + Sync> {
        Arc::new(
            RepoToolExecutor::new(snapshot.root.clone(), snapshot.head_sha.clone())
                .expect("executor"),
        )
    }

    /// The class-1 proof fixture: `parse_port` is made strict in the
    /// diff — correct for numeric input — while the existing caller
    /// in `src/main.rs` (not in the diff) passes
    /// `env::var("PORT").unwrap_or_default()`, the empty string when
    /// `PORT` is unset. The entry file carries a secret-shaped line
    /// the opening context must show only redacted.
    fn class1_fixture() -> RepoFixture {
        let fixture = RepoFixture::new("class1");
        fixture.write(
            "AGENTS.md",
            "# Fixture repo notes\n\nLocal note: API_KEY = \"supersecretvalue123\" for the dev box.\n",
        );
        fixture.write(
            "src/main.rs",
            "mod parse;\n\nfn main() {\n    let raw = std::env::var(\"PORT\").unwrap_or_default();\n    let port = parse::parse_port(&raw);\n    println!(\"listening on {port}\");\n}\n",
        );
        fixture.write(
            "src/parse.rs",
            "// parse helpers for the server bootstrap\n// (kept dependency-free on purpose)\n\npub fn parse_port(raw: &str) -> u16 {\n    raw.parse().unwrap_or_default()\n}\n",
        );
        fixture.commit_all("base");
        fixture.write(
            "src/parse.rs",
            "// parse helpers for the server bootstrap\n// (kept dependency-free on purpose)\n\npub fn parse_port(raw: &str) -> u16 {\n    raw.parse().expect(\"PORT must be numeric\")\n}\n",
        );
        fixture.write_diff();
        fixture
    }

    const CLASS1_FINDING: &str = r#"[{
        "file": "src/parse.rs",
        "line_start": 5,
        "line_end": 5,
        "severity": "High",
        "category": "Correctness",
        "title": "parse_port panics on non-numeric input",
        "description": "The strict parse panics on any non-numeric input. The existing caller passes an environment value that is empty whenever PORT is unset, so the process crashes at startup in exactly that configuration.",
        "evidence": "src/main.rs line 4 reads PORT with unwrap_or_default, and line 5 passes it to parse_port; the new expect at src/parse.rs line 5 panics on the empty string.",
        "fix_focus_areas": ["src/parse.rs[5-5]"],
        "recommended_fix": "Keep the lenient contract at this boundary: return Option or parse with a documented default, or validate PORT at startup with a clear error.",
        "citations": ["src/main.rs:5"]
    }]"#;

    #[test]
    fn class1_fixture_pulls_and_cites_the_caller() {
        let fixture = class1_fixture();
        let snapshot = fixture.snapshot();
        let executor = executor_for(&snapshot);
        let transport = ScriptedTransport::new(
            vec![
                tool_call_response("read_file", "{\"path\": \"src/main.rs\"}"),
                text_response(CLASS1_FINDING),
            ],
            text_response("[]"),
        );

        let outcome = run_correctness_session(
            &snapshot,
            &fixture.diff_text(),
            "AGENTS.md",
            &transport,
            &executor,
            AgentConfig::default(),
        );

        let LensOutcome::Completed(findings) = outcome else {
            panic!("expected completed findings, got {outcome:?}");
        };
        assert_eq!(findings.len(), 1);
        assert_eq!(
            findings[0].citations.as_deref(),
            Some(&["src/main.rs:5".to_string()][..])
        );

        // The opening context carried the full changed file (line 1
        // is outside the diff hunk), the repo map, and the entry —
        // redacted, because the reads went through the executor.
        let opening = transport.request_text(0);
        assert!(opening.contains("parse helpers for the server bootstrap"));
        assert!(opening.contains("src/main.rs"));
        assert!(opening.contains("<redacted>"), "entry secret redacted");
        assert!(!opening.contains("supersecretvalue123"));
        assert!(!opening.contains("{file_contents}"));
        // The caller file's contents were NOT in the opening context
        // (its only appearance anywhere up front would be the diff,
        // which does not touch it): the session had to pull them.
        assert!(!opening.contains("env::var"));

        // The second model turn saw the executor's tool result for
        // the caller pull.
        let second = transport.request_text(1);
        assert!(second.contains("env::var"));
    }

    #[test]
    fn opening_context_pages_beyond_the_first_window() {
        let fixture = RepoFixture::new("paging");
        fixture.write("AGENTS.md", "# Fixture repo notes\n");
        let mut big = String::new();
        for line in 1..=250 {
            if line == 240 {
                big.push_str("// MARKER_DEEP_LINE\n");
            } else {
                big.push_str(&format!("// filler line {line}\n"));
            }
        }
        fixture.write("src/big.rs", &big);
        fixture.commit_all("base");
        fixture.write(
            "src/big.rs",
            &big.replace("// filler line 2\n", "// filler line 2 changed\n"),
        );
        fixture.write_diff();
        let snapshot = fixture.snapshot();
        let executor = executor_for(&snapshot);
        let transport = ScriptedTransport::new(vec![text_response("[]")], text_response("[]"));

        let outcome = run_correctness_session(
            &snapshot,
            &fixture.diff_text(),
            "AGENTS.md",
            &transport,
            &executor,
            AgentConfig::default(),
        );

        assert_eq!(outcome, LensOutcome::Completed(Vec::new()));
        let opening = transport.request_text(0);
        assert!(
            opening.contains("MARKER_DEEP_LINE"),
            "full contents reach past the first read window"
        );
    }

    #[test]
    fn clean_diff_returns_empty_findings() {
        let fixture = class1_fixture();
        let snapshot = fixture.snapshot();
        let executor = executor_for(&snapshot);
        let transport = ScriptedTransport::new(vec![text_response("[]")], text_response("[]"));

        let outcome = run_correctness_session(
            &snapshot,
            &fixture.diff_text(),
            "AGENTS.md",
            &transport,
            &executor,
            AgentConfig::default(),
        );

        assert_eq!(outcome, LensOutcome::Completed(Vec::new()));
    }

    #[test]
    fn prose_output_is_a_producer_failure() {
        let fixture = class1_fixture();
        let snapshot = fixture.snapshot();
        let executor = executor_for(&snapshot);
        let transport = ScriptedTransport::new(
            vec![text_response("The change looks correct to me.")],
            text_response("[]"),
        );

        let outcome = run_correctness_session(
            &snapshot,
            &fixture.diff_text(),
            "AGENTS.md",
            &transport,
            &executor,
            AgentConfig::default(),
        );

        assert!(
            matches!(outcome, LensOutcome::ProducerFailure { .. }),
            "{outcome:?}"
        );
    }

    #[test]
    fn tiny_turn_budget_is_a_budget_outcome() {
        let fixture = class1_fixture();
        let snapshot = fixture.snapshot();
        let executor = executor_for(&snapshot);
        // The fallback answers every turn with another tool call; the
        // engine's turn budget must end the session as a recorded
        // budget outcome, not a hang.
        let transport = ScriptedTransport::new(
            Vec::new(),
            tool_call_response("read_file", "{\"path\": \"src/main.rs\"}"),
        );

        let outcome = run_correctness_session(
            &snapshot,
            &fixture.diff_text(),
            "AGENTS.md",
            &transport,
            &executor,
            AgentConfig {
                max_turns: 1,
                ..AgentConfig::default()
            },
        );

        assert!(
            matches!(outcome, LensOutcome::BudgetExhausted { .. }),
            "{outcome:?}"
        );
    }

    #[test]
    fn findings_validation_rejects_bad_shapes() {
        // Unknown severity value.
        let bad = CLASS1_FINDING.replace("\"High\"", "\"Blocker\"");
        assert!(parse_findings(&bad).is_err());
        // Malformed citation.
        let bad = CLASS1_FINDING.replace("src/main.rs:5", "src/main.rs");
        assert!(parse_findings(&bad).is_err());
        // Inverted line range.
        let bad = CLASS1_FINDING.replace("\"line_start\": 5", "\"line_start\": 9");
        assert!(parse_findings(&bad).is_err());
        // The well-formed fixture finding parses, citations intact.
        let findings = parse_findings(CLASS1_FINDING).expect("valid findings");
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].file, "src/parse.rs");
    }

    #[test]
    fn system_prompt_strips_the_provenance_comment() {
        let prompt = system_prompt();
        assert!(!prompt.contains("Provenance"));
        assert!(prompt.contains("correctness"));
    }
}
