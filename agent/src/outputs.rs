//! Writers for Chomp's findings JSON and synthesis decision log, plus
//! the [`Finding`] type: the findings output contract (proposal §8.5),
//! shared by every lens and consumed downstream by synthesis (FRG-26).

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Finding severity, exactly the four values the output contract
/// allows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Severity {
    Critical,
    High,
    Medium,
    Low,
}

/// One review finding in the §8.5 output contract. Field names and
/// shapes are the contract — synthesis (FRG-26) and the renderers
/// consume this JSON — so deserialization denies unknown fields: a
/// session emitting anything else has not produced a valid finding.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Finding {
    pub file: String,
    pub line_start: u64,
    pub line_end: u64,
    pub severity: Severity,
    pub category: String,
    pub title: String,
    pub description: String,
    pub evidence: String,
    pub fix_focus_areas: Vec<String>,
    pub recommended_fix: String,
    /// `path:line` locations the session actually read in support of
    /// relational claims (callers, callees, consumers). Optional in
    /// the contract; synthesis treats uncited relational claims as
    /// unverified.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub citations: Option<Vec<String>>,
}

/// Versioned synthesis decision log.
///
/// Entries are kind-namespaced JSON objects. Host events land from
/// FRG-20 (contract resolution); per-finding dispositions join in
/// FRG-25/FRG-26 under their own kinds.
#[derive(Debug, Serialize)]
struct DecisionLog {
    version: u32,
    entries: Vec<serde_json::Value>,
}

/// One host event for the decision log: a named occurrence in a
/// named subsystem, with the human-readable specifics attached.
/// Producer dispositions (FRG-25/26) carry richer per-kind shapes;
/// host events are deliberately this small.
pub fn host_event(kind: &str, event: &str, detail: &str) -> serde_json::Value {
    serde_json::json!({
        "kind": kind,
        "event": event,
        "detail": detail,
    })
}

/// Write one findings array (combined or per-lens) to a path.
///
/// An empty slice is a legitimately empty review and writes `[]`.
/// Serialization byte-shape is the established pretty-printed array
/// plus a trailing newline.
pub fn write_findings(findings: &[Finding], findings_path: &Path) -> Result<(), String> {
    write_json(findings, findings_path, "findings")
}

/// Write the clustered combined findings: each member carries its
/// pre-dedup cluster tag (FRG-25) for the synthesizer (FRG-26).
/// Same byte-shape convention as [`write_findings`].
pub fn write_clustered_findings(
    clustered: &[crate::synthesis::ClusteredFinding],
    findings_path: &Path,
) -> Result<(), String> {
    write_json(clustered, findings_path, "clustered findings")
}

fn write_json<T: serde::Serialize + ?Sized>(
    value: &T,
    path: &Path,
    label: &str,
) -> Result<(), String> {
    let mut serialized = serde_json::to_string_pretty(value)
        .map_err(|error| format!("failed to serialize Chomp {label}: {error}"))?;
    serialized.push('\n');
    write_file(path, serialized.as_bytes(), label)
}

/// Write the decision log: the run's recorded entries under
/// `DecisionLog{version:1}`.
pub fn write_decision_log(
    decision_entries: &[serde_json::Value],
    decisions_path: &Path,
) -> Result<(), String> {
    let decision_log = DecisionLog {
        version: 1,
        entries: decision_entries.to_vec(),
    };
    let mut decisions = serde_json::to_string_pretty(&decision_log)
        .map_err(|error| format!("failed to serialize Chomp decision log: {error}"))?;
    decisions.push('\n');
    write_file(decisions_path, decisions.as_bytes(), "decision log")
}

/// Write the PR-Agent payload: the adapter's review JSON on
/// completion, an empty file otherwise (today's behavior when the
/// producer fails closed before spawn).
pub fn write_pr_agent(payload: Option<&str>, pr_agent_path: &Path) -> Result<(), String> {
    match payload {
        Some(review_json) => write_file(pr_agent_path, review_json.as_bytes(), "PR-Agent guide"),
        None => write_file(pr_agent_path, b"", "PR-Agent guide"),
    }
}

/// Derive a per-lens findings path from the combined `--out-findings`
/// path (plan D6): when the file name ends in `.json`, insert
/// `.<stage>` before it (`findings.json` →
/// `findings.correctness.json`); otherwise append `.<stage>.json`.
pub fn lens_findings_path(out_findings: &Path, stage: &str) -> PathBuf {
    if out_findings.extension().is_some_and(|ext| ext == "json") {
        let stem = out_findings
            .file_stem()
            .expect("a file name with an extension has a stem");
        let mut name = stem.to_os_string();
        name.push(format!(".{stage}.json"));
        out_findings.with_file_name(name)
    } else {
        let mut path = out_findings.as_os_str().to_owned();
        path.push(format!(".{stage}.json"));
        PathBuf::from(path)
    }
}

