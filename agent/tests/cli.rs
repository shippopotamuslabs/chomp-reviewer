use std::fs;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{SystemTime, UNIX_EPOCH};

struct Fixture {
    root: PathBuf,
    repo: PathBuf,
    diff: PathBuf,
    findings: PathBuf,
    decisions: PathBuf,
    pr_agent: PathBuf,
}

impl Fixture {
    fn new(label: &str) -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock after epoch")
            .as_nanos();
        let root =
            std::env::temp_dir().join(format!("chomp-cli-{label}-{}-{nonce}", std::process::id()));
        let repo = root.join("repo");
        fs::create_dir_all(repo.join("src")).expect("create fixture repo");
        fs::write(repo.join("src/lib.rs"), "pub fn answer() -> u32 { 41 }\n")
            .expect("write fixture source");
        // The FRG-19 review path captures a snapshot, which needs a
        // real git checkout with a HEAD commit.
        for args in [
            vec!["init", "-q", "-b", "main"],
            vec!["add", "-A"],
            vec!["commit", "-q", "-m", "base"],
        ] {
            let status = Command::new("git")
                .arg("-C")
                .arg(&repo)
                .args([
                    "-c",
                    "user.email=fixture@example.com",
                    "-c",
                    "user.name=Fixture",
                ])
                .args(&args)
                .status()
                .expect("git runs");
            assert!(status.success(), "git {args:?} failed");
        }
        Self {
            diff: root.join("mr.diff"),
            findings: root.join("findings.json"),
            decisions: root.join("decisions.json"),
            pr_agent: root.join("pr-agent.md"),
            repo,
            root,
        }
    }

    /// Commit an extra file on top of the base commit, so it exists
    /// on the base branch (`main`) the review resolves against.
    fn commit_extra(&self, rel: &str, contents: &str) {
        fs::write(self.repo.join(rel), contents).expect("write extra file");
        for args in [vec!["add", "-A"], vec!["commit", "-q", "-m", "extra"]] {
            let status = Command::new("git")
                .arg("-C")
                .arg(&self.repo)
                .args([
                    "-c",
                    "user.email=fixture@example.com",
                    "-c",
                    "user.name=Fixture",
                ])
                .args(&args)
                .status()
                .expect("git runs");
            assert!(status.success(), "git {args:?} failed");
        }
    }

    fn write_diff(&self) {
        fs::write(
            self.repo.join("src/lib.rs"),
            "pub fn answer() -> u32 { 42 }\n",
        )
        .expect("modify fixture source");
        let output = Command::new("git")
            .arg("-C")
            .arg(&self.repo)
            .args(["diff", "--no-color"])
            .output()
            .expect("git diff runs");
        assert!(output.status.success());
        fs::write(&self.diff, &output.stdout).expect("write fixture diff");
    }

    fn review_args(&self) -> Vec<String> {
        vec![
            "review".to_string(),
            "--repo".to_string(),
            self.repo.display().to_string(),
            "--diff".to_string(),
            self.diff.display().to_string(),
            "--base-ref".to_string(),
            "main".to_string(),
            "--pr-url".to_string(),
            "https://github.com/example/project/pull/1".to_string(),
            "--out-findings".to_string(),
            self.findings.display().to_string(),
            "--out-decisions".to_string(),
            self.decisions.display().to_string(),
            "--out-pr-agent".to_string(),
            self.pr_agent.display().to_string(),
        ]
    }

    fn run_with_diff_path(&self, diff: &Path) -> Output {
        let mut args = self.review_args();
        let position = args
            .iter()
            .position(|arg| arg == "--diff")
            .expect("diff flag present");
        args[position + 1] = diff.display().to_string();
        run(&args)
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn run(args: &[String]) -> Output {
    // PR-Agent fails closed before spawn without a GitHub token: the
    // tests must never depend on the developer's real environment, so
    // the token is always stripped (a recorded producer_error outage
    // is the deterministic expectation).
    Command::new(env!("CARGO_BIN_EXE_chomp"))
        .args(args)
        .env_remove("GITHUB__USER_TOKEN")
        .output()
        .expect("run chomp binary")
}

fn run_with_env(args: &[String], envs: &[(&str, &str)]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_chomp"))
        .args(args)
        .envs(envs.iter().copied())
        .env_remove("GITHUB__USER_TOKEN")
        .output()
        .expect("run chomp binary")
}

/// Read one framed HTTP request (headers + Content-Length body)
/// from a stub connection.
fn read_stub_request(stream: &mut std::net::TcpStream) -> Vec<u8> {
    let mut raw = Vec::new();
    let mut chunk = [0u8; 4096];
    let mut expected: Option<usize> = None;
    loop {
        match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                raw.extend_from_slice(&chunk[..n]);
                if expected.is_none() {
                    if let Some(pos) = raw.windows(4).position(|window| window == b"\r\n\r\n") {
                        let headers = String::from_utf8_lossy(&raw[..pos]).to_lowercase();
                        let length = headers
                            .lines()
                            .find_map(|line| line.strip_prefix("content-length:"))
                            .and_then(|value| value.trim().parse::<usize>().ok())
                            .unwrap_or(0);
                        expected = Some(pos + 4 + length);
                    }
                }
                if expected.is_some_and(|total| raw.len() >= total) {
                    break;
                }
            }
            Err(_) => break,
        }
    }
    raw
}

