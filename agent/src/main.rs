//! Chomp: a repo-aware agentic code-review host built on `agent_core`.
//!
//! This binary is a host for `agent_core`, an owner of repository tools, and
//! nothing else. FRG-15 scaffolds the CLI, configuration loading, and output
//! contract; the review stages land in their own tickets.

mod contract;
mod host;
mod outputs;
mod pr_agent;
mod sessions;
mod snapshot;
mod synthesis;
mod tools;

use std::env;
use std::ffi::OsString;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::process;

const EXIT_SUCCESS: i32 = 0;
const EXIT_ORCHESTRATION_FAILURE: i32 = 2;

#[derive(Debug, PartialEq, Eq)]
struct ReviewArgs {
    repo: PathBuf,
    diff: PathBuf,
    base_ref: String,
    pr_url: String,
    config: Option<PathBuf>,
    ticket_file: Option<PathBuf>,
    out_findings: PathBuf,
    out_decisions: PathBuf,
    out_pr_agent: PathBuf,
}

#[derive(Debug, PartialEq, Eq)]
enum CliRequest {
    Help,
    Review(Box<ReviewArgs>),
}

fn usage() -> &'static str {
    "Chomp — repo-aware agentic code review\n\
\n\
Usage:\n\
  chomp review --repo <path> --diff <path> --base-ref <ref> --pr-url <url> \\\n\
      [--config <path>] [--ticket-file <path>] \\\n\
      --out-findings <path> --out-decisions <path> --out-pr-agent <path>\n\
  chomp --help\n\
\n\
Review arguments:\n\
  --repo <path>          Path to the pull-request head checkout\n\
  --diff <path>          Path to the pull-request diff file\n\
  --base-ref <ref>       Base branch ref or SHA\n\
  --pr-url <url>         Pull-request URL, for the PR-Agent adapter\n\
  --config <path>        Optional Chomp configuration file; overrides\n\
                         <repo>/.pr_agent.toml when present\n\
  --ticket-file <path>   Optional ticket artifact (FRG-6)\n\
  --out-findings <path>  Path for the final findings JSON\n\
  --out-decisions <path> Path for the synthesis decision log JSON\n\
  --out-pr-agent <path>  Path for the raw PR-Agent guide\n\
\n\
Environment:\n\
  OPENROUTER_API_KEY     API key used by the model transport only\n\
  FORGE_MODEL            Model for all sessions unless overridden\n\
\n\
Exit codes:\n\
  0  Review produced, including a legitimately empty review\n\
  2  Orchestration failure; no review was produced\n"
}

fn parse_cli<I>(args: I) -> Result<CliRequest, String>
where
    I: IntoIterator<Item = OsString>,
{
    let args: Vec<OsString> = args.into_iter().collect();
    let Some(command) = args.first() else {
        return Err("missing command: expected `review` or `--help`".to_string());
    };

    if command == "--help" || command == "-h" {
        if args.len() == 1 {
            return Ok(CliRequest::Help);
        }
        return Err("`--help` does not accept additional arguments".to_string());
    }

    if command != "review" {
        return Err(format!(
            "unknown command `{}`: expected `review` or `--help`",
            command.to_string_lossy()
        ));
    }

    if args
        .iter()
        .skip(1)
        .any(|arg| arg == "--help" || arg == "-h")
    {
        return Ok(CliRequest::Help);
    }

    let mut repo = None;
    let mut diff = None;
    let mut base_ref = None;
    let mut pr_url = None;
    let mut config = None;
    let mut ticket_file = None;
    let mut out_findings = None;
    let mut out_decisions = None;
    let mut out_pr_agent = None;

    let mut index = 1;
    while index < args.len() {
        let flag = args[index].to_string_lossy().into_owned();
        macro_rules! assign {
            ($slot:ident) => {{
                if $slot.is_some() {
                    return Err(format!("duplicate argument `{flag}`"));
                }
                let Some(value) = args.get(index + 1) else {
                    return Err(format!("missing value for `{flag}`"));
                };
                $slot = Some(value.clone());
            }};
        }

        match flag.as_str() {
            "--repo" => assign!(repo),
            "--diff" => assign!(diff),
            "--base-ref" => assign!(base_ref),
            "--pr-url" => assign!(pr_url),
            "--config" => assign!(config),
            "--ticket-file" => assign!(ticket_file),
            "--out-findings" => assign!(out_findings),
            "--out-decisions" => assign!(out_decisions),
            "--out-pr-agent" => assign!(out_pr_agent),
            _ if flag.starts_with("--") => return Err(format!("unknown argument `{flag}`")),
            _ => return Err(format!("unexpected positional argument `{flag}`")),
        }
        index += 2;
    }

    let required = [
        ("--repo", repo),
        ("--diff", diff),
        ("--base-ref", base_ref),
        ("--pr-url", pr_url),
        ("--out-findings", out_findings),
        ("--out-decisions", out_decisions),
        ("--out-pr-agent", out_pr_agent),
    ];
    let mut values = Vec::with_capacity(required.len());
    for (flag, value) in required {
        let Some(value) = value else {
            return Err(format!("missing required argument `{flag}`"));
        };
        values.push(value);
    }

    let mut values = values.into_iter();
    let repo = values.next().expect("required argument count checked");
    let diff = values.next().expect("required argument count checked");
    let base_ref = values.next().expect("required argument count checked");
    let pr_url = values.next().expect("required argument count checked");
    let out_findings = values.next().expect("required argument count checked");
    let out_decisions = values.next().expect("required argument count checked");
    let out_pr_agent = values.next().expect("required argument count checked");

    Ok(CliRequest::Review(Box::new(ReviewArgs {
        repo: PathBuf::from(repo),
        diff: PathBuf::from(diff),
        base_ref: base_ref.to_string_lossy().into_owned(),
        pr_url: pr_url.to_string_lossy().into_owned(),
        config: config.map(PathBuf::from),
        ticket_file: ticket_file.map(PathBuf::from),
        out_findings: PathBuf::from(out_findings),
        out_decisions: PathBuf::from(out_decisions),
        out_pr_agent: PathBuf::from(out_pr_agent),
    })))
}

