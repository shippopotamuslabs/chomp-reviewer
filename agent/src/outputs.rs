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
    let mut decisions = serde_json::to_string_pretty(&decision_log)
        .map_err(|error| format!("failed to serialize Chomp decision log: {error}"))?;
    decisions.push('\n');
    write_file(decisions_path, decisions.as_bytes(), "decision log")?;
    write_file(pr_agent_path, b"", "PR-Agent guide")
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

    #[test]
    fn writes_the_scaffold_output_shapes() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock after epoch")
            .as_nanos();
        let dir: PathBuf =
            std::env::temp_dir().join(format!("chomp-outputs-{}-{nonce}", std::process::id()));
        fs::create_dir_all(&dir).expect("create temp dir");
        let findings = dir.join("findings.json");
        let decisions = dir.join("decisions.json");
        let pr_agent = dir.join("pr-agent.md");

        write_outputs(&findings, &decisions, &pr_agent).expect("write outputs");

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
}
