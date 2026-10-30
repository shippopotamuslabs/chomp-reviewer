//! Forge configuration loading and contract entry-file resolution.
//!
//! The configuration surface is the `[forge]` table of the caller's
//! `.pr_agent.toml` (FRG-15). Resolution (FRG-20, proposal §8.7)
//! reads the entry file from the **base branch** of the pull request,
//! never the head: the rules a PR is judged against must not be
//! rewritable by the PR itself. The resolved content reaches lens
//! sessions through this module's channel only — the FRG-18 general
//! tools are rooted at the head snapshot and cannot read the base
//! tree, so a session can never confuse head code with base rules.

use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde::Deserialize;

const DEFAULT_ENTRY_FILE: &str = "AGENTS.md";
const DEFAULT_ENTRY_MAX_LINES: usize = 2_000;
const DEFAULT_DEEP_REVIEW: bool = true;
const DEFAULT_LENS_TIMEOUT_SECS: u64 = 600;
const DEFAULT_PR_AGENT_TIMEOUT_SECS: u64 = 900;
const DEFAULT_SYNTHESIS_TIMEOUT_SECS: u64 = 600;
/// Decision D6 (proposal §17): the synthesizer's `run_pr_agent`
/// follow-up cap per review. Zero disables follow-ups (every call
/// fails cap-exhausted); it is a setting, not an error, so unlike the
/// timeouts it has no greater-than-zero validation.
const DEFAULT_PR_AGENT_FOLLOWUPS: usize = 2;

/// The `[forge]` section of a repository's `.pr_agent.toml` file.
///
/// Unknown keys in this section and elsewhere in the file are ignored so the
/// file can keep serving PR-Agent and future Chomp configuration surfaces.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForgeConfig {
    /// Repository agent/contract entry file, relative to the repo root.
    pub entry_file: String,
    /// Additional context files handed to PR-Agent's static context channel.
    pub pr_agent_context_files: Vec<String>,
    /// Size-guard ceiling for the entry file, in lines (proposal §17
    /// decision D3). An entry file past the ceiling is truncated at
    /// resolution, with the truncation recorded.
    pub entry_max_lines: usize,
    /// Full fan-out runs when true. Reserved: the reduced mode is not
    /// available in Forge 2.0 v1; the fan-out owns the ignore-with-record
    /// behavior, not the parser.
    pub deep_review: bool,
    /// Per-lens stage timeout, in seconds.
    pub lens_timeout_secs: u64,
    /// PR-Agent stage timeout, in seconds.
    pub pr_agent_timeout_secs: u64,
    /// Reserved for FRG-26 synthesis; unused by any stage in this ticket.
    pub synthesis_timeout_secs: u64,
    /// Synthesizer's `run_pr_agent` follow-up cap per review (FRG-27,
    /// proposal §11/D6). Enforced by the host; zero disables.
    pub pr_agent_followups: usize,
}

impl Default for ForgeConfig {
    fn default() -> Self {
        let entry_file = DEFAULT_ENTRY_FILE.to_string();
        Self {
            pr_agent_context_files: vec![entry_file.clone()],
            entry_file,
            entry_max_lines: DEFAULT_ENTRY_MAX_LINES,
            deep_review: DEFAULT_DEEP_REVIEW,
            lens_timeout_secs: DEFAULT_LENS_TIMEOUT_SECS,
            pr_agent_timeout_secs: DEFAULT_PR_AGENT_TIMEOUT_SECS,
            synthesis_timeout_secs: DEFAULT_SYNTHESIS_TIMEOUT_SECS,
            pr_agent_followups: DEFAULT_PR_AGENT_FOLLOWUPS,
        }
    }
}

#[derive(Debug, Default, Deserialize)]
struct ConfigFile {
    forge: Option<ForgeSection>,
}

#[derive(Debug, Default, Deserialize)]
struct ForgeSection {
    entry_file: Option<String>,
    pr_agent_context_files: Option<Vec<String>>,
    entry_max_lines: Option<usize>,
    deep_review: Option<bool>,
    lens_timeout_secs: Option<u64>,
    pr_agent_timeout_secs: Option<u64>,
    synthesis_timeout_secs: Option<u64>,
    pr_agent_followups: Option<usize>,
}