fn write_file(path: &Path, contents: &[u8], label: &str) -> Result<(), String> {
    fs::write(path, contents)
        .map_err(|error| format!("failed to write {label} {}: {error}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_dir() -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock after epoch")
            .as_nanos();
        let dir =
            std::env::temp_dir().join(format!("chomp-outputs-{}-{nonce}", std::process::id()));
        fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    #[test]
    fn writes_the_empty_output_shapes() {
        let dir = temp_dir();
        let findings = dir.join("findings.json");
        let decisions = dir.join("decisions.json");
        let pr_agent = dir.join("pr-agent.md");

        write_findings(&[], &findings).expect("write findings");
        write_decision_log(&[], &decisions).expect("write decision log");
        write_pr_agent(None, &pr_agent).expect("write pr-agent guide");

        assert_eq!(
            fs::read_to_string(&findings).expect("read findings"),
            "[]\n"
        );
        let decisions: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&decisions).expect("read decisions"))
                .expect("decisions are JSON");
        assert_eq!(decisions["version"], 1);
        assert_eq!(decisions["entries"], serde_json::json!([]));
        assert_eq!(
            fs::read(&pr_agent).expect("read pr-agent guide"),
            Vec::<u8>::new()
        );
        fs::remove_dir_all(dir).expect("clean temp dir");
    }

    #[test]
    fn writes_findings_with_citations() {
        let dir = temp_dir();
        let findings_path = dir.join("findings.json");
        let finding = Finding {
            file: "src/parse.rs".to_string(),
            line_start: 2,
            line_end: 2,
            severity: Severity::High,
            category: "Correctness".to_string(),
            title: "parse_port panics on non-numeric input".to_string(),
            description: "The strict parse panics.".to_string(),
            evidence: "The caller passes an empty default.".to_string(),
            fix_focus_areas: vec!["src/parse.rs[2-2]".to_string()],
            recommended_fix: "Return a Result.".to_string(),
            citations: Some(vec!["src/main.rs:5".to_string()]),
        };

        write_findings(std::slice::from_ref(&finding), &findings_path).expect("write findings");

        let written: Vec<Finding> =
            serde_json::from_str(&fs::read_to_string(&findings_path).expect("read findings"))
                .expect("findings round-trip");
        assert_eq!(written, vec![finding]);
        fs::remove_dir_all(dir).expect("clean temp dir");
    }

    #[test]
    fn decision_entries_are_written_into_the_log() {
        let dir = temp_dir();
        let decisions = dir.join("decisions.json");
        let entries = vec![host_event(
            "contract",
            "entry_absent",
            "no entry file at AGENTS.md on the base ref; contract channel empty",
        )];

        write_decision_log(&entries, &decisions).expect("write decision log");

        let written: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&decisions).expect("read decisions"))
                .expect("decisions are JSON");
        assert_eq!(written["version"], 1);
        assert_eq!(written["entries"], serde_json::json!(entries));
        fs::remove_dir_all(dir).expect("clean temp dir");
    }

    #[test]
    fn pr_agent_payload_round_trips_verbatim() {
        let dir = temp_dir();
        let pr_agent = dir.join("pr-agent.md");
        let review_json = "{\"review\":{\"summary\":\"looks fine\"}}";

        write_pr_agent(Some(review_json), &pr_agent).expect("write pr-agent guide");

        assert_eq!(
            fs::read_to_string(&pr_agent).expect("read pr-agent guide"),
            review_json
        );
        fs::remove_dir_all(dir).expect("clean temp dir");
    }

    #[test]
    fn lens_paths_insert_the_stage_before_a_json_suffix() {
        assert_eq!(
            lens_findings_path(Path::new("out/findings.json"), "correctness"),
            PathBuf::from("out/findings.correctness.json")
        );
        assert_eq!(
            lens_findings_path(Path::new("out/findings.json"), "pr_agent"),
            PathBuf::from("out/findings.pr_agent.json")
        );
    }

    #[test]
    fn lens_paths_append_the_stage_without_a_json_suffix() {
        assert_eq!(
            lens_findings_path(Path::new("out/findings"), "structure"),
            PathBuf::from("out/findings.structure.json")
        );
    }

    #[test]
    fn lens_paths_keep_nested_dirs_and_multi_dot_names() {
        assert_eq!(
            lens_findings_path(Path::new("out/nested/my.findings.json"), "contract"),
            PathBuf::from("out/nested/my.findings.contract.json")
        );
        assert_eq!(
            lens_findings_path(Path::new("out/nested/my.findings"), "contract"),
            PathBuf::from("out/nested/my.findings.contract.json")
        );
    }
}
