//! PR-Agent subprocess adapter (FRG-23, Move 1, option C).
//!
//! The adapter runs the stock PR-Agent 0.49.0 CLI against the real PR
//! URL through the GitHub provider, with the rendered repo map injected
//! via `[artifacts]`, publishing disabled, and the structured review
//! captured silently through `--json-output` — the parsed `review`
//! data plus `usage`/`coverage` that later lands at `--out-pr-agent`.
//! The guide's markdown form has no silent channel on any version, so
//! the evidence here is JSON, not guide bytes (plan D1, vision gate
//! 2026-10-29).
//!
//! ## Environment contract (plan D5; FRG-28 provisions)
//!
//! | variable                 | meaning                                         |
//! |--------------------------|-------------------------------------------------|
//! | `FORGE_PR_AGENT_PYTHON`  | interpreter for `-m pr_agent.cli`; default `python3` (FRG-28 points it at the workflow venv) |
//! | `GITHUB__USER_TOKEN`     | GitHub provider credential, forwarded verbatim; absent/empty → `ProducerFailure` before spawn |
//! | `OPENROUTER_API_KEY`     | mapped to `OPENROUTER__KEY` for the subprocess; absent → `ProducerFailure` before spawn |
//! | `FORGE_MODEL`            | resolved like the transport resolves it, then given the `openrouter/` prefix PR-Agent's litellm routing requires when no provider prefix is present |
//!
//! Subprocess env set on top of the inherited environment:
//! `CONFIG__PUBLISH_OUTPUT=false`,
//! `CONFIG__PUBLISH_OUTPUT_PROGRESS=false`, `CONFIG__MODEL`,
//! `CONFIG__CUSTOM_MODEL_MAX_TOKENS=1000000` (1.x precedent),
//! `OPENROUTER__KEY`, `GITHUB__USER_TOKEN`,
//! `CONFIG__REPO_CONTEXT_FILES` (always explicit — even the empty
//! stand-down list, so upstream's `["AGENTS.md"]` default can never
//! resurrect itself; JSON-list encoding per the Chunk 0 probe),
//! `CONFIG__REPO_CONTEXT_MAX_LINES=<entry_max_lines>`,
//! `ARTIFACT_PATH=<run-dir>/repo-map.txt` (omitted when the map is
//! empty), and `GITHUB_WORKSPACE` pinned to the run dir (plan D6) so
//! `ARTIFACT_PATH` satisfies upstream's under-workspace rule.
//!
//! Every failure is a [`PrAgentOutcome::ProducerFailure`] value —
//! never an error, never a retry. Secrets never appear in `reason`
//! strings. Wiring into `main` (parallel fan-out, stage timeouts,
//! outage recording, `write_outputs`) lands in FRG-24; until then
//! this module is standalone-testable and its public items are
//! `#[allow(dead_code)]` by the same convention as
//! `run_structure_session`.

use std::fs;
use std::io::Read;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Model fallback shared with the transport: `resolve_model` there is
/// private to the lib target, so the `DEFAULT_MODEL` fallback rule is
/// replicated here (trim, strip the `openrouter/` prefix, fall back)
/// and the `openrouter/` routing prefix is then applied when the
/// resolved value carries no provider prefix.
use chomp::transport::DEFAULT_MODEL;

/// Request for one PR-Agent producer run.
#[derive(Debug, Clone)]
// Wired into main by FRG-24 fan-out; standalone-testable until then.
#[allow(dead_code)]
pub struct PrAgentRequest {
    /// The real PR URL, passed as `--pr_url`.
    pub pr_url: String,
    /// Context files for PR-Agent's static channel, from
    /// `ContractResolution::pr_agent_context_files` (empty on
    /// stand-down — still sent explicitly, never omitted).
    pub context_files: Vec<String>,
    /// `RepoMap::render()` output, injected via `[artifacts]`.
    pub repo_map: String,
    /// ForgeConfig cap, forwarded as `repo_context_max_lines`.
    pub entry_max_lines: usize,
    /// Caller-supplied budget; FRG-24 passes the stage budget.
    pub timeout: Duration,
}

/// The adapter outcome, mirroring the lens outcome discipline: a
/// producer failure is a value, not a `Result` error.
#[derive(Debug, Clone, PartialEq, Eq)]
// Wired into main by FRG-24 fan-out; standalone-testable until then.
#[allow(dead_code)]
pub enum PrAgentOutcome {
    /// PR-Agent exited 0 and its JSON held a `review` object; `json`
    /// is the file's exact bytes.
    Completed { json: String },
    /// Anything else: missing env (no spawn), spawn failure, nonzero
    /// exit, timeout kill, or an unreadable/invalid output file.
    ProducerFailure { reason: String },
}

