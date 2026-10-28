//! Forge configuration loading and, later, contract entry-file resolution.
//!
//! Resolution of the entry file from the base branch, including its size
//! guard and absence behavior, lands in FRG-20.

use std::fs;
use std::path::{Path, PathBuf};

use serde::Deserialize;

const DEFAULT_ENTRY_FILE: &str = "AGENTS.md";

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
}

impl Default for ForgeConfig {
    fn default() -> Self {
        let entry_file = DEFAULT_ENTRY_FILE.to_string();
        Self {
            pr_agent_context_files: vec![entry_file.clone()],
            entry_file,
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
    Ok(parsed.forge.map(ForgeConfig::from).unwrap_or_default())
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
            "[forge]\nentry_file = \"CLAUDE.md\"\npr_agent_context_files = [\"CLAUDE.md\", \"docs/contract.md\"]\n",
            Path::new("fixture.toml"),
        )
        .expect("parse forge values");
        assert_eq!(config.entry_file, "CLAUDE.md");
        assert_eq!(
            config.pr_agent_context_files,
            vec!["CLAUDE.md".to_string(), "docs/contract.md".to_string()]
        );
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
}