fn ensure_readable_file(path: &Path, label: &str) -> Result<(), String> {
    // File::open succeeds on directories under Linux; require a regular file.
    let metadata = std::fs::metadata(path)
        .map_err(|error| format!("cannot read {label} {}: {error}", path.display()))?;
    if !metadata.is_file() {
        return Err(format!("{label} {} is not a file", path.display()));
    }
    File::open(path)
        .map(|_| ())
        .map_err(|error| format!("cannot read {label} {}: {error}", path.display()))
}

fn validate_inputs(args: &ReviewArgs) -> Result<(), String> {
    let repo_metadata = std::fs::metadata(&args.repo).map_err(|error| {
        format!(
            "cannot inspect repository checkout {}: {error}",
            args.repo.display()
        )
    })?;
    if !repo_metadata.is_dir() {
        return Err(format!(
            "repository checkout is not a directory: {}",
            args.repo.display()
        ));
    }
    if args.base_ref.trim().is_empty() {
        return Err("--base-ref must not be empty".to_string());
    }
    if args.pr_url.trim().is_empty() {
        return Err("--pr-url must not be empty".to_string());
    }

    ensure_readable_file(&args.diff, "diff file")?;
    if let Some(ticket_file) = &args.ticket_file {
        ensure_readable_file(ticket_file, "ticket file")?;
    }
    Ok(())
}

fn run_review(args: &ReviewArgs) -> Result<(), String> {
    // The scaffold validates and loads the real configuration even though no
    // producer stages consume it yet; later stages inherit this exact flow.
    let _config = contract::load_config(args.config.as_deref(), &args.repo)?;
    validate_inputs(args)?;
    outputs::write_outputs(&args.out_findings, &args.out_decisions, &args.out_pr_agent)
}

fn run() -> i32 {
    match parse_cli(env::args_os().skip(1)) {
        Ok(CliRequest::Help) => {
            print!("{}", usage());
            EXIT_SUCCESS
        }
        Ok(CliRequest::Review(args)) => match run_review(&args) {
            Ok(()) => EXIT_SUCCESS,
            Err(error) => {
                eprintln!("chomp: {error}");
                EXIT_ORCHESTRATION_FAILURE
            }
        },
        Err(error) => {
            eprintln!("chomp: {error}\n\n{}", usage());
            EXIT_ORCHESTRATION_FAILURE
        }
    }
}

fn main() {
    process::exit(run());
}

#[cfg(test)]
mod tests {
    use super::*;

    fn os(args: &[&str]) -> Vec<OsString> {
        args.iter().map(OsString::from).collect()
    }

    fn full_review_args() -> Vec<OsString> {
        os(&[
            "review",
            "--repo",
            "/repo",
            "--diff",
            "/tmp/mr.diff",
            "--base-ref",
            "main",
            "--pr-url",
            "https://github.com/example/project/pull/1",
            "--out-findings",
            "/tmp/findings.json",
            "--out-decisions",
            "/tmp/decisions.json",
            "--out-pr-agent",
            "/tmp/pr-agent.md",
        ])
    }

    #[test]
    fn help_is_a_request() {
        assert_eq!(parse_cli(os(&["--help"])), Ok(CliRequest::Help));
        assert_eq!(parse_cli(os(&["review", "--help"])), Ok(CliRequest::Help));
    }

    #[test]
    fn parses_a_full_review_command() {
        let Ok(CliRequest::Review(args)) = parse_cli(full_review_args()) else {
            panic!("expected a review request");
        };
        assert_eq!(args.repo, PathBuf::from("/repo"));
        assert_eq!(args.diff, PathBuf::from("/tmp/mr.diff"));
        assert_eq!(args.base_ref, "main");
        assert_eq!(args.pr_url, "https://github.com/example/project/pull/1");
        assert_eq!(args.config, None);
        assert_eq!(args.ticket_file, None);
        assert_eq!(args.out_findings, PathBuf::from("/tmp/findings.json"));
        assert_eq!(args.out_decisions, PathBuf::from("/tmp/decisions.json"));
        assert_eq!(args.out_pr_agent, PathBuf::from("/tmp/pr-agent.md"));
    }

    #[test]
    fn missing_required_flag_is_an_error() {
        let mut args = full_review_args();
        let position = args
            .iter()
            .position(|arg| arg == "--diff")
            .expect("fixture has --diff");
        args.drain(position..position + 2);
        let error = parse_cli(args).expect_err("missing --diff must fail");
        assert!(
            error.contains("missing required argument `--diff`"),
            "{error}"
        );
    }

    #[test]
    fn unknown_and_duplicate_flags_are_errors() {
        let mut unknown = full_review_args();
        unknown.push(OsString::from("--bogus"));
        unknown.push(OsString::from("x"));
        assert!(parse_cli(unknown)
            .expect_err("unknown flag must fail")
            .contains("unknown argument `--bogus`"));

        let mut duplicate = full_review_args();
        duplicate.push(OsString::from("--repo"));
        duplicate.push(OsString::from("/other"));
        assert!(parse_cli(duplicate)
            .expect_err("duplicate flag must fail")
            .contains("duplicate argument `--repo`"));
    }
}