/// Run one PR-Agent producer invocation against the real PR URL.
///
/// Reads the D5 environment itself; absent credentials fail before any
/// spawn. The run dir is removed on every path (cleanup failure is
/// not a producer failure).
// Wired into main by FRG-24 fan-out; standalone-testable until then.
#[allow(dead_code)]
pub fn run_pr_agent(request: &PrAgentRequest) -> PrAgentOutcome {
    run_pr_agent_with_env(request, &|name| std::env::var(name).ok())
}

/// The env-source seam the tests drive: the public entry point stays
/// env-based; tests pass an explicit lookup instead of mutating the
/// process environment for the build path.
fn run_pr_agent_with_env(
    request: &PrAgentRequest,
    lookup: &dyn Fn(&str) -> Option<String>,
) -> PrAgentOutcome {
    let invocation = match build_invocation(request, lookup) {
        Ok(invocation) => invocation,
        Err(reason) => return PrAgentOutcome::ProducerFailure { reason },
    };

    let run_dir = invocation.run_dir.clone();
    let outcome = run_invocation(&invocation, request.timeout);
    // Best-effort cleanup on all paths; failure to clean is not a
    // producer failure.
    let _ = fs::remove_dir_all(&run_dir);
    outcome
}

/// A fully-resolved invocation: what to spawn, with which argv, which
/// extra env pairs (on top of the inherited environment), and where
/// the outputs live. Pure data — tests assert the wire shape from
/// this without spawning a subprocess.
#[derive(Debug, Clone)]
struct Invocation {
    program: String,
    args: Vec<String>,
    /// Pairs set on the child on top of the inherited environment.
    env: Vec<(String, String)>,
    run_dir: PathBuf,
    review_json: PathBuf,
}

/// Cap on the stderr kept in a failure reason: enough to diagnose,
/// not enough to flood the decision log.
const MAX_STDERR_TAIL_CHARS: usize = 2_000;

/// Poll interval for `try_wait` while the child runs.
const POLL_INTERVAL: Duration = Duration::from_millis(10);

/// Build the invocation for `request`: create the per-run dir, write
/// `repo-map.txt` when the map is non-empty, and resolve the program,
/// argv, and env pairs. `Err` is a missing-credential reason — the
/// caller must not spawn.
fn build_invocation(
    request: &PrAgentRequest,
    lookup: &dyn Fn(&str) -> Option<String>,
) -> Result<Invocation, String> {
    let nonempty = |name: &str| {
        lookup(name)
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| {
                format!(
                    "missing required environment variable {name}: \
                     refusing to spawn a subprocess that cannot read the PR"
                )
            })
    };
    let github_token = nonempty("GITHUB__USER_TOKEN")?;
    let openrouter_key = nonempty("OPENROUTER_API_KEY")?;
    let python = lookup("FORGE_PR_AGENT_PYTHON")
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| "python3".to_string());
    let model = resolve_pr_agent_model(lookup("FORGE_MODEL"));

    // JSON-list encoding, exactly as the Chunk 0 probe verified
    // dynaconf parses it (`'["a", "b"]'` → both entries, `'[]'` →
    // empty). Serialization cannot fail for strings; a failure would
    // mean inventing an encoding, so it is an error, not a fallback.
    // Encoded BEFORE touching the filesystem so a serialization
    // failure cannot leak a created run dir (plan D6: cleanup on all
    // paths).
    let context_files = serde_json::to_string(&request.context_files)
        .map_err(|error| format!("cannot encode context files for pr-agent: {error}"))?;

    let run_dir =
        fresh_run_dir().map_err(|error| format!("cannot create pr-agent run dir: {error}"))?;
    if let Err(error) = fs::create_dir_all(&run_dir) {
        let _ = fs::remove_dir_all(&run_dir);
        return Err(format!(
            "cannot create pr-agent run dir {}: {error}",
            run_dir.display()
        ));
    }
    let review_json = run_dir.join("review.json");
    // The nonce dir is `pid + nanos`, so a same-nanosecond collision
    // with a crashed run's dir would silently reuse it via
    // `create_dir_all` — and a leftover `review.json` could then be
    // read back as this run's success if the child exits 0 without
    // writing. `review.json` must never predate the spawn.
    if review_json.exists() {
        if let Err(error) = fs::remove_file(&review_json) {
            let _ = fs::remove_dir_all(&run_dir);
            return Err(format!(
                "cannot clear stale pr-agent review JSON {}: {error}",
                review_json.display()
            ));
        }
    }

    let args = vec![
        "-m".to_string(),
        "pr_agent.cli".to_string(),
        format!("--pr_url={}", request.pr_url),
        format!("--json-output={}", review_json.display()),
        "review".to_string(),
    ];

    let mut env = vec![
        ("CONFIG__PUBLISH_OUTPUT".to_string(), "false".to_string()),
        (
            "CONFIG__PUBLISH_OUTPUT_PROGRESS".to_string(),
            "false".to_string(),
        ),
        ("CONFIG__MODEL".to_string(), model),
        (
            "CONFIG__CUSTOM_MODEL_MAX_TOKENS".to_string(),
            "1000000".to_string(),
        ),
        ("OPENROUTER__KEY".to_string(), openrouter_key),
        ("GITHUB__USER_TOKEN".to_string(), github_token),
        ("CONFIG__REPO_CONTEXT_FILES".to_string(), context_files),
        (
            "CONFIG__REPO_CONTEXT_MAX_LINES".to_string(),
            request.entry_max_lines.to_string(),
        ),
        (
            "GITHUB_WORKSPACE".to_string(),
            run_dir.to_string_lossy().into_owned(),
        ),
    ];

    // An empty render (zero-entry snapshot) carries no artifact: pass
    // the map through anyway when non-empty (upstream truncates past
    // its own max_artifact_size=50000; no second truncation here).
    if !request.repo_map.is_empty() {
        let artifact = run_dir.join("repo-map.txt");
        if let Err(error) = fs::write(&artifact, &request.repo_map) {
            let _ = fs::remove_dir_all(&run_dir);
            return Err(format!(
                "cannot write pr-agent repo map {}: {error}",
                artifact.display()
            ));
        }
        env.push((
            "ARTIFACT_PATH".to_string(),
            artifact.to_string_lossy().into_owned(),
        ));
    }

    Ok(Invocation {
        program: python,
        args,
        env,
        run_dir,
        review_json,
    })
}