fn serve_stub(listener: TcpListener, route: impl Fn(&[u8]) -> String + Send + 'static) {
    std::thread::spawn(move || {
        // The fan-out drives three lenses concurrently, and a lens
        // may call the model more than once: serve every connection
        // on its own thread until the test process exits.
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else {
                continue;
            };
            let body = route(&read_stub_request(&mut stream));
            std::thread::spawn(move || {
                let response = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len(),
                );
                let _ = stream.write_all(response.as_bytes());
                let _ = stream.flush();
            });
        }
    });
}

/// A stub for the OpenRouter chat-completions endpoint: answers every
/// request with the given body. Returns the base URL to point
/// `OPENROUTER_BASE_URL` at.
fn stub_model_server(response_body: &str) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind stub server");
    let addr = listener.local_addr().expect("stub server addr");
    let body = response_body.to_string();
    serve_stub(listener, move |_| body.clone());
    format!("http://{addr}")
}

/// A per-producer stub: routes on the lens markers in the system
/// prompt (`Lens A` correctness, `Lens B` structure, `Lens C`
/// contract) plus the synthesizer session (FRG-26, `# Synthesizer`
/// marker), so concurrent producers deterministically receive their
/// own script even though their requests overlap in time. Every
/// stubbed review must script synthesis: an empty body means the
/// synthesizer receives the lens body (garbage-in tests only).
fn stub_model_server_with_synthesis(
    correctness: &str,
    structure: &str,
    contract: &str,
    synthesis: &str,
) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind stub server");
    let addr = listener.local_addr().expect("stub server addr");
    let (correctness, structure, contract, synthesis) = (
        correctness.to_string(),
        structure.to_string(),
        contract.to_string(),
        synthesis.to_string(),
    );
    serve_stub(listener, move |request| {
        let text = String::from_utf8_lossy(request);
        if text.contains("Synthesizer") && !synthesis.is_empty() {
            synthesis.clone()
        } else if text.contains("Lens C") {
            contract.clone()
        } else if text.contains("Lens B") {
            structure.clone()
        } else {
            correctness.clone()
        }
    });
    format!("http://{addr}")
}

/// One valid envelope finding tagged per lens, so a cross-lens
/// interleave would show up as the wrong file or title on a stage.
fn lens_finding(tag: &str) -> serde_json::Value {
    serde_json::json!({
        "file": format!("src/{tag}.rs"),
        "line_start": 5,
        "line_end": 5,
        "severity": "High",
        "category": "Review",
        "title": format!("{tag} lens finding"),
        "description": format!("The {tag} lens reports an issue in the changed code."),
        "evidence": "src/lib.rs line 1 changed in the diff.",
        "fix_focus_areas": [format!("src/{tag}.rs[5-5]")],
        "recommended_fix": format!("Address the {tag} concern before merging."),
        "citations": ["src/lib.rs:1"],
    })
}

/// A chat-completions body whose assistant text is the synthesis
/// envelope (FRG-26): summary, final findings, and one decision entry
/// per producer.
fn chat_body_for_synthesis(
    summary: &str,
    findings: &[serde_json::Value],
    decisions: &[serde_json::Value],
) -> String {
    let content = serde_json::to_string(&serde_json::json!({
        "summary": summary,
        "findings": findings,
        "decisions": decisions,
    }))
    .expect("synthesis envelope serializes");
    serde_json::to_string(&serde_json::json!({
        "choices": [{"message": {"role": "assistant", "content": content}, "finish_reason": "stop"}]
    }))
    .expect("chat body serializes")
}

