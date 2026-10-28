use std::fs;
use std::path::PathBuf;
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
        fs::create_dir_all(&repo).expect("create fixture repo");
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
        fs::write(&self.diff, "diff --git a/src/lib.rs b/src/lib.rs\n")
            .expect("write fixture diff");
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

    let output = run(&fixture.review_args());
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