/// Resolve the `CONFIG__MODEL` value: the transport's rule (trim,
/// strip the `openrouter/` prefix, `DEFAULT_MODEL` fallback —
/// replicated because `transport::resolve_model` is private) followed
/// by exactly one `openrouter/` routing prefix. The prefix is applied
/// unconditionally rather than only to bare names: the subprocess
/// authenticates solely to OpenRouter (`OPENROUTER__KEY` is the only
/// model credential it receives), so every value must route through
/// litellm's OpenRouter provider — a passed-through `anthropic/...`
/// would make litellm try Anthropic with no key. This reproduces the
/// 1.x precedent exactly (`openrouter/meta/muse-spark-1.3-contributor`)
/// and round-trips it.
fn resolve_pr_agent_model(raw: Option<String>) -> String {
    let resolved = match raw {
        Some(raw) => {
            let trimmed = raw.trim();
            let stripped = trimmed.strip_prefix("openrouter/").unwrap_or(trimmed);
            if stripped.is_empty() {
                DEFAULT_MODEL.to_string()
            } else {
                stripped.to_string()
            }
        }
        None => DEFAULT_MODEL.to_string(),
    };
    format!("openrouter/{resolved}")
}

/// Fresh per-run dir: the established nonce pattern
/// (`temp_dir() + pid + nanos`).
fn fresh_run_dir() -> std::io::Result<PathBuf> {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or(0);
    Ok(std::env::temp_dir().join(format!("chomp-pr-agent-{}-{nonce}", std::process::id())))
}

/// Join a pipe-reader thread with a short grace: the child has
/// exited, so its own write ends are closed and the reader should
/// already be at EOF; if a forked grandchild still holds a pipe open,
/// give up after the grace and return what (if anything) arrived.
/// Detaching is safe: the thread owns only its pipe fd.
fn join_soon(handle: thread::JoinHandle<String>) -> String {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !handle.is_finished() && Instant::now() < deadline {
        thread::sleep(POLL_INTERVAL);
    }
    if handle.is_finished() {
        handle.join().unwrap_or_default()
    } else {
        String::new()
    }
}

/// Keep the last `MAX_STDERR_TAIL_CHARS` characters (char-boundary
/// safe) of piped stderr for failure reasons.
fn stderr_tail(text: &str) -> String {
    if text.chars().count() <= MAX_STDERR_TAIL_CHARS {
        return text.to_string();
    }
    let skip = text.chars().count() - MAX_STDERR_TAIL_CHARS;
    text.chars().skip(skip).collect()
}