/// One synthesis decision entry: producer N, raw disposition string,
/// and the kept/merged target when the disposition carries one.
fn synthesis_decision(
    producer: usize,
    disposition: &str,
    final_index: Option<usize>,
) -> serde_json::Value {
    let mut entry = serde_json::Map::new();
    entry.insert("producer".to_string(), serde_json::json!(producer));
    entry.insert("disposition".to_string(), serde_json::json!(disposition));
    if let Some(final_index) = final_index {
        entry.insert("final".to_string(), serde_json::json!(final_index));
    }
    serde_json::Value::Object(entry)
}

/// A chat-completions body whose assistant text is the given findings
/// envelope.
fn chat_body_for(findings: &[serde_json::Value], note: Option<&str>) -> String {
    let mut envelope = serde_json::Map::new();
    envelope.insert(
        "findings".to_string(),
        serde_json::Value::Array(findings.to_vec()),
    );
    if let Some(note) = note {
        envelope.insert(
            "note".to_string(),
            serde_json::Value::String(note.to_string()),
        );
    }
    let content =
        serde_json::to_string(&serde_json::Value::Object(envelope)).expect("envelope serializes");
    serde_json::to_string(&serde_json::json!({
        "choices": [{"message": {"role": "assistant", "content": content}, "finish_reason": "stop"}]
    }))
    .expect("chat body serializes")
}

#[test]
fn help_documents_the_full_cli_contract() {
    let output = run(&["--help".to_string()]);
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).expect("help is UTF-8");
    for expected in [
        "chomp review",
        "--repo",
        "--diff",
        "--base-ref",
        "--pr-url",
        "--config",
        "--ticket-file",
        "--out-findings",
        "--out-decisions",
        "--out-pr-agent",
        "OPENROUTER_API_KEY",
        "FORGE_MODEL",
        "Exit codes:",
    ] {
        assert!(
            stdout.contains(expected),
            "help is missing {expected}:\n{stdout}"
        );
    }
}

#[test]
fn fixture_review_exits_zero_and_writes_all_outputs() {
    let fixture = Fixture::new("success");
    fixture.write_diff();

    // Every producer runs end to end against a stub model endpoint
    // that answers with empty findings; the synthesizer answers the
    // clean bill.
    let empty = chat_body_for(&[], None);
    let clean_bill = chat_body_for_synthesis("No findings.", &[], &[]);
    let base_url = stub_model_server_with_synthesis(&empty, &empty, &empty, &clean_bill);
    let output = run_with_env(
        &fixture.review_args(),
        &[
            ("OPENROUTER_API_KEY", "test-key"),
            ("OPENROUTER_BASE_URL", &base_url),
        ],
    );
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        fs::read_to_string(&fixture.findings).expect("findings written"),
        "[]\n"
    );
    let decisions: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&fixture.decisions).expect("decisions written"))
            .expect("decisions are JSON");
    assert_eq!(decisions["version"], 1);
    // FRG-20: this fixture has no entry file on the base branch, so
    // the run records the contract stand-down and still succeeds —
    // absence degrades the review, it never fails it.
    assert_eq!(decisions["entries"][0]["kind"], "contract");
    assert_eq!(decisions["entries"][0]["event"], "entry_absent");
    // The synthesizer completed over zero producers and recorded the
    // completion plus the rollup summary.
    let synthesis: Vec<&serde_json::Value> = decisions["entries"]
        .as_array()
        .expect("entries is an array")
        .iter()
        .filter(|entry| entry["kind"] == "synthesis")
        .collect();
    assert_eq!(synthesis.len(), 2, "{decisions}");
    assert_eq!(synthesis[0]["event"], "completed");
    assert_eq!(synthesis[1]["event"], "summary");
    assert_eq!(
        fs::read(&fixture.pr_agent).expect("pr-agent guide written"),
        Vec::<u8>::new()
    );
}

