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
        let entry_file = section.entry_file.unwrap_or_else(|| DEFAULT_ENTRY_FILE.to_string());
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