/// Spawn the invocation, poll to `timeout`, and map every outcome to
/// a [`PrAgentOutcome`]. Stdout/stderr drain on threads so a chatty
/// child can never block on a full pipe while the parent polls.
fn run_invocation(invocation: &Invocation, timeout: Duration) -> PrAgentOutcome {
    let mut child = match Command::new(&invocation.program)
        .args(&invocation.args)
        .envs(invocation.env.iter().cloned())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(child) => child,
        Err(error) => {
            return PrAgentOutcome::ProducerFailure {
                reason: format!(
                    "failed to spawn pr-agent interpreter `{}`: {error}",
                    invocation.program
                ),
            };
        }
    };

    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let stdout_reader = thread::spawn(move || {
        let mut text = String::new();
        if let Some(mut pipe) = stdout {
            let _ = pipe.read_to_string(&mut text);
        }
        text
    });
    let stderr_reader = thread::spawn(move || {
        let mut text = String::new();
        if let Some(mut pipe) = stderr {
            let _ = pipe.read_to_string(&mut text);
        }
        text
    });

    let started = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) => {
                if started.elapsed() >= timeout {
                    break None;
                }
                thread::sleep(POLL_INTERVAL);
            }
            Err(error) => {
                return PrAgentOutcome::ProducerFailure {
                    reason: format!("failed to poll pr-agent subprocess: {error}"),
                };
            }
        }
    };

    let Some(status) = status else {
        // Timeout: kill and reap. The pipe readers are NOT joined
        // here: a killed child may have forked a grandchild that
        // still holds the pipe write ends open, in which case a join
        // would block until that grandchild exits on its own. The
        // detached readers own only their pipe fds and die with the
        // process; their output is irrelevant on the timeout path.
        let _ = child.kill();
        let _ = child.wait();
        drop(stdout_reader);
        drop(stderr_reader);
        return PrAgentOutcome::ProducerFailure {
            reason: format!(
                "pr-agent timed out after {}ms; killed subprocess",
                timeout.as_millis()
            ),
        };
    };

    let _ = child.wait();
    // Bounded joins: the child has exited, but a forked grandchild
    // could in principle still hold a pipe open; never hang the
    // review on that — take what arrived within grace, else move on.
    let _stdout = join_soon(stdout_reader);
    let stderr = join_soon(stderr_reader);
    if !status.success() {
        let tail = stderr_tail(stderr.trim());
        let detail = if tail.is_empty() {
            "(no stderr)".to_string()
        } else {
            tail
        };
        return PrAgentOutcome::ProducerFailure {
            reason: format!("pr-agent exited with status {status}: {detail}"),
        };
    }

    let bytes = match fs::read(&invocation.review_json) {
        Ok(bytes) => bytes,
        Err(error) => {
            return PrAgentOutcome::ProducerFailure {
                reason: format!(
                    "pr-agent succeeded but wrote no review JSON at {}: {error}",
                    invocation.review_json.display()
                ),
            };
        }
    };
    if bytes.is_empty() {
        return PrAgentOutcome::ProducerFailure {
            reason: format!(
                "pr-agent review JSON at {} is empty",
                invocation.review_json.display()
            ),
        };
    }
    let text = match String::from_utf8(bytes) {
        Ok(text) => text,
        Err(_) => {
            return PrAgentOutcome::ProducerFailure {
                reason: format!(
                    "pr-agent review JSON at {} is not valid UTF-8 JSON",
                    invocation.review_json.display()
                ),
            };
        }
    };
    let parsed: serde_json::Value = match serde_json::from_str(&text) {
        Ok(parsed) => parsed,
        Err(error) => {
            return PrAgentOutcome::ProducerFailure {
                reason: format!(
                    "pr-agent review JSON at {} is not valid JSON: {error}",
                    invocation.review_json.display()
                ),
            };
        }
    };
    if !parsed
        .get("review")
        .is_some_and(serde_json::Value::is_object)
    {
        return PrAgentOutcome::ProducerFailure {
            reason: format!(
                "pr-agent review JSON at {} has no `review` object",
                invocation.review_json.display()
            ),
        };
    }
    PrAgentOutcome::Completed { json: text }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;
    use std::sync::{Mutex, OnceLock};

    /// Serializes every test that mutates the process environment:
    /// Rust runs tests in parallel in one process, so unguarded
    /// set/remove races with any other env reader.
    static ENV_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

    fn env_lock() -> std::sync::MutexGuard<'static, ()> {
        // Recover from poisoning: a failed test still runs its
        // `EnvRestore` drops during unwind, so the environment is
        // restored and the next test may proceed (a poisoned lock
        // would otherwise cascade one failure into all).
        ENV_LOCK
            .get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Saves the listed vars and restores them on drop, so env
    /// mutations never leak between tests even on assertion panic.
    struct EnvRestore {
        saved: Vec<(String, Option<String>)>,
    }

    impl EnvRestore {
        fn capture(names: &[&str]) -> Self {
            Self {
                saved: names
                    .iter()
                    .map(|name| ((*name).to_string(), std::env::var(name).ok()))
                    .collect(),
            }
        }
    }

    impl Drop for EnvRestore {
        fn drop(&mut self) {
            for (name, value) in self.saved.drain(..) {
                match value {
                    Some(value) => std::env::set_var(&name, value),
                    None => std::env::remove_var(&name),
                }
            }
        }
    }

    fn fixture_dir(label: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock after epoch")
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "chomp-pr-agent-test-{label}-{}-{nonce}",
            std::process::id()
        ));
        fs::create_dir_all(&dir).expect("create fixture dir");
        dir
    }

    fn test_request() -> PrAgentRequest {
        PrAgentRequest {
            pr_url: "https://github.com/example/project/pull/7".to_string(),
            context_files: vec!["AGENTS.md".to_string()],
            repo_map: "src/lib.rs (12)\n".to_string(),
            entry_max_lines: 2000,
            timeout: Duration::from_secs(30),
        }
    }

    fn env_lookup(pairs: &HashMap<String, String>) -> impl Fn(&str) -> Option<String> + '_ {
        move |name| pairs.get(name).cloned()
    }

    fn full_env(python: &str) -> HashMap<String, String> {
        HashMap::from([
            ("FORGE_PR_AGENT_PYTHON".to_string(), python.to_string()),
            (
                "GITHUB__USER_TOKEN".to_string(),
                "token-for-env-map".to_string(),
            ),
            (
                "OPENROUTER_API_KEY".to_string(),
                "key-for-env-map".to_string(),
            ),
            (
                "FORGE_MODEL".to_string(),
                "openrouter/meta/muse-spark-1.3-contributor".to_string(),
            ),
        ])
    }

    /// Write the fake interpreter: a shell script that records its
    /// argv and selected env to `$FAKE_RECORD`, copies the artifact
    /// to `$FAKE_ARTIFACT_COPY`, then behaves per `$FAKE_MODE` —
    /// `ok` (copy `$FAKE_CANNED` to the `--json-output` path),
    /// `exit3`, `sleep`, `nofile`, `empty`, `nonjson`, `noreview`.
    fn write_fake(dir: &Path) -> PathBuf {
        let fake = dir.join("fake-python");
        let script = r#"#!/bin/sh
record="$FAKE_RECORD"
{
  i=0
  for a in "$@"; do
    i=$((i + 1))
    echo "arg$i=$a"
  done
  echo "CONFIG__PUBLISH_OUTPUT=$CONFIG__PUBLISH_OUTPUT"
  echo "CONFIG__PUBLISH_OUTPUT_PROGRESS=$CONFIG__PUBLISH_OUTPUT_PROGRESS"
  echo "CONFIG__MODEL=$CONFIG__MODEL"
  echo "CONFIG__CUSTOM_MODEL_MAX_TOKENS=$CONFIG__CUSTOM_MODEL_MAX_TOKENS"
  echo "CONFIG__REPO_CONTEXT_FILES=$CONFIG__REPO_CONTEXT_FILES"
  echo "CONFIG__REPO_CONTEXT_MAX_LINES=$CONFIG__REPO_CONTEXT_MAX_LINES"
  echo "ARTIFACT_PATH=$ARTIFACT_PATH"
  echo "GITHUB_WORKSPACE=$GITHUB_WORKSPACE"
  if [ -n "$GITHUB__USER_TOKEN" ]; then echo "GITHUB__USER_TOKEN_SET=yes"; else echo "GITHUB__USER_TOKEN_SET=no"; fi
  if [ -n "$OPENROUTER__KEY" ]; then echo "OPENROUTER__KEY_SET=yes"; else echo "OPENROUTER__KEY_SET=no"; fi
} > "$record"
out=""
for a in "$@"; do
  case "$a" in
    --json-output=*) out="${a#--json-output=}" ;;
  esac