#[test]
fn oversized_entry_file_records_its_truncation() {
    let fixture = Fixture::new("truncated-entry");
    let mut agents = String::new();
    for line in 1..=10 {
        agents.push_str(&format!("rule line {line}\n"));
    }
    fixture.commit_extra("AGENTS.md", &agents);
    fixture.commit_extra(".pr_agent.toml", "[forge]\nentry_max_lines = 3\n");
    fixture.write_diff();

    let empty = chat_body_for(&[], None);
    let clean_bill = chat_body_for_synthesis("No findings.", &[], &[]);
    let base_url = stub_model_server_with_synthesis(&empty, &empty, &empty, &clean_bill);
    let output = run_with_env(
        &fixture.review_args(),
        &[
            ("OPENROUTER_API_KEY", "test-key"),
            ("OPENROUTER_BASE_URL", &base_url),
        ],
    );
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let decisions: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&fixture.decisions).expect("decisions written"))
            .expect("decisions are JSON");
    assert_eq!(decisions["entries"][0]["kind"], "contract");
    assert_eq!(decisions["entries"][0]["event"], "entry_truncated");
    let detail = decisions["entries"][0]["detail"]
        .as_str()
        .expect("detail is text");
    assert!(detail.contains("first 3 of 10 lines"), "{detail}");
}

#[test]
fn directory_as_diff_exits_two_and_writes_no_outputs() {
    let fixture = Fixture::new("directory-diff");
    // The diff path points at the repo directory itself: a directory is
    // openable on Linux but is not a readable diff file.
    let output = fixture.run_with_diff_path(&fixture.repo);
    assert_eq!(output.status.code(), Some(2));
    assert!(!fixture.findings.exists());
    assert!(!fixture.decisions.exists());
    assert!(!fixture.pr_agent.exists());
}

#[test]
fn missing_diff_exits_two_and_writes_no_outputs() {
    let fixture = Fixture::new("missing-diff");

    let output = run(&fixture.review_args());
    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("cannot read diff file"), "{stderr}");
    assert!(!fixture.findings.exists());
    assert!(!fixture.decisions.exists());
    assert!(!fixture.pr_agent.exists());
}

#[test]
fn missing_required_flag_exits_two() {
    let output = run(&[
        "review".to_string(),
        "--repo".to_string(),
        "/tmp".to_string(),
    ]);
    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("missing required argument `--diff`"),
        "{stderr}"
    );
}

/// Read the decision log the run wrote.
fn read_decisions(fixture: &Fixture) -> serde_json::Value {
    serde_json::from_str(&fs::read_to_string(&fixture.decisions).expect("decisions written"))
        .expect("decisions are JSON")
}

/// Find the single stage entry for a stage.
fn stage_entry<'a>(decisions: &'a serde_json::Value, stage: &str) -> &'a serde_json::Value {
    let matches: Vec<&serde_json::Value> = decisions["entries"]
        .as_array()
        .expect("entries is an array")
        .iter()
        .filter(|entry| entry["kind"] == "stage" && entry["stage"] == stage)
        .collect();
    assert_eq!(matches.len(), 1, "one {stage} stage entry: {decisions}");
    matches[0]
}

/// A per-lens findings path next to the combined `--out-findings`.
fn lens_path(fixture: &Fixture, stage: &str) -> PathBuf {
    let name = fixture
        .findings
        .file_stem()
        .expect("findings has a stem")
        .to_os_string();
    let mut lens_name = name;
    lens_name.push(format!(".{stage}.json"));
    fixture.findings.with_file_name(lens_name)
}

