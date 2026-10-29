//! FRG-54 live smoke: drive the real `chomp` binary through one
//! full correctness review against the real OpenRouter endpoint.
//!
//! Ignored by default, and never part of the PR gate: this test
//! makes a real, billed API call, and a live model is
//! nondeterministic. It asserts mechanics only — the process exits
//! 0 and the outputs have the contract shapes — and never anything
//! about finding content. A failure here means the live path broke
//! (provider drift, auth, response shape), which is exactly what
//! the nightly run exists to catch.
//!
//! Run it with:
//!
//! ```sh
//! OPENROUTER_API_KEY=... cargo test --locked --test live_smoke -- --ignored
//! ```
//!
//! `FORGE_MODEL` and `OPENROUTER_BASE_URL` are deliberately removed
//! from the child's environment: the subject under test is the
//! production configuration — the default model against the real
//! endpoint.

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
    fn new() -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock after epoch")
            .as_nanos();
        let root =
            std::env::temp_dir().join(format!("chomp-live-smoke-{}-{nonce}", std::process::id()));
        let repo = root.join("repo");
        fs::create_dir_all(repo.join("src")).expect("create fixture repo");
        let fixture = Self {
            diff: root.join("mr.diff"),
            findings: root.join("findings.json"),
            decisions: root.join("decisions.json"),
            pr_agent: root.join("pr-agent.md"),
            repo,
            root,
        };
        // The class-1 shape (a strict callee whose existing caller
        // passes the one breaking value) — the fixture carries a
        // real defect for the session to find, though the test
        // asserts nothing about whether it does.
        fixture.write(
            "src/main.rs",
            "mod parse;\n\nfn main() {\n    let raw = std::env::var(\"PORT\").unwrap_or_default();\n    let port = parse::parse_port(&raw);\n    println!(\"listening on {port}\");\n}\n",
        );
        fixture.write(
            "src/parse.rs",
            "pub fn parse_port(raw: &str) -> u16 {\n    raw.parse().unwrap_or_default()\n}\n",
        );
        fixture.git(&["init", "-q"]);
        fixture.git(&["add", "-A"]);
        fixture.git(&["commit", "-q", "-m", "base"]);
        fixture.write(
            "src/parse.rs",
            "pub fn parse_port(raw: &str) -> u16 {\n    raw.parse().expect(\"PORT must be numeric\")\n}\n",
        );
        let output = Command::new("git")
            .arg("-C")
            .arg(&fixture.repo)
            .args(["diff", "--no-color"])
            .output()
            .expect("git diff runs");
        assert!(output.status.success());
        fs::write(&fixture.diff, &output.stdout).expect("write diff");
        fixture
    }

    fn write(&self, rel: &str, contents: &str) {
        fs::write(self.repo.join(rel), contents).expect("write fixture file");
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

#[test]
#[ignore = "live OpenRouter smoke (FRG-54): run with --ignored and OPENROUTER_API_KEY set"]
fn live_review_runs_end_to_end() {
    let api_key = std::env::var("OPENROUTER_API_KEY")
        .ok()
        .filter(|key| !key.trim().is_empty())
        .expect("OPENROUTER_API_KEY must be set to run the live smoke");
    let fixture = Fixture::new();

    let output: Output = Command::new(env!("CARGO_BIN_EXE_chomp"))
        .args(fixture.review_args())
        .env("OPENROUTER_API_KEY", api_key)
        .env_remove("OPENROUTER_BASE_URL")
        .env_remove("FORGE_MODEL")
        .output()
        .expect("run chomp binary");

    assert!(
        output.status.success(),
        "live review failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let findings: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&fixture.findings).expect("findings written"))
            .expect("findings are JSON");
    let items = findings.as_array().expect("findings are a JSON array");
    println!("live smoke: session completed, {} finding(s)", items.len());
    let decisions: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&fixture.decisions).expect("decisions written"))
            .expect("decisions are JSON");
    assert_eq!(decisions["version"], 1);
    assert!(fixture.pr_agent.exists(), "PR-Agent guide written");
}