impl From<ForgeSection> for ForgeConfig {
    fn from(section: ForgeSection) -> Self {
        let entry_file = section
            .entry_file
            .unwrap_or_else(|| DEFAULT_ENTRY_FILE.to_string());
        let pr_agent_context_files = section
            .pr_agent_context_files
            .unwrap_or_else(|| vec![entry_file.clone()]);
        Self {
            entry_file,
            pr_agent_context_files,
            entry_max_lines: section.entry_max_lines.unwrap_or(DEFAULT_ENTRY_MAX_LINES),
            deep_review: section.deep_review.unwrap_or(DEFAULT_DEEP_REVIEW),
            lens_timeout_secs: section
                .lens_timeout_secs
                .unwrap_or(DEFAULT_LENS_TIMEOUT_SECS),
            pr_agent_timeout_secs: section
                .pr_agent_timeout_secs
                .unwrap_or(DEFAULT_PR_AGENT_TIMEOUT_SECS),
            synthesis_timeout_secs: section
                .synthesis_timeout_secs
                .unwrap_or(DEFAULT_SYNTHESIS_TIMEOUT_SECS),
            pr_agent_followups: section
                .pr_agent_followups
                .unwrap_or(DEFAULT_PR_AGENT_FOLLOWUPS),
        }
    }
}

/// Parse a configuration document. The document may be a full
/// `.pr_agent.toml`; unrelated tables and keys are ignored.
pub fn parse_config_str(contents: &str, source: &Path) -> Result<ForgeConfig, String> {
    let parsed: ConfigFile = toml::from_str(contents).map_err(|error| {
        format!(
            "failed to parse Chomp configuration {}: {error}",
            source.display()
        )
    })?;
    let config = parsed.forge.map(ForgeConfig::from).unwrap_or_default();
    for (key, value) in [
        ("lens_timeout_secs", config.lens_timeout_secs),
        ("pr_agent_timeout_secs", config.pr_agent_timeout_secs),
        ("synthesis_timeout_secs", config.synthesis_timeout_secs),
    ] {
        if value == 0 {
            return Err(format!(
                "invalid Chomp configuration {}: {key} must be greater than 0",
                source.display()
            ));
        }
    }
    Ok(config)
}

/// Load the effective configuration for a review.
///
/// Precedence is explicit `--config`, then `<repo>/.pr_agent.toml`, then the
/// built-in defaults. The explicit file is the workflow's override channel.
pub fn load_config(explicit_path: Option<&Path>, repo: &Path) -> Result<ForgeConfig, String> {
    let repo_config: PathBuf = repo.join(".pr_agent.toml");
    let source = match explicit_path {
        Some(path) => Some(path.to_path_buf()),
        None if repo_config.exists() => Some(repo_config),
        None => None,
    };

    let Some(source) = source else {
        return Ok(ForgeConfig::default());
    };

    let contents = fs::read_to_string(&source).map_err(|error| {
        format!(
            "failed to read Chomp configuration {}: {error}",
            source.display()
        )
    })?;
    parse_config_str(&contents, &source)
}

/// Why the entry file is absent from the base branch. Both cases
/// degrade the same way (the contract channel is empty); the reason
/// is carried for the recorded stand-down note.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AbsentReason {
    /// No file exists at the configured path on the base ref.
    Missing,
    /// A file exists there but is not UTF-8 text, so it cannot serve
    /// as a governing document.
    NotText,
}

/// The outcome of resolving the contract entry file against the base
/// branch — the contract channel's payload for lens sessions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContractResolution {
    /// The governing copy, read from the base branch. Content past
    /// the size-guard ceiling is truncated, with
    /// `truncated_from_lines` recording the original line total.
    Governing {
        entry_file: String,
        content: String,
        truncated_from_lines: Option<usize>,
    },
    /// No governing copy: the contract lens stands down and the
    /// other lenses proceed without contract context (§8.7 rule 4).
    Absent {
        entry_file: String,
        reason: AbsentReason,
    },
}