#[test]
fn fan_out_success_writes_combined_and_per_lens_findings() {
    let fixture = Fixture::new("fanout-success");
    // A clean governing entry: no contract event, all three lenses run.
    fixture.commit_extra("AGENTS.md", "# Fixture rules\n\nAlways handle errors.\n");
    fixture.write_diff();

    let base_url = stub_model_server_with_synthesis(
        &chat_body_for(&[lens_finding("correctness")], None),
        &chat_body_for(&[lens_finding("structure")], None),
        &chat_body_for(&[lens_finding("contract")], None),
        &chat_body_for_synthesis(
            "One merged finding.",
            &[lens_finding("merged")],
            &[
                synthesis_decision(1, "kept", Some(1)),
                synthesis_decision(2, "merged-into:1", None),
                synthesis_decision(
                    3,
                    "dropped:false-positive; already addressed on the base branch",
                    None,
                ),
            ],
        ),
    );
    let output = run_with_env(
        &fixture.review_args(),
        &[
            ("OPENROUTER_API_KEY", "test-key"),
            ("OPENROUTER_BASE_URL", &base_url),
        ],
    );
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // The combined file carries the synthesizer's final findings, not
    // the pre-dedup union: the stubbed session merged all three
    // producers into one.
    let combined: Vec<serde_json::Value> =
        serde_json::from_str(&fs::read_to_string(&fixture.findings).expect("findings written"))
            .expect("findings are JSON");
    assert_eq!(
        combined
            .iter()
            .map(|finding| finding["title"].as_str().expect("title is text"))
            .collect::<Vec<_>>(),
        vec!["merged lens finding"]
    );

    // Each completed lens keeps its own findings in its own file.
    for stage in ["correctness", "structure", "contract"] {
        let per_lens: Vec<serde_json::Value> = serde_json::from_str(
            &fs::read_to_string(lens_path(&fixture, stage)).expect("per-lens written"),
        )
        .expect("per-lens findings are JSON");
        assert_eq!(per_lens.len(), 1, "{stage} keeps one finding");
        assert_eq!(
            per_lens[0]["title"].as_str().expect("title is text"),
            format!("{stage} lens finding")
        );
    }

    // Decision log: run entry naming the engine choice, then four
    // stage entries — three completions and the PR-Agent outage —
    // then the synthesis completion, the rollup summary, and one
    // disposition per producer finding.
    let decisions = read_decisions(&fixture);
    assert_eq!(decisions["version"], 1);
    let entries = decisions["entries"]
        .as_array()
        .expect("entries is an array");
    assert_eq!(entries.len(), 10, "{decisions}");
    let dispositions: Vec<&serde_json::Value> = entries
        .iter()
        .filter(|entry| entry["kind"] == "synthesis_disposition")
        .collect();
    assert_eq!(dispositions.len(), 3, "{decisions}");
    assert_eq!(dispositions[0]["producer"], 1, "{decisions}");
    assert_eq!(dispositions[0]["disposition"], "kept", "{decisions}");
    assert_eq!(dispositions[0]["final"], 1, "{decisions}");
    assert_eq!(
        dispositions[1]["disposition"], "merged-into:1",
        "{decisions}"
    );
    assert_eq!(
        dispositions[2]["reason"], "false-positive; already addressed on the base branch",
        "{decisions}"
    );
    assert_eq!(entries[0]["kind"], "run");
    assert_eq!(entries[0]["event"], "fan_out");
    assert!(
        entries[0]["detail"]
            .as_str()
            .expect("detail is text")
            .contains("one AgentEngine per lens session"),
        "{}",
        entries[0]["detail"]
    );
    for (stage, expected_findings) in [("correctness", 1), ("structure", 1), ("contract", 1)] {
        let entry = stage_entry(&decisions, stage);
        assert_eq!(entry["status"], "completed", "{entry}");
        assert_eq!(entry["mode"], serde_json::Value::Null, "{entry}");
        assert_eq!(entry["findings"], expected_findings, "{entry}");
        assert_eq!(entry["note"], serde_json::Value::Null, "{entry}");
    }
    let pr_agent = stage_entry(&decisions, "pr_agent");
    assert_eq!(pr_agent["status"], "producer_outage", "{pr_agent}");
    assert_eq!(pr_agent["mode"], "producer_error", "{pr_agent}");
    assert_eq!(pr_agent["findings"], serde_json::Value::Null, "{pr_agent}");

    // PR-Agent failed closed before spawn: the guide exists and is empty.
    assert_eq!(
        fs::read(&fixture.pr_agent).expect("pr-agent guide written"),
        Vec::<u8>::new()
    );
}

