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
            vec!["init", "-q"],
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
    Command::new(env!("CARGO_BIN_EXE_chomp"))
        .args(args)
        .output()
        .expect("run chomp binary")
}

fn run_with_env(args: &[String], envs: &[(&str, &str)]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_chomp"))
        .args(args)
        .envs(envs.iter().copied())
        .output()
        .expect("run chomp binary")
}

/// A one-shot stub for the OpenRouter chat-completions endpoint:
/// accepts a single connection, drains the request, and answers with
/// the given body. Returns the base URL to point
/// `OPENROUTER_BASE_URL` at.
fn stub_model_server(response_body: &str) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind stub server");
    let addr = listener.local_addr().expect("stub server addr");
    let body = response_body.to_string();
    std::thread::spawn(move || {
        let Ok((mut stream, _)) = listener.accept() else {
            return;
        };
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
        let response = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len(),
        );
        let _ = stream.write_all(response.as_bytes());
        let _ = stream.flush();
    });
    format!("http://{addr}")
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

    // The correctness session runs end to end against a stub model
    // endpoint that answers with an empty findings array.
    let base_url = stub_model_server(
        r#"{"choices":[{"message":{"role":"assistant","content":"[]"},"finish_reason":"stop"}]}"#,
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
    assert_eq!(
        fs::read_to_string(&fixture.findings).expect("findings written"),
        "[]\n"
    );
    let decisions: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&fixture.decisions).expect("decisions written"))
            .expect("decisions are JSON");
    assert_eq!(decisions["version"], 1);
    assert_eq!(decisions["entries"], serde_json::json!([]));
    assert_eq!(
        fs::read(&fixture.pr_agent).expect("pr-agent guide written"),
        Vec::<u8>::new()
    );
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