impl ContractResolution {
    /// The context files PR-Agent's static channel should receive
    /// (§8.7 rule 4): the configured list when a governing copy
    /// exists, empty when the contract channel stands down. The
    /// resolution rule and its tests land here; the consumer is
    /// FRG-23's adapter, so production code does not call this yet.
    #[allow(dead_code)]
    pub fn pr_agent_context_files(&self, config: &ForgeConfig) -> Vec<String> {
        match self {
            ContractResolution::Governing { .. } => config.pr_agent_context_files.clone(),
            ContractResolution::Absent { .. } => Vec::new(),
        }
    }

    /// The host event this resolution records in the decision log,
    /// as `(event, detail)`: a truncation and a stand-down are
    /// recorded; a clean governing copy is the unremarkable case and
    /// records nothing.
    pub fn recorded_event(&self) -> Option<(&'static str, String)> {
        match self {
            ContractResolution::Governing {
                entry_file,
                content,
                truncated_from_lines: Some(total),
            } => Some((
                "entry_truncated",
                format!(
                    "{entry_file} truncated to its first {} of {total} lines by the size guard",
                    content.lines().count()
                ),
            )),
            ContractResolution::Absent {
                entry_file,
                reason,
            } => Some((
                "entry_absent",
                match reason {
                    AbsentReason::Missing => format!(
                        "no entry file at {entry_file} on the base ref; contract channel empty"
                    ),
                    AbsentReason::NotText => format!(
                        "entry file at {entry_file} on the base ref is not UTF-8 text; contract channel empty"
                    ),
                },
            )),
            ContractResolution::Governing { .. } => None,
        }
    }
}

/// Resolution failures. Absence of the entry file is NOT a failure
/// (it is [`ContractResolution::Absent`]); a failure means the
/// caller's own inputs are wrong — the base ref does not resolve, or
/// git itself cannot run against the checkout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContractError {
    BadBaseRef { base_ref: String, detail: String },
    Git { detail: String },
}

impl fmt::Display for ContractError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ContractError::BadBaseRef { base_ref, detail } => {
                write!(f, "base ref `{base_ref}` does not resolve: {detail}")
            }
            ContractError::Git { detail } => {
                write!(f, "git failed during contract resolution: {detail}")
            }
        }
    }
}

impl std::error::Error for ContractError {}

fn git(repo: &Path, args: &[&str]) -> Result<Output, ContractError> {
    Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .map_err(|error| ContractError::Git {
            detail: error.to_string(),
        })
}

fn stderr_text(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).trim().to_string()
}