#[test]
fn fan_out_all_lenses_garbage_exits_two_with_outage_records() {
    let fixture = Fixture::new("fanout-garbage");
    // Governing so every producer runs; the stub answers garbage for
    // every lens and the synthesizer, so all five producers fail.
    fixture.commit_extra("AGENTS.md", "# Fixture rules\n\nAlways handle errors.\n");
    fixture.write_diff();

    let garbage = serde_json::to_string(&serde_json::json!({
        "choices": [{"message": {"role": "assistant", "content": "this is plain prose, not a findings envelope"}, "finish_reason": "stop"}]
    }))
    .expect("garbage body serializes");
    let base_url = stub_model_server(&garbage);
    let output = run_with_env(
        &fixture.review_args(),
        &[
            ("OPENROUTER_API_KEY", "test-key"),
            ("OPENROUTER_BASE_URL", &base_url),
        ],
    );
    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&output.stderr);

    // The decision log still exists with five outage records: the
    // four fan-out producers plus the synthesizer, whose garbage
    // output is a visible outage, never a disguised empty review.
    let decisions = read_decisions(&fixture);
    let entries = decisions["entries"]
        .as_array()
        .expect("entries is an array");
    assert_eq!(entries.len(), 6, "{decisions}");
    let synthesis: Vec<&serde_json::Value> = entries
        .iter()
        .filter(|entry| entry["kind"] == "synthesis")
        .collect();
    assert_eq!(synthesis.len(), 1, "{decisions}");
    assert_eq!(synthesis[0]["event"], "outage", "{decisions}");
    for stage in ["correctness", "structure", "contract"] {
        let entry = stage_entry(&decisions, stage);
        assert_eq!(entry["status"], "producer_outage", "{entry}");
        assert_eq!(entry["mode"], "invalid_output", "{entry}");
    }
    let pr_agent = stage_entry(&decisions, "pr_agent");
    assert_eq!(pr_agent["status"], "producer_outage", "{pr_agent}");
    assert_eq!(pr_agent["mode"], "producer_error", "{pr_agent}");

    // stderr names every failed stage with its mode.
    for expected in [
        "correctness (invalid_output)",
        "structure (invalid_output)",
        "contract (invalid_output)",
        "pr_agent (producer_error)",
    ] {
        assert!(
            stderr.contains(expected),
            "stderr names {expected}:\n{stderr}"
        );
    }

    // Failed lenses never get a masquerading `[]` file; the combined
    // findings are empty and the PR-Agent guide is an empty file.
    for stage in ["correctness", "structure", "contract"] {
        assert!(
            !lens_path(&fixture, stage).exists(),
            "{stage} per-lens file must not exist"
        );
    }
    assert_eq!(
        fs::read_to_string(&fixture.findings).expect("findings written"),
        "[]\n"
    );
    assert_eq!(
        fs::read(&fixture.pr_agent).expect("pr-agent guide written"),
        Vec::<u8>::new()
    );
}

#[test]
fn fan_out_governing_but_uncheckable_contract_records_note() {
    let fixture = Fixture::new("fanout-note");
    fixture.commit_extra("AGENTS.md", "# Fixture rules\n\nNo checkable rules here.\n");
    fixture.write_diff();

    let note = "the governing copy states nothing checkable";
    let base_url = stub_model_server_with_synthesis(
        &chat_body_for(&[lens_finding("correctness")], None),
        &chat_body_for(&[lens_finding("structure")], None),
        &chat_body_for(&[], Some(note)),
        &chat_body_for_synthesis(
            "One merged finding.",
            &[lens_finding("merged")],
            &[
                synthesis_decision(1, "kept", Some(1)),
                synthesis_decision(2, "merged-into:1", None),
            ],
        ),
    );
    let output = run_with_env(
        &fixture.review_args(),
        &[
            ("OPENROUTER_API_KEY", "test-key"),
            ("OPENROUTER_BASE_URL", &base_url),
        ],
    );
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // The contract stage carries the stand-down note with no findings.
    let decisions = read_decisions(&fixture);
    let contract = stage_entry(&decisions, "contract");
    assert_eq!(contract["status"], "completed", "{contract}");
    assert_eq!(contract["findings"], 0, "{contract}");
    assert_eq!(contract["note"], note, "{contract}");

    // The combined file holds the synthesizer's merge of the two
    // checking lenses; the contract per-lens file is an honest empty
    // array.
    let combined: Vec<serde_json::Value> =
        serde_json::from_str(&fs::read_to_string(&fixture.findings).expect("findings written"))
            .expect("findings are JSON");
    assert_eq!(combined.len(), 1);
    assert_eq!(
        combined[0]["title"].as_str().expect("title is text"),
        "merged lens finding"
    );
    assert_eq!(
        fs::read_to_string(lens_path(&fixture, "contract")).expect("contract per-lens written"),
        "[]\n"
    );

    // A governing entry records no stand-down event anywhere.
    let absent = decisions["entries"]
        .as_array()
        .expect("entries is an array")
        .iter()
        .filter(|entry| entry["event"] == "entry_absent")
        .count();
    assert_eq!(absent, 0, "{decisions}");
}

