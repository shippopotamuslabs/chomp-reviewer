//! Chomp: a repo-aware agentic code-review host built on `agent_core`.
//!
//! This binary is a host for `agent_core`, an owner of repository tools, and
//! nothing else. FRG-15 scaffolded the CLI, configuration loading, and
//! output contract; FRG-19/FRG-21/FRG-22 run the three lens sessions end
//! to end; FRG-20 resolves the contract entry file from the base branch;
//! FRG-23 adapts the PR-Agent subprocess. FRG-24 fans the three lenses
//! and the adapter out concurrently and records every producer's
//! outcome in the decision log. Synthesis lands in its own ticket
//! (FRG-26).

mod contract;
mod fan_out;
mod host;
mod outputs;
mod pr_agent;
mod sessions;
mod synthesis;

use std::env;
use std::ffi::OsString;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::process;
use std::sync::Arc;
use std::time::{Duration, Instant};

use agent_core::session::AgentConfig;

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
    // Composition root: configuration, snapshot, executor, and
    // transport are constructed here and passed down; the session
    // layer depends on the ports, never on these concrete types
    // beyond construction.
    let config = contract::load_config(args.config.as_deref(), &args.repo)?;
    validate_inputs(args)?;
    let diff_text = std::fs::read_to_string(&args.diff)
        .map_err(|error| format!("cannot read diff file {}: {error}", args.diff.display()))?;
    let snapshot = chomp::snapshot::Snapshot::capture(&args.repo, &args.diff)
        .map_err(|error| error.to_string())?;
    // Contract resolution (FRG-20): the governing entry copy comes
    // from the base branch, behind its own channel. A bad base ref
    // is an orchestration failure; an absent entry file is not — it
    // resolves to a recorded stand-down and the review proceeds.
    let resolution = contract::resolve_contract(&args.repo, &args.base_ref, &config)
        .map_err(|error| error.to_string())?;
    let mut decision_entries: Vec<serde_json::Value> = resolution
        .recorded_event()
        .map(|(event, detail)| outputs::host_event("contract", event, &detail))
        .into_iter()
        .collect();
    if !config.deep_review {
        decision_entries.push(outputs::host_event(
            "config",
            "deep_review_ignored",
            "reduced mode requested (deep_review = false); not available in Forge 2.0 v1, full fan-out ran",
        ));
    }
    let executor: Arc<dyn chomp::tools::ToolExecutor + Send + Sync> = Arc::new(
        chomp::tools::RepoToolExecutor::new(snapshot.root.clone(), snapshot.head_sha.clone())
            .map_err(|error| {
                format!(
                    "cannot open tool executor at {}: {error}",
                    snapshot.root.display()
                )
            })?,
    );
    let transport: Arc<chomp::transport::OpenRouterTransport> = Arc::new(
        chomp::transport::OpenRouterTransport::from_env().map_err(|error| error.to_string())?,
    );
    decision_entries.push(outputs::host_event(
        "run",
        "fan_out",
        "one AgentEngine per lens session, constructed inside its driver thread; one shared model transport and tool executor across driver threads",
    ));
    let timeouts = fan_out::StageTimeouts::from_config(&config);
    let snapshot = Arc::new(snapshot);
    let pr_agent_request = pr_agent::PrAgentRequest {
        pr_url: args.pr_url.clone(),
        context_files: config.pr_agent_context_files.clone(),
        repo_map: snapshot.repo_map.render(),
        entry_max_lines: config.entry_max_lines,
        timeout: timeouts.pr_agent,
    };
    let pr_agent_runner: fan_out::PrAgentRunner = Arc::new(pr_agent::run_pr_agent);
    // Synthesis inputs captured before the fan-out move: the rendered
    // repo map, and clones of the diff, contract, and executor the
    // synthesizer session needs after the producers report. The
    // follow-up timeout is the PR-Agent stage budget.
    let synthesis_repo_map = snapshot.repo_map.render();
    let followup_timeout = timeouts.pr_agent;
    let result = fan_out::run_fan_out(fan_out::FanOutInputs {
        snapshot,
        diff_text: diff_text.clone(),
        contract: resolution.clone(),
        executor: executor.clone(),
        transports: fan_out::LensTransports {
            correctness: transport.clone(),
            structure: transport.clone(),
            contract: transport.clone(),
        },
        agent_config: AgentConfig::default(),
        pr_agent_request,
        pr_agent_runner,
        timeouts,
    });
    for record in &result.records {
        decision_entries.push(stage_entry(record));
    }
    let combined: Vec<outputs::Finding> = result
        .lens_findings
        .iter()
        .flat_map(|(_, findings)| findings.clone())
        .collect();
    // Deterministic pre-dedup (FRG-25, proposal §8.8): the combined
    // producer findings are clustered for the synthesizer below.
    // Annotation only — every member reaches the session.
    let clustered = synthesis::cluster_findings(&combined);
    // Synthesis (FRG-26, proposal §8.8): the synthesizer adjudicates
    // the clustered findings with repository tools and emits the
    // final findings plus one decision entry per producer finding.
    let ticket_text = synthesis::read_ticket_text(args.ticket_file.as_deref());
    let synthesis_inputs = synthesis::SynthesisInputs {
        clustered: &clustered,
        pr_agent_guide: result.pr_agent_json.as_deref(),
        ticket_text: ticket_text.as_deref(),
        diff_text: &diff_text,
        repo_map: &synthesis_repo_map,
        contract: &resolution,
    };
    let synthesis_deadline = Instant::now() + Duration::from_secs(config.synthesis_timeout_secs);
    // The synthesizer's tool router (FRG-27): the shared read executor
    // plus the capped PR-Agent follow-up runner. The router is the
    // session's executor; its invocation log drains into the decision
    // log after the outcome match, on every path.
    let followup_runner: synthesis::FollowupRunner = Arc::new(pr_agent::run_pr_agent_followup);
    let followup_tools = Arc::new(synthesis::SynthesisTools::new(
        executor.clone(),
        followup_runner,
        pr_agent::PrAgentRequest {
            pr_url: args.pr_url.clone(),
            context_files: config.pr_agent_context_files.clone(),
            repo_map: synthesis_repo_map.clone(),
            entry_max_lines: config.entry_max_lines,
            timeout: followup_timeout,
        },
        config.pr_agent_followups,
    ));
    let followup_executor: Arc<dyn chomp::tools::ToolExecutor + Send + Sync> =
        followup_tools.clone();
    let synthesis_error = match synthesis::run_synthesis_session(
        &synthesis_inputs,
        transport.as_ref(),
        &followup_executor,
        Some(synthesis_deadline),
    ) {
        synthesis::SynthesisOutcome::Completed {
            findings,
            summary,
            conformance,
            decisions,
        } => {
            decision_entries.push(outputs::host_event(
                "synthesis",
                "completed",
                &format!(
                    "{} final finding(s) with {} reconciled disposition(s)",
                    findings.len(),
                    decisions.len()
                ),
            ));
            decision_entries.push(outputs::host_event("synthesis", "summary", &summary));
            if let Some(conformance) = &conformance {
                decision_entries.push(outputs::synthesis_conformance(conformance));
            }
            for decision in &decisions {
                decision_entries.push(outputs::synthesis_disposition(decision));
            }
            outputs::write_findings(&findings, &args.out_findings)?;
            None
        }
        synthesis::SynthesisOutcome::BudgetExhausted { reason } => {
            // The recorded fallback: a worse review is posted, never
            // no review. The union is deterministic (FRG-25 tags
            // stripped in order) and the log says what fired and why.
            let (union, entry) = synthesis::synthesis_fallback(&clustered, &reason);
            decision_entries.push(entry);
            outputs::write_findings(&union, &args.out_findings)?;
            None
        }
        synthesis::SynthesisOutcome::ProducerFailure { reason }
        | synthesis::SynthesisOutcome::InvalidOutput { reason }
        | synthesis::SynthesisOutcome::TimedOut { reason } => {
            // A broken synthesizer is a visible orchestration failure
            // (proposal principle 7): the outage is recorded and the
            // empty findings file is explicit, never disguised.
            decision_entries.push(outputs::host_event("synthesis", "outage", &reason));
            outputs::write_findings(&[], &args.out_findings)?;
            Some(reason)
        }
    };
    for (stage, findings) in &result.lens_findings {
        outputs::write_findings(
            findings,
            &outputs::lens_findings_path(&args.out_findings, stage.as_str()),
        )?;
    }
    outputs::write_decision_log(&decision_entries, &args.out_decisions)?;
    outputs::write_pr_agent(result.pr_agent_json.as_deref(), &args.out_pr_agent)?;
    if !result.any_ran_completed {
        let failed: Vec<String> = result
            .records
            .iter()
            .filter(|record| !record.completed)
            .map(|record| {
                format!(
                    "{} ({})",
                    record.stage.as_str(),
                    record
                        .mode
                        .map(fan_out::FailureMode::as_str)
                        .unwrap_or("unknown")
                )
            })
            .collect();
        return Err(format!(
            "all review producers failed: {}",
            failed.join(", ")
        ));
    }
    for followup in followup_tools.take_followups() {
        decision_entries.push(outputs::pr_agent_followup(&followup));
    }
    if let Some(reason) = synthesis_error {
        return Err(format!("synthesis failed: {reason}"));
    }
    Ok(())
}

/// One stage record as a decision-log entry (plan D4): the run's
/// per-producer account — completion or outage with its failure
/// mode, stand-down note, finding count, and host-side timing.
fn stage_entry(record: &fan_out::StageRecord) -> serde_json::Value {
    serde_json::json!({
        "kind": "stage",
        "stage": record.stage.as_str(),
        "status": if record.completed { "completed" } else { "producer_outage" },
        "mode": record.mode.map(fan_out::FailureMode::as_str),
        "detail": record.detail,
        "note": record.note,
        "findings": record.findings,
        "started_offset_ms": record.started_offset_ms as u64,
        "elapsed_ms": record.elapsed_ms as u64,
    })
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