/// Resolve the contract entry file from the base branch.
///
/// Read-only git against the caller's checkout. The configured entry
/// path is only ever an argument to `git show <ref>:<path>` — never
/// opened on the filesystem — so an odd or hostile `entry_file`
/// cannot escape the tree: git rejects it, which lands on
/// [`AbsentReason::Missing`].
pub fn resolve_contract(
    repo: &Path,
    base_ref: &str,
    config: &ForgeConfig,
) -> Result<ContractResolution, ContractError> {
    let verify = git(
        repo,
        &["rev-parse", "--verify", &format!("{base_ref}^{{commit}}")],
    )?;
    if !verify.status.success() {
        return Err(ContractError::BadBaseRef {
            base_ref: base_ref.to_string(),
            detail: stderr_text(&verify),
        });
    }

    let entry_file = config.entry_file.clone();
    let spec = format!("{base_ref}:{entry_file}");
    let exists = git(repo, &["cat-file", "-e", &spec])?;
    if !exists.status.success() {
        return Ok(ContractResolution::Absent {
            entry_file,
            reason: AbsentReason::Missing,
        });
    }
    let shown = git(repo, &["show", &spec])?;
    if !shown.status.success() {
        // Present at the existence check, gone or unreadable at the
        // read: degrade to absence rather than fail the review.
        return Ok(ContractResolution::Absent {
            entry_file,
            reason: AbsentReason::Missing,
        });
    }
    let text = match String::from_utf8(shown.stdout) {
        Ok(text) => text,
        Err(_) => {
            return Ok(ContractResolution::Absent {
                entry_file,
                reason: AbsentReason::NotText,
            });
        }
    };

    let total_lines = text.lines().count();
    if total_lines > config.entry_max_lines {
        let mut content: String = text
            .lines()
            .take(config.entry_max_lines)
            .collect::<Vec<_>>()
            .join("\n");
        content.push('\n');
        return Ok(ContractResolution::Governing {
            entry_file,
            content,
            truncated_from_lines: Some(total_lines),
        });
    }
    Ok(ContractResolution::Governing {
        entry_file,
        content: text,
        truncated_from_lines: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_dir(label: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock after epoch")
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "chomp-contract-{label}-{}-{nonce}",
            std::process::id()
        ));
        fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    #[test]
    fn defaults_when_no_config_file_exists() {
        let repo = temp_dir("defaults");
        let config = load_config(None, &repo).expect("load defaults");
        assert_eq!(config, ForgeConfig::default());
        assert_eq!(config.entry_file, "AGENTS.md");
        assert_eq!(config.pr_agent_context_files, vec!["AGENTS.md".to_string()]);
        assert_eq!(config.entry_max_lines, 2_000);
        fs::remove_dir_all(repo).expect("clean temp dir");
    }

    #[test]
    fn empty_document_uses_defaults() {
        let config = parse_config_str("", Path::new("fixture.toml")).expect("parse empty");
        assert_eq!(config, ForgeConfig::default());
    }

    #[test]
    fn forge_values_are_honored() {
        let config = parse_config_str(
            "[forge]\nentry_file = \"CLAUDE.md\"\npr_agent_context_files = [\"CLAUDE.md\", \"docs/contract.md\"]\nentry_max_lines = 42\n",
            Path::new("fixture.toml"),
        )
        .expect("parse forge values");
        assert_eq!(config.entry_file, "CLAUDE.md");
        assert_eq!(
            config.pr_agent_context_files,
            vec!["CLAUDE.md".to_string(), "docs/contract.md".to_string()]
        );
        assert_eq!(config.entry_max_lines, 42);
    }

    #[test]
    fn context_files_default_to_the_entry_file() {
        let config = parse_config_str(
            "[forge]\nentry_file = \"CLAUDE.md\"\n",
            Path::new("fixture.toml"),
        )
        .expect("parse entry only");
        assert_eq!(config.pr_agent_context_files, vec!["CLAUDE.md".to_string()]);
    }

    #[test]
    fn explicit_config_beats_repo_config_and_repo_config_beats_defaults() {
        let repo = temp_dir("precedence");
        fs::write(
            repo.join(".pr_agent.toml"),
            "[forge]\nentry_file = \"REPO.md\"\n",
        )
        .expect("write repo config");
        let explicit = repo.join("workflow.toml");
        fs::write(&explicit, "[forge]\nentry_file = \"WORKFLOW.md\"\n")
            .expect("write explicit config");

        let from_repo = load_config(None, &repo).expect("load repo config");
        assert_eq!(from_repo.entry_file, "REPO.md");

        let from_explicit = load_config(Some(&explicit), &repo).expect("load explicit config");
        assert_eq!(from_explicit.entry_file, "WORKFLOW.md");
        fs::remove_dir_all(repo).expect("clean temp dir");
    }

    #[test]
    fn malformed_toml_is_an_error_with_the_source_named() {
        let error = parse_config_str("[forge]\nentry_file = ", Path::new("broken.toml"))
            .expect_err("malformed TOML must fail");
        assert!(
            error.contains("failed to parse Chomp configuration broken.toml"),
            "{error}"
        );
    }

    #[test]
    fn foreign_tables_and_keys_are_ignored() {
        let config = parse_config_str(
            "[pr_agent]\nmodel = \"some-model\"\n\n[forge]\nentry_file = \"GUIDE.md\"\nfuture_key = 3\n\n[unrelated]\nvalues = [1, 2]\n",
            Path::new("fixture.toml"),
        )
        .expect("foreign content ignored");
        assert_eq!(config.entry_file, "GUIDE.md");
        assert_eq!(config.pr_agent_context_files, vec!["GUIDE.md".to_string()]);
    }

    #[test]
    fn timeout_keys_default_when_absent() {
        let config = parse_config_str("", Path::new("fixture.toml")).expect("parse empty");
        assert_eq!(config.lens_timeout_secs, 600);
        assert_eq!(config.pr_agent_timeout_secs, 900);
        assert_eq!(config.synthesis_timeout_secs, 600);
        assert!(config.deep_review);
    }

    #[test]
    fn all_four_keys_land_verbatim() {
        let config = parse_config_str(
            "[forge]\ndeep_review = false\nlens_timeout_secs = 30\npr_agent_timeout_secs = 45\nsynthesis_timeout_secs = 60\n",
            Path::new("fixture.toml"),
        )
        .expect("parse four keys");
        assert!(!config.deep_review);
        assert_eq!(config.lens_timeout_secs, 30);
        assert_eq!(config.pr_agent_timeout_secs, 45);
        assert_eq!(config.synthesis_timeout_secs, 60);
    }

    #[test]
    fn followup_cap_defaults_to_two_and_parses() {
        let config = parse_config_str("", Path::new("fixture.toml")).expect("parse empty");
        assert_eq!(config.pr_agent_followups, 2);

        let config = parse_config_str(
            "[forge]\npr_agent_followups = 1\n",
            Path::new("fixture.toml"),
        )
        .expect("parse followup cap");
        assert_eq!(config.pr_agent_followups, 1);
    }

    #[test]
    fn zero_followups_disables_rather_than_erroring() {
        let config = parse_config_str(
            "[forge]\npr_agent_followups = 0\n",
            Path::new("fixture.toml"),
        )
        .expect("zero followups parses: it disables the tool");
        assert_eq!(config.pr_agent_followups, 0);
    }

    #[test]
    fn zero_lens_timeout_is_an_error_naming_the_key() {
        let error = parse_config_str(
            "[forge]\nlens_timeout_secs = 0\n",
            Path::new("fixture.toml"),
        )
        .expect_err("zero lens timeout must fail");
        assert!(error.contains("lens_timeout_secs"), "{error}");
    }

    #[test]
    fn zero_pr_agent_timeout_is_an_error_naming_the_key() {
        let error = parse_config_str(
            "[forge]\npr_agent_timeout_secs = 0\n",
            Path::new("fixture.toml"),
        )
        .expect_err("zero pr_agent timeout must fail");
        assert!(error.contains("pr_agent_timeout_secs"), "{error}");
    }

    #[test]
    fn zero_synthesis_timeout_is_an_error_naming_the_key() {
        let error = parse_config_str(
            "[forge]\nsynthesis_timeout_secs = 0\n",
            Path::new("fixture.toml"),
        )
        .expect_err("zero synthesis timeout must fail");
        assert!(error.contains("synthesis_timeout_secs"), "{error}");
    }

    #[test]
    fn deep_review_false_is_preserved() {
        let config = parse_config_str("[forge]\ndeep_review = false\n", Path::new("fixture.toml"))
            .expect("parse deep_review=false");
        assert!(!config.deep_review);
        assert_eq!(config.lens_timeout_secs, 600);
        assert_eq!(config.pr_agent_timeout_secs, 900);
        assert_eq!(config.synthesis_timeout_secs, 600);
    }

    /// A git repo on branch `main` with one base commit; tests stage
    /// head states on a `feature` branch on top.
    struct GitFixture {
        repo: PathBuf,
    }

    impl GitFixture {
        fn new(label: &str) -> Self {
            let repo = temp_dir(label);
            let fixture = Self { repo };
            fixture.git(&["init", "-q", "-b", "main"]);
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

        fn write(&self, rel: &str, contents: &[u8]) {
            let path = self.repo.join(rel);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).expect("create parent dir");
            }
            fs::write(path, contents).expect("write fixture file");
        }

        fn commit_all(&self, message: &str) {
            self.git(&["add", "-A"]);
            self.git(&["commit", "-q", "-m", message]);
        }
    }

    impl Drop for GitFixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.repo);
        }
    }

    #[test]
    fn governing_copy_comes_from_the_base_branch_not_the_head() {
        let fixture = GitFixture::new("base-wins");
        fixture.write("AGENTS.md", b"BASE RULE: judge by this.\n");
        fixture.commit_all("base");
        fixture.git(&["checkout", "-q", "-b", "feature"]);
        fixture.write("AGENTS.md", b"HEAD RULE: excuse everything.\n");
        fixture.commit_all("head edit");

        let resolution =
            resolve_contract(&fixture.repo, "main", &ForgeConfig::default()).expect("resolve");
        let ContractResolution::Governing {
            content,
            truncated_from_lines,
            ..
        } = &resolution
        else {
            panic!("expected a governing copy, got {resolution:?}");
        };
        assert_eq!(content, "BASE RULE: judge by this.\n");
        assert_eq!(*truncated_from_lines, None);
        assert_eq!(resolution.recorded_event(), None);
        assert_eq!(
            resolution.pr_agent_context_files(&ForgeConfig::default()),
            vec!["AGENTS.md".to_string()]
        );
    }

    #[test]
    fn missing_entry_file_is_absence_not_an_error() {
        let fixture = GitFixture::new("absent");
        fixture.write("src/lib.rs", b"pub fn f() {}\n");
        fixture.commit_all("base");

        let resolution =
            resolve_contract(&fixture.repo, "main", &ForgeConfig::default()).expect("resolve");
        assert_eq!(
            resolution,
            ContractResolution::Absent {
                entry_file: "AGENTS.md".to_string(),
                reason: AbsentReason::Missing,
            }
        );
        let (event, detail) = resolution.recorded_event().expect("absence is recorded");
        assert_eq!(event, "entry_absent");
        assert!(detail.contains("AGENTS.md"), "{detail}");
        assert!(resolution
            .pr_agent_context_files(&ForgeConfig::default())
            .is_empty());
    }

    #[test]
    fn non_text_entry_file_degrades_to_absence() {
        let fixture = GitFixture::new("binary");
        fixture.write("AGENTS.md", &[0xff, 0xfe, 0x00, 0x01]);
        fixture.commit_all("base");

        let resolution =
            resolve_contract(&fixture.repo, "main", &ForgeConfig::default()).expect("resolve");
        assert_eq!(
            resolution,
            ContractResolution::Absent {
                entry_file: "AGENTS.md".to_string(),
                reason: AbsentReason::NotText,
            }
        );
    }

    #[test]
    fn oversized_entry_file_is_truncated_at_the_ceiling_and_recorded() {
        let fixture = GitFixture::new("truncated");
        let mut text = String::new();
        for line in 1..=10 {
            text.push_str(&format!("rule line {line}\n"));
        }
        fixture.write("AGENTS.md", text.as_bytes());
        fixture.commit_all("base");

        let config = ForgeConfig {
            entry_max_lines: 3,
            ..ForgeConfig::default()
        };
        let resolution = resolve_contract(&fixture.repo, "main", &config).expect("resolve");
        let ContractResolution::Governing {
            content,
            truncated_from_lines,
            ..
        } = &resolution
        else {
            panic!("expected a governing copy, got {resolution:?}");
        };
        assert_eq!(content, "rule line 1\nrule line 2\nrule line 3\n");
        assert_eq!(*truncated_from_lines, Some(10));
        let (event, detail) = resolution.recorded_event().expect("truncation is recorded");
        assert_eq!(event, "entry_truncated");
        assert!(detail.contains("first 3 of 10 lines"), "{detail}");
    }

    #[test]
    fn an_unresolvable_base_ref_is_a_caller_error() {
        let fixture = GitFixture::new("bad-ref");
        fixture.write("AGENTS.md", b"rules\n");
        fixture.commit_all("base");

        let error = resolve_contract(&fixture.repo, "no-such-branch", &ForgeConfig::default())
            .expect_err("a bad base ref must fail");
        assert!(
            matches!(error, ContractError::BadBaseRef { .. }),
            "{error:?}"
        );
        assert!(error.to_string().contains("no-such-branch"), "{error}");
    }
}