#[test]
fn fan_out_absent_contract_records_a_single_stand_down() {
    let fixture = Fixture::new("fanout-absent");
    // No entry file on the base branch: the contract lens stands down.
    fixture.write_diff();

    let base_url = stub_model_server_with_synthesis(
        &chat_body_for(&[lens_finding("correctness")], None),
        &chat_body_for(&[lens_finding("structure")], None),
        &chat_body_for(&[], None),
        &chat_body_for_synthesis(
            "One merged finding.",
            &[lens_finding("merged")],
            &[
                synthesis_decision(1, "kept", Some(1)),
                synthesis_decision(2, "merged-into:1", None),
            ],
        ),
    );
    let output = run_with_env(
        &fixture.review_args(),
        &[
            ("OPENROUTER_API_KEY", "test-key"),
            ("OPENROUTER_BASE_URL", &base_url),
        ],
    );
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // Exactly one entry_absent event in the whole log, and the
    // contract stage note is null.
    let decisions = read_decisions(&fixture);
    let absent = decisions["entries"]
        .as_array()
        .expect("entries is an array")
        .iter()
        .filter(|entry| entry["event"] == "entry_absent")
        .count();
    assert_eq!(absent, 1, "{decisions}");
    let contract = stage_entry(&decisions, "contract");
    assert_eq!(contract["status"], "completed", "{contract}");
    assert_eq!(contract["findings"], 0, "{contract}");
    assert_eq!(contract["note"], serde_json::Value::Null, "{contract}");
}

#[test]
fn fan_out_deep_review_false_still_fans_out_with_record() {
    let fixture = Fixture::new("fanout-reduced");
    fs::write(
        fixture.repo.join(".pr_agent.toml"),
        "[forge]\ndeep_review = false\n",
    )
    .expect("write repo config");
    fixture.write_diff();

    let base_url = stub_model_server_with_synthesis(
        &chat_body_for(&[lens_finding("correctness")], None),
        &chat_body_for(&[lens_finding("structure")], None),
        &chat_body_for(&[lens_finding("contract")], None),
        &chat_body_for_synthesis(
            "One merged finding.",
            &[lens_finding("merged")],
            &[
                synthesis_decision(1, "kept", Some(1)),
                synthesis_decision(2, "merged-into:1", None),
            ],
        ),
    );
    let output = run_with_env(
        &fixture.review_args(),
        &[
            ("OPENROUTER_API_KEY", "test-key"),
            ("OPENROUTER_BASE_URL", &base_url),
        ],
    );
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // The full fan-out still ran (four stage entries) and the reduced
    // mode was recorded, not honored; then the synthesizer merged the
    // two checking lenses (completion, summary, two dispositions).
    let decisions = read_decisions(&fixture);
    let entries = decisions["entries"]
        .as_array()
        .expect("entries is an array");
    assert_eq!(entries.len(), 11, "{decisions}");
    assert_eq!(entries[0]["kind"], "contract");
    assert_eq!(entries[0]["event"], "entry_absent");
    assert_eq!(entries[1]["kind"], "config");
    assert_eq!(entries[1]["event"], "deep_review_ignored");
    assert!(
        entries[1]["detail"]
            .as_str()
            .expect("detail is text")
            .contains("deep_review = false"),
        "{}",
        entries[1]["detail"]
    );
    assert_eq!(entries[2]["kind"], "run");
    for stage in ["correctness", "structure", "contract", "pr_agent"] {
        stage_entry(&decisions, stage);
    }
}

#[test]
fn fan_out_zero_lens_timeout_is_a_config_error_before_any_producer() {
    let fixture = Fixture::new("fanout-zero-timeout");
    fs::write(
        fixture.repo.join(".pr_agent.toml"),
        "[forge]\nlens_timeout_secs = 0\n",
    )
    .expect("write repo config");
    fixture.write_diff();

    let output = run_with_env(
        &fixture.review_args(),
        &[
            ("OPENROUTER_API_KEY", "test-key"),
            ("OPENROUTER_BASE_URL", "http://127.0.0.1:1"),
        ],
    );
    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("lens_timeout_secs"), "{stderr}");
    // The config error lands before any producer runs: no artifacts.
    assert!(!fixture.findings.exists());
    assert!(!fixture.decisions.exists());
    assert!(!fixture.pr_agent.exists());
}
