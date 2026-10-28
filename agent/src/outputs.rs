//! Writers for Chomp's findings JSON and synthesis decision log.

use std::fs;
use std::path::Path;

use serde::Serialize;

/// Versioned synthesis decision log.
///
/// FRG-15 emits an empty log. FRG-25 and FRG-26 evolve the entry schema and
/// populate dispositions for producer findings.
#[derive(Debug, Serialize)]
struct DecisionLog {
    version: u32,
    entries: Vec<serde_json::Value>,
}

/// Write the three scaffold review outputs.
///
/// Findings are an empty JSON array (a legitimately empty review), the
/// decision log is the minimal versioned empty object, and the PR-Agent guide
/// is an empty file until FRG-23 captures a real one.
pub fn write_outputs(
    findings_path: &Path,
    decisions_path: &Path,
    pr_agent_path: &Path,
) -> Result<(), String> {
    write_file(findings_path, b"[]\n", "findings")?;

    let decision_log = DecisionLog {
        version: 1,
        entries: Vec::new(),
    };
    let mut decisions = serde_json::to_string_pretty(&decision_log).map_err(|error| {
        format!("failed to serialize Chomp decision log: {error}")
    })?;
    decisions.push('\n');
    write_file(decisions_path, decisions.as_bytes(), "decision log")?;
    write_file(pr_agent_path, b"", "PR-Agent guide")
}

fn write_file(path: &Path, contents: &[u8], label: &str) -> Result<(), String> {
    fs::write(path, contents)
        .map_err(|error| format!("failed to write {label} {}: {error}", path.display()))
}