done
if [ -n "$ARTIFACT_PATH" ] && [ -n "$FAKE_ARTIFACT_COPY" ]; then
  cp "$ARTIFACT_PATH" "$FAKE_ARTIFACT_COPY"
fi
case "$FAKE_MODE" in
  ok) cp "$FAKE_CANNED" "$out" ;;
  exit3) echo "boom: reviewer failed loudly" >&2; exit 3 ;;
  sleep) exec sleep 30 ;;
  nofile) : ;;
  empty) : > "$out" ;;
  nonjson) printf 'not json at all{{{' > "$out" ;;
  noreview) printf '{"usage":{"tokens":1}}' > "$out" ;;
esac
"#;
        fs::write(&fake, script).expect("write fake interpreter");
        let mut permissions = fs::metadata(&fake).expect("stat fake").permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&fake, permissions).expect("chmod fake");
        fake
    }

    fn record_map(record: &str) -> HashMap<String, String> {
        record
            .lines()
            .filter_map(|line| line.split_once('='))
            .map(|(key, value)| (key.to_string(), value.to_string()))
            .collect()
    }

    // -- build_invocation (pure, no env mutation) ----------------------

    #[test]
    fn invocation_carries_exact_args_and_env() {
        let dir = fixture_dir("wire");
        let env = full_env("python3-test");
        let request = test_request();
        let invocation = build_invocation(&request, &env_lookup(&env)).expect("build invocation");
        assert_eq!(invocation.program, "python3-test");
        let expected_json = invocation.review_json.display().to_string();
        assert_eq!(
            invocation.args,
            vec![
                "-m".to_string(),
                "pr_agent.cli".to_string(),
                format!("--pr_url={}", request.pr_url),
                format!("--json-output={expected_json}"),
                "review".to_string(),
            ]
        );
        let pairs: HashMap<_, _> = invocation.env.into_iter().collect();
        assert_eq!(pairs["CONFIG__PUBLISH_OUTPUT"], "false");
        assert_eq!(pairs["CONFIG__PUBLISH_OUTPUT_PROGRESS"], "false");
        assert_eq!(
            pairs["CONFIG__MODEL"],
            "openrouter/meta/muse-spark-1.3-contributor"
        );
        assert_eq!(pairs["CONFIG__CUSTOM_MODEL_MAX_TOKENS"], "1000000");
        assert_eq!(pairs["CONFIG__REPO_CONTEXT_FILES"], r#"["AGENTS.md"]"#);
        assert_eq!(pairs["CONFIG__REPO_CONTEXT_MAX_LINES"], "2000");
        // Secrets travel by name only: values come from the env, never
        // from the request, and are asserted only in the live-env
        // tests by presence, never by value.
        assert!(pairs.contains_key("GITHUB__USER_TOKEN"));
        assert!(pairs.contains_key("OPENROUTER__KEY"));
        assert!(pairs.contains_key("ARTIFACT_PATH"));
        assert_eq!(
            Path::new(&pairs["GITHUB_WORKSPACE"]),
            invocation.run_dir.as_path()
        );
        assert!(invocation.review_json.starts_with(&invocation.run_dir));
        fs::remove_dir_all(&dir).ok();
        fs::remove_dir_all(&invocation.run_dir).ok();
    }

    #[test]
    fn fresh_invocation_has_no_predated_review_json() {
        // Guards the stale-output fix: whatever the nonce dir
        // collided with, `review.json` must not exist before the
        // spawn, so a child that exits 0 without writing can never
        // be credited with a previous run's success.
        let env = full_env("python3");
        let invocation =
            build_invocation(&test_request(), &env_lookup(&env)).expect("build invocation");
        assert!(
            !invocation.review_json.exists(),
            "review.json {} must not predate the spawn",
            invocation.review_json.display()
        );
        fs::remove_dir_all(&invocation.run_dir).ok();
    }

    #[test]
    fn empty_context_list_is_sent_explicitly_as_empty_json() {
        let env = full_env("python3");
        let mut request = test_request();
        request.context_files = Vec::new();
        let invocation = build_invocation(&request, &env_lookup(&env)).expect("build invocation");
        let pairs: HashMap<_, _> = invocation.env.into_iter().collect();
        assert_eq!(
            pairs.get("CONFIG__REPO_CONTEXT_FILES").map(String::as_str),
            Some("[]"),
            "the empty stand-down list must be present, never omitted"
        );
        fs::remove_dir_all(&invocation.run_dir).ok();
    }

    #[test]
    fn empty_repo_map_omits_artifact_path() {
        let env = full_env("python3");
        let mut request = test_request();
        request.repo_map = String::new();
        let invocation = build_invocation(&request, &env_lookup(&env)).expect("build invocation");
        let pairs: HashMap<_, _> = invocation.env.into_iter().collect();
        assert!(
            !pairs.contains_key("ARTIFACT_PATH"),
            "empty map must not inject an empty artifact"
        );
        fs::remove_dir_all(&invocation.run_dir).ok();
    }

    #[test]
    fn missing_credentials_fail_before_spawn() {
        let mut env = full_env("python3");
        env.remove("GITHUB__USER_TOKEN");
        let error = build_invocation(&test_request(), &env_lookup(&env))
            .expect_err("missing token must fail");
        assert!(error.contains("GITHUB__USER_TOKEN"), "{error}");

        let mut env = full_env("python3");
        env.remove("OPENROUTER_API_KEY");
        let error = build_invocation(&test_request(), &env_lookup(&env))
            .expect_err("missing key must fail");
        assert!(error.contains("OPENROUTER_API_KEY"), "{error}");
    }

    #[test]
    fn interpreter_defaults_to_python3() {
        let mut env = full_env("python3");
        env.remove("FORGE_PR_AGENT_PYTHON");
        let invocation =
            build_invocation(&test_request(), &env_lookup(&env)).expect("build invocation");
        assert_eq!(invocation.program, "python3");
        fs::remove_dir_all(&invocation.run_dir).ok();
    }

    #[test]
    fn model_resolution_adds_openrouter_prefix_only_when_bare() {
        assert_eq!(
            resolve_pr_agent_model(None),
            format!("openrouter/{DEFAULT_MODEL}")
        );
        assert_eq!(
            resolve_pr_agent_model(Some(
                "openrouter/meta/muse-spark-1.3-contributor".to_string()
            )),
            "openrouter/meta/muse-spark-1.3-contributor".to_string()
        );
        assert_eq!(
            resolve_pr_agent_model(Some("anthropic/claude-x".to_string())),
            "openrouter/anthropic/claude-x"
        );
        assert_eq!(
            resolve_pr_agent_model(Some("bare-model".to_string())),
            "openrouter/bare-model"
        );
        assert_eq!(
            resolve_pr_agent_model(Some("openrouter/".to_string())),
            format!("openrouter/{DEFAULT_MODEL}")
        );
    }

    // -- live-env runs (serialized through ENV_LOCK) -------------------

    /// Point the process environment at the fake interpreter and its
    /// control vars. Returns the record/artifact-copy paths; the
    /// caller holds `_guard` (the env lock) and `_restore` (var
    /// restoration) for the whole test.
    fn use_fake_env(
        dir: &Path,
        fake: &Path,
        mode: &str,
        canned: Option<&str>,
    ) -> (EnvRestore, PathBuf, PathBuf) {
        let restore = EnvRestore::capture(&[
            "FORGE_PR_AGENT_PYTHON",
            "GITHUB__USER_TOKEN",
            "OPENROUTER_API_KEY",
            "FORGE_MODEL",
            "FAKE_MODE",
            "FAKE_RECORD",
            "FAKE_CANNED",
            "FAKE_ARTIFACT_COPY",
        ]);
        let record = dir.join(format!("record-{mode}"));
        let artifact_copy = dir.join(format!("artifact-{mode}.txt"));
        std::env::set_var("FORGE_PR_AGENT_PYTHON", fake);
        std::env::set_var("GITHUB__USER_TOKEN", "test-github-token");
        std::env::set_var("OPENROUTER_API_KEY", "test-openrouter-key");
        std::env::set_var("FORGE_MODEL", "openrouter/meta/muse-spark-1.3-contributor");
        std::env::set_var("FAKE_MODE", mode);
        std::env::set_var("FAKE_RECORD", &record);
        std::env::set_var("FAKE_ARTIFACT_COPY", &artifact_copy);
        match canned {
            Some(bytes) => {
                let path = dir.join(format!("canned-{mode}.json"));
                fs::write(&path, bytes).expect("write canned JSON");
                std::env::set_var("FAKE_CANNED", &path);
            }
            None => std::env::remove_var("FAKE_CANNED"),
        }
        (restore, record, artifact_copy)
    }

    #[test]
    fn happy_path_returns_byte_identical_json_and_cleans_up() {
        let _lock = env_lock();
        let dir = fixture_dir("happy");
        let fake = write_fake(&dir);
        let canned = "{\"review\":{\"score\":9},\"usage\":{\"tokens\":3},\"coverage\":{}}";
        let (_restore, record, artifact_copy) = use_fake_env(&dir, &fake, "ok", Some(canned));

        let outcome = run_pr_agent(&test_request());
        let PrAgentOutcome::Completed { json } = outcome else {
            panic!("expected Completed, got {outcome:?}");
        };
        assert_eq!(json, canned, "json must be the file's exact bytes");

        let recorded = fs::read_to_string(&record).expect("fake record exists");
        let pairs = record_map(&recorded);
        let request = test_request();
        let json_path = pairs
            .get("arg4")
            .and_then(|arg| arg.strip_prefix("--json-output="))
            .expect("fake records the --json-output arg");
        assert_eq!(
            pairs.get("arg1").map(String::as_str),
            Some("-m"),
            "full argv:\n{recorded}"
        );
        assert_eq!(pairs.get("arg2").map(String::as_str), Some("pr_agent.cli"));
        assert_eq!(
            pairs.get("arg3").map(String::as_str),
            Some(format!("--pr_url={}", request.pr_url).as_str())
        );
        assert_eq!(pairs.get("arg5").map(String::as_str), Some("review"));
        assert_eq!(pairs["CONFIG__PUBLISH_OUTPUT"], "false");
        assert_eq!(pairs["CONFIG__PUBLISH_OUTPUT_PROGRESS"], "false");
        assert_eq!(
            pairs["CONFIG__MODEL"],
            "openrouter/meta/muse-spark-1.3-contributor"
        );
        assert_eq!(pairs["CONFIG__CUSTOM_MODEL_MAX_TOKENS"], "1000000");
        assert_eq!(pairs["CONFIG__REPO_CONTEXT_FILES"], r#"["AGENTS.md"]"#);
        assert_eq!(pairs["CONFIG__REPO_CONTEXT_MAX_LINES"], "2000");
        // Secrets cross by presence only — never asserted by value.
        assert_eq!(pairs["GITHUB__USER_TOKEN_SET"], "yes");
        assert_eq!(pairs["OPENROUTER__KEY_SET"], "yes");

        // The repo map reaches the artifact path byte-for-byte, and
        // the whole run dir (workspace + outputs) is gone after.
        let workspace = PathBuf::from(&pairs["GITHUB_WORKSPACE"]);
        assert_eq!(
            fs::read_to_string(&artifact_copy).expect("artifact copy"),
            request.repo_map
        );
        assert!(
            !workspace.exists(),
            "run dir {} must be removed",
            workspace.display()
        );
        assert!(
            !Path::new(json_path).exists(),
            "review.json lives in the removed run dir"
        );
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn nonzero_exit_maps_to_failure_with_status_and_stderr() {
        let _lock = env_lock();
        let dir = fixture_dir("exit3");
        let fake = write_fake(&dir);
        let (_restore, record, _artifact) = use_fake_env(&dir, &fake, "exit3", None);

        let outcome = run_pr_agent(&test_request());
        let PrAgentOutcome::ProducerFailure { reason } = outcome else {
            panic!("expected ProducerFailure, got {outcome:?}");
        };
        assert!(reason.contains("exit status: 3"), "{reason}");
        assert!(reason.contains("boom: reviewer failed loudly"), "{reason}");
        assert!(
            !reason.contains("test-github-token"),
            "no secrets: {reason}"
        );
        assert!(
            !reason.contains("test-openrouter-key"),
            "no secrets: {reason}"
        );
        // The run dir is cleaned even on failure.
        let recorded = fs::read_to_string(&record).expect("fake record exists");
        let workspace = PathBuf::from(&record_map(&recorded)["GITHUB_WORKSPACE"]);
        assert!(!workspace.exists(), "run dir must be removed on failure");
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn killed_subprocess_maps_to_timeout_and_finishes_fast() {
        let _lock = env_lock();
        let dir = fixture_dir("timeout");
        let fake = write_fake(&dir);
        let (_restore, _record, _artifact) = use_fake_env(&dir, &fake, "sleep", None);

        let mut request = test_request();
        request.timeout = Duration::from_millis(200);
        let started = Instant::now();
        let outcome = run_pr_agent(&request);
        let elapsed = started.elapsed();
        let PrAgentOutcome::ProducerFailure { reason } = outcome else {
            panic!("expected ProducerFailure, got {outcome:?}");
        };
        assert!(reason.contains("timed out"), "{reason}");
        assert!(
            elapsed < Duration::from_secs(10),
            "timeout must kill the 30s sleeper, took {elapsed:?}"
        );
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn invalid_outputs_map_to_distinct_failures() {
        let _lock = env_lock();
        let dir = fixture_dir("bad-output");
        let fake = write_fake(&dir);
        let mut reasons = Vec::new();
        for mode in ["nofile", "empty", "nonjson", "noreview"] {
            let (_restore, _record, _artifact) = use_fake_env(&dir, &fake, mode, None);
            let outcome = run_pr_agent(&test_request());
            let PrAgentOutcome::ProducerFailure { reason } = outcome else {
                panic!("mode {mode}: expected ProducerFailure, got {outcome:?}");
            };
            reasons.push(reason);
        }
        assert!(reasons[0].contains("no review JSON"), "{}", reasons[0]);
        assert!(reasons[1].contains("is empty"), "{}", reasons[1]);
        assert!(reasons[2].contains("not valid JSON"), "{}", reasons[2]);
        assert!(reasons[3].contains("no `review` object"), "{}", reasons[3]);
        let unique: std::collections::HashSet<_> = reasons.iter().collect();
        assert_eq!(
            unique.len(),
            4,
            "each bad-output mode needs a distinct reason"
        );
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn missing_credentials_spawn_nothing() {
        let _lock = env_lock();
        let dir = fixture_dir("nospawn");
        let fake = write_fake(&dir);
        for missing in ["GITHUB__USER_TOKEN", "OPENROUTER_API_KEY"] {
            let (_restore, record, _artifact) =
                use_fake_env(&dir, &fake, "ok", Some("{\"review\":{}}"));
            std::env::remove_var(missing);
            let _ = fs::remove_file(&record);
            let outcome = run_pr_agent(&test_request());
            let PrAgentOutcome::ProducerFailure { reason } = outcome else {
                panic!("missing {missing}: expected ProducerFailure, got {outcome:?}");
            };
            assert!(reason.contains(missing), "{reason}");
            assert!(
                !record.exists(),
                "missing {missing}: fake record {record:?} must be absent (no spawn)"
            );
        }
        fs::remove_dir_all(&dir).ok();
    }
}
