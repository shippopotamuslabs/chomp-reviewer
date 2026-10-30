//! Parallel fan-out orchestration over the three lens sessions and the
//! PR-Agent adapter (FRG-24, Chunk 4).
//!
//! After snapshot and contract resolution, four producers run
//! concurrently — one detached thread each (plan D1) — and the host
//! collects one [`StageRecord`] per producer in canonical [`Stage`]
//! order against absolute deadlines. A lens that misses its deadline
//! is recorded [`FailureMode::TimedOut`]; the PR-Agent collection
//! deadline carries a grace period past its stage timeout (plan D9)
//! because the adapter can spend a few seconds reaping a killed child
//! before returning its own typed `TimedOut`.
//!
//! The PR-Agent producer runs through an injected [`PrAgentRunner`]
//! seam rather than calling the adapter directly: adapter failures
//! depend on process-global environment variables, so calling the
//! real adapter here would race with `pr_agent.rs`' own
//! environment-mutating tests in the same test binary. The
//! composition root (Chunk 5 `main`) passes a runner that calls
//! `pr_agent::run_pr_agent`; the adapter itself is covered by its own
//! tests in `pr_agent.rs`.
//!
//! Wiring into `main`'s review flow landed in Chunk 5; the unit
//! tests still drive this module directly through injected runners.

use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

use agent_core::session::AgentConfig;
use chomp::model_transport::ModelTransport;
use chomp::snapshot::Snapshot;
use chomp::tools::ToolExecutor;

use crate::contract::{ContractResolution, ForgeConfig};
use crate::outputs::Finding;
use crate::pr_agent::{PrAgentOutcome, PrAgentRequest};
use crate::sessions::{self, LensOutcome};

/// Extra collection wait past the PR-Agent stage timeout (plan D9):
/// the adapter kills its child at the stage timeout but can spend up
/// to ~5 s in pipe-reader joins before returning its typed `TimedOut`
/// (pr_agent.rs), and that typed reason is worth recording.
const PR_AGENT_COLLECTION_GRACE: Duration = Duration::from_secs(10);

/// A fan-out producer, in canonical collection order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum Stage {
    Correctness,
    Structure,
    Contract,
    PrAgent,
}

impl Stage {
    /// Canonical stage order: lenses first, then PR-Agent.
    pub(crate) const ALL: [Stage; 4] = [
        Stage::Correctness,
        Stage::Structure,
        Stage::Contract,
        Stage::PrAgent,
    ];

    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Stage::Correctness => "correctness",
            Stage::Structure => "structure",
            Stage::Contract => "contract",
            Stage::PrAgent => "pr_agent",
        }
    }
}

/// How a producer failed. Recorded in the decision log (plan D3/D4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FailureMode {
    ProducerError,
    BudgetExhausted,
    InvalidOutput,
    TimedOut,
    Crashed,
}

impl FailureMode {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            FailureMode::ProducerError => "producer_error",
            FailureMode::BudgetExhausted => "budget_exhausted",
            FailureMode::InvalidOutput => "invalid_output",
            FailureMode::TimedOut => "timed_out",
            FailureMode::Crashed => "crashed",
        }
    }
}

/// Per-stage budgets. `synthesis` is reserved for FRG-26: carried
/// here so the timeout framework lands whole (plan D7), unused by any
/// stage in this ticket.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct StageTimeouts {
    pub lens: Duration,
    pub pr_agent: Duration,
    pub synthesis: Duration,
}

impl StageTimeouts {
    pub(crate) fn from_config(config: &ForgeConfig) -> Self {
        Self {
            lens: Duration::from_secs(config.lens_timeout_secs),
            pr_agent: Duration::from_secs(config.pr_agent_timeout_secs),
            synthesis: Duration::from_secs(config.synthesis_timeout_secs),
        }
    }
}

/// One model transport per lens (plan D8): separate slots so tests
/// can script each lens independently — a single shared scripted
/// queue would interleave nondeterministically across threads.
pub(crate) struct LensTransports {
    pub correctness: Arc<dyn ModelTransport + Send + Sync>,
    pub structure: Arc<dyn ModelTransport + Send + Sync>,
    pub contract: Arc<dyn ModelTransport + Send + Sync>,
}

/// The PR-Agent producer seam: the composition root injects the
/// runner; Chunk 5 passes one calling `pr_agent::run_pr_agent`.
pub(crate) type PrAgentRunner = Arc<dyn Fn(&PrAgentRequest) -> PrAgentOutcome + Send + Sync>;

/// Everything one fan-out run needs. Every thread moves owned clones
/// (plan D8); nothing is borrowed across the spawn boundary.
pub(crate) struct FanOutInputs {
    pub snapshot: Arc<Snapshot>,
    pub diff_text: String,
    pub contract: ContractResolution,
    pub executor: Arc<dyn ToolExecutor + Send + Sync>,
    pub transports: LensTransports,
    pub agent_config: AgentConfig,
    pub pr_agent_request: PrAgentRequest,
    pub pr_agent_runner: PrAgentRunner,
    pub timeouts: StageTimeouts,
}

/// One producer's recorded outcome (plan D4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct StageRecord {
    pub stage: Stage,
    pub completed: bool,
    pub mode: Option<FailureMode>,
    pub detail: String,
    pub note: Option<String>,
    pub findings: Option<usize>,
    pub started_offset_ms: u128,
    pub elapsed_ms: u128,
}

/// The fan-out result for `main`: stage records in canonical order,
/// completed lenses' findings in canonical lens order, the completed
/// PR-Agent JSON if any, and the plan-D5 ran-completed fact.
pub(crate) struct FanOutResult {
    pub records: Vec<StageRecord>,
    pub lens_findings: Vec<(Stage, Vec<Finding>)>,
    pub pr_agent_json: Option<String>,
    pub any_ran_completed: bool,
}

/// Run the four producers concurrently and collect one record each.
///
/// One detached thread per producer (plan D1): they are never joined,
/// so a hung producer cannot block collection — the host abandons it
/// at its absolute deadline and the abandoned thread's eventual
/// report lands on a dropped receiver and is silently ignored.
pub(crate) fn run_fan_out(inputs: FanOutInputs) -> FanOutResult {
    let origin = Instant::now();
    // Plan D5: a completed contract stage counts as ran only when the
    // resolution actually drove a session (governing). The absent
    // short-circuit never ran, so it cannot rescue an otherwise
    // all-failed run.
    let contract_ran = matches!(inputs.contract, ContractResolution::Governing { .. });
    let lens_timeout = inputs.timeouts.lens;
    let lens_deadline = origin + lens_timeout;
    let pr_deadline = origin + inputs.timeouts.pr_agent + PR_AGENT_COLLECTION_GRACE;

    let correctness = spawn_lens(
        Stage::Correctness,
        sessions::run_correctness_session,
        &inputs,
        inputs.transports.correctness.clone(),
        lens_deadline,
    );
    let structure = spawn_lens(
        Stage::Structure,
        sessions::run_structure_session,
        &inputs,
        inputs.transports.structure.clone(),
        lens_deadline,
    );
    let contract = spawn_lens(
        Stage::Contract,
        sessions::run_contract_session,
        &inputs,
        inputs.transports.contract.clone(),
        lens_deadline,
    );
    let pr_agent = spawn_pr_agent(&inputs);

    let mut records = Vec::with_capacity(Stage::ALL.len());
    let mut lens_findings = Vec::new();
    for (stage, started, rx) in [correctness, structure, contract] {
        let (record, findings) =
            collect_lens(stage, origin, started, rx, lens_deadline, lens_timeout);
        if let Some(findings) = findings {
            lens_findings.push((stage, findings));
        }
        records.push(record);
    }
    let (pr_record, json) = collect_pr_agent(origin, pr_agent, pr_deadline, inputs.timeouts);
    records.push(pr_record);

    let any_ran_completed = records
        .iter()
        .any(|record| record.completed && (record.stage != Stage::Contract || contract_ran));
    FanOutResult {
        records,
        lens_findings,
        pr_agent_json: json,
        any_ran_completed,
    }
}

/// One lens runner's shape: the three public `sessions` runners share
/// it, so the spawn helper takes a function pointer.
type LensRunner = fn(
    &Snapshot,
    &str,
    &ContractResolution,
    &dyn ModelTransport,
    &Arc<dyn ToolExecutor + Send + Sync>,
    AgentConfig,
    Option<Instant>,
) -> LensOutcome;

/// Spawn one lens producer thread. Returns the host-side start time
/// (taken when the spawn returns) and the outcome receiver.
///
/// If the builder itself fails, the closure — and its sender — drops
/// with the error, so collection sees a disconnect and records
/// `crashed`: a spawn failure is never silent.
fn spawn_lens(
    stage: Stage,
    runner: LensRunner,
    inputs: &FanOutInputs,
    transport: Arc<dyn ModelTransport + Send + Sync>,
    deadline: Instant,
) -> (Stage, Instant, mpsc::Receiver<LensOutcome>) {
    let (tx, rx) = mpsc::channel();
    let snapshot = inputs.snapshot.clone();
    let diff_text = inputs.diff_text.clone();
    let contract = inputs.contract.clone();
    let executor = inputs.executor.clone();
    let agent_config = inputs.agent_config.clone();
    let _detached = std::thread::Builder::new()
        .name(format!("forge-{}", stage.as_str()))
        .spawn(move || {
            let outcome = runner(
                &snapshot,
                &diff_text,
                &contract,
                &*transport,
                &executor,
                agent_config,
                Some(deadline),
            );
            // The host may have abandoned this stage at its deadline;
            // a failed send then is the expected case, not an error.
            let _ = tx.send(outcome);
        });
    // The detached handle drops here by design (plan D1): the host
    // never joins a producer.
    (stage, Instant::now(), rx)
}

/// Spawn the PR-Agent producer thread through the injected runner.
fn spawn_pr_agent(inputs: &FanOutInputs) -> (Instant, mpsc::Receiver<PrAgentOutcome>) {
    let (tx, rx) = mpsc::channel();
    let request = inputs.pr_agent_request.clone();
    let runner = inputs.pr_agent_runner.clone();
    let _detached = std::thread::Builder::new()
        .name(format!("forge-{}", Stage::PrAgent.as_str()))
        .spawn(move || {
            let outcome = runner(&request);
            let _ = tx.send(outcome);
        });
    (Instant::now(), rx)
}

/// What collection of one lens found: a report, a missed deadline, or
/// a dead producer.
enum LensCollection {
    Reported(LensOutcome),
    TimedOut,
    Crashed,
}

/// Collect one lens against its absolute deadline.
fn collect_lens(
    stage: Stage,
    origin: Instant,
    started: Instant,
    rx: mpsc::Receiver<LensOutcome>,
    deadline: Instant,
    timeout: Duration,
) -> (StageRecord, Option<Vec<Finding>>) {
    let started_offset_ms = started.duration_since(origin).as_millis();
    let collected = match rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
        Ok(outcome) => LensCollection::Reported(outcome),
        Err(mpsc::RecvTimeoutError::Timeout) => LensCollection::TimedOut,
        Err(mpsc::RecvTimeoutError::Disconnected) => LensCollection::Crashed,
    };
    let elapsed_ms = Instant::now().duration_since(started).as_millis();
    match collected {
        LensCollection::Reported(outcome) => {
            let converted = convert_lens_outcome(outcome);
            let record = StageRecord {
                stage,
                completed: converted.completed,
                mode: converted.mode,
                detail: converted.detail,
                note: converted.note,
                findings: converted.completed.then_some(converted.findings.len()),
                started_offset_ms,
                elapsed_ms,
            };
            let kept = converted.completed.then_some(converted.findings);
            (record, kept)
        }
        LensCollection::TimedOut => (
            StageRecord {
                stage,
                completed: false,
                mode: Some(FailureMode::TimedOut),
                detail: format!(
                    "{} stage timed out after {} with no report; its in-flight work was abandoned at the deadline",
                    stage.as_str(),
                    fmt_timeout(timeout),
                ),
                note: None,
                findings: None,
                started_offset_ms,
                elapsed_ms,
            },
            None,
        ),
        LensCollection::Crashed => (
            StageRecord {
                stage,
                completed: false,
                mode: Some(FailureMode::Crashed),
                detail: format!(
                    "{} producer thread crashed before reporting (channel disconnected)",
                    stage.as_str(),
                ),
                note: None,
                findings: None,
                started_offset_ms,
                elapsed_ms,
            },
            None,
        ),
    }
}

/// Collect the PR-Agent producer against its absolute deadline
/// (stage timeout plus the D9 grace period).
fn collect_pr_agent(
    origin: Instant,
    spawned: (Instant, mpsc::Receiver<PrAgentOutcome>),
    deadline: Instant,
    timeouts: StageTimeouts,
) -> (StageRecord, Option<String>) {
    let (started, rx) = spawned;
    let stage = Stage::PrAgent;
    let started_offset_ms = started.duration_since(origin).as_millis();
    let collected = match rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
        Ok(outcome) => Some(outcome),
        Err(mpsc::RecvTimeoutError::Timeout) => None,
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            return (
                StageRecord {
                    stage,
                    completed: false,
                    mode: Some(FailureMode::Crashed),
                    detail: format!(
                        "{} producer thread crashed before reporting (channel disconnected)",
                        stage.as_str(),
                    ),
                    note: None,
                    findings: None,
                    started_offset_ms,
                    elapsed_ms: Instant::now().duration_since(started).as_millis(),
                },
                None,
            );
        }
    };
    let elapsed_ms = Instant::now().duration_since(started).as_millis();
    match collected {
        Some(outcome) => {
            let converted = convert_pr_agent_outcome(outcome);
            (
                StageRecord {
                    stage,
                    completed: converted.completed,
                    mode: converted.mode,
                    detail: converted.detail,
                    note: None,
                    findings: None,
                    started_offset_ms,
                    elapsed_ms,
                },
                converted.json,
            )
        }
        None => (
            StageRecord {
                stage,
                completed: false,
                mode: Some(FailureMode::TimedOut),
                detail: format!(
                    "{} stage timed out after {} with no report; its in-flight work was abandoned at the deadline",
                    stage.as_str(),
                    fmt_timeout(timeouts.pr_agent),
                ),
                note: None,
                findings: None,
                started_offset_ms,
                elapsed_ms,
            },
            None,
        ),
    }
}

/// A converted lens outcome: the record fields plus the findings to
/// keep when the stage completed.
struct ConvertedLens {
    completed: bool,
    mode: Option<FailureMode>,
    detail: String,
    note: Option<String>,
    findings: Vec<Finding>,
}

fn convert_lens_outcome(outcome: LensOutcome) -> ConvertedLens {
    match outcome {
        LensOutcome::Completed { findings, note } => {
            let detail = match findings.len() {
                0 => "no findings".to_string(),
                1 => "1 finding".to_string(),
                count => format!("{count} findings"),
            };
            ConvertedLens {
                completed: true,
                mode: None,
                detail,
                note,
                findings,
            }
        }
        LensOutcome::ProducerFailure { reason } => ConvertedLens {
            completed: false,
            mode: Some(FailureMode::ProducerError),
            detail: reason,
            note: None,
            findings: Vec::new(),
        },
        LensOutcome::BudgetExhausted { reason } => ConvertedLens {
            completed: false,
            mode: Some(FailureMode::BudgetExhausted),
            detail: reason,
            note: None,
            findings: Vec::new(),
        },
        LensOutcome::InvalidOutput { reason } => ConvertedLens {
            completed: false,
            mode: Some(FailureMode::InvalidOutput),
            detail: reason,
            note: None,
            findings: Vec::new(),
        },
        LensOutcome::TimedOut { reason } => ConvertedLens {
            completed: false,
            mode: Some(FailureMode::TimedOut),
            detail: reason,
            note: None,
            findings: Vec::new(),
        },
    }
}

/// A converted PR-Agent outcome: the record fields plus the review
/// JSON to keep when the stage completed.
struct ConvertedPrAgent {
    completed: bool,
    mode: Option<FailureMode>,
    detail: String,
    json: Option<String>,
}

fn convert_pr_agent_outcome(outcome: PrAgentOutcome) -> ConvertedPrAgent {
    match outcome {
        PrAgentOutcome::Completed { json } => ConvertedPrAgent {
            completed: true,
            mode: None,
            detail: "PR-Agent review captured".to_string(),
            json: Some(json),
        },
        PrAgentOutcome::ProducerFailure { reason } => ConvertedPrAgent {
            completed: false,
            mode: Some(FailureMode::ProducerError),
            detail: reason,
            json: None,
        },
        PrAgentOutcome::InvalidOutput { reason } => ConvertedPrAgent {
            completed: false,
            mode: Some(FailureMode::InvalidOutput),
            detail: reason,
            json: None,
        },
        PrAgentOutcome::TimedOut { reason } => ConvertedPrAgent {
            completed: false,
            mode: Some(FailureMode::TimedOut),
            detail: reason,
            json: None,
        },
    }
}

/// Person-readable timeout rendering for stage records. This text is
/// written to be quoted under a ⚠️ line on the PR (plan D4/WUMBO),
/// so it names the stage and the budget in plain words.
fn fmt_timeout(timeout: Duration) -> String {
    if timeout < Duration::from_secs(1) {
        format!("{}ms", timeout.as_millis())
    } else if timeout.subsec_millis() == 0 {
        format!("{}s", timeout.as_secs())
    } else {
        format!("{}.{:03}s", timeout.as_secs(), timeout.subsec_millis())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::fs;
    use std::path::PathBuf;
    use std::process::Command;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::Mutex;

    use agent_core::turn_io::{FinishReason, ModelRequest, ModelResponse};
    use chomp::model_transport::TransportError;
    use chomp::tools::RepoToolExecutor;

    use crate::contract::AbsentReason;

    static FIXTURE_SEQ: AtomicU64 = AtomicU64::new(0);

    // -- transport fixtures ---------------------------------------------------

    fn text_response(text: &str) -> ModelResponse {
        ModelResponse {
            assistant_text: Some(text.to_string()),
            tool_calls: Vec::new(),
            finish_reason: FinishReason::Stop,
            provider_reasoning_content: None,
        }
    }

    /// One valid envelope finding tagged per lens, so a cross-lens
    /// interleave would show up as the wrong file/title on a stage.
    fn lens_envelope(tag: &str) -> String {
        format!(
            concat!(
                "{{\"findings\":[{{\"file\":\"src/{0}.rs\",\"line_start\":5,",
                "\"line_end\":5,\"severity\":\"High\",\"category\":\"Review\",",
                "\"title\":\"{0} lens finding\",",
                "\"description\":\"The {0} lens reports an issue in the changed code.\",",
                "\"evidence\":\"src/app.rs line 2 changed in the diff.\",",
                "\"fix_focus_areas\":[\"src/{0}.rs[5-5]\"],",
                "\"recommended_fix\":\"Address the {0} concern before merging.\",",
                "\"citations\":[\"src/app.rs:2\"]}}]}}"
            ),
            tag
        )
    }

    struct ScriptedTransport {
        script: Mutex<VecDeque<ModelResponse>>,
        fallback: ModelResponse,
    }

    impl ScriptedTransport {
        fn envelope(text: &str) -> Self {
            Self {
                script: VecDeque::new().into(),
                fallback: text_response(text),
            }
        }
    }

    impl ModelTransport for ScriptedTransport {
        fn complete_model_turn(
            &self,
            _request: &ModelRequest,
        ) -> Result<ModelResponse, TransportError> {
            Ok(self
                .script
                .lock()
                .expect("script lock")
                .pop_front()
                .unwrap_or_else(|| self.fallback.clone()))
        }
    }

    struct SleepingTransport {
        delay: Duration,
        text: String,
    }

    impl ModelTransport for SleepingTransport {
        fn complete_model_turn(
            &self,
            _request: &ModelRequest,
        ) -> Result<ModelResponse, TransportError> {
            std::thread::sleep(self.delay);
            Ok(text_response(&self.text))
        }
    }

    struct PanickingTransport;

    impl ModelTransport for PanickingTransport {
        fn complete_model_turn(
            &self,
            _request: &ModelRequest,
        ) -> Result<ModelResponse, TransportError> {
            panic!("scripted lens panic for the crashed-producer scenario");
        }
    }

    struct GarbageTransport;

    impl ModelTransport for GarbageTransport {
        fn complete_model_turn(
            &self,
            _request: &ModelRequest,
        ) -> Result<ModelResponse, TransportError> {
            Ok(text_response(
                "this is plain prose, not a findings envelope",
            ))
        }
    }

    // -- repo fixture (mirrors the sessions/host test construction) -----------

    struct FixtureRepo {
        base: PathBuf,
        repo: PathBuf,
        diff_path: PathBuf,
    }

    impl FixtureRepo {
        fn new(label: &str) -> Self {
            let nonce = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos();
            let seq = FIXTURE_SEQ.fetch_add(1, Ordering::SeqCst);
            let base = std::env::temp_dir().join(format!(
                "chomp-fanout-{label}-{}-{nonce}-{seq}",
                std::process::id()
            ));
            let repo = base.join("repo");
            fs::create_dir_all(repo.join("src")).expect("create fixture repo");
            let fixture = Self {
                diff_path: base.join("mr.diff"),
                base,
                repo,
            };
            fixture.git(&["init", "-q"]);
            fixture.write("AGENTS.md", "# Fixture repo notes\n");
            fixture.write("src/app.rs", "pub fn run() {}\n");
            fixture.commit_all("base");
            fixture.write(
                "src/app.rs",
                "pub fn run() {\n    println!(\"changed\");\n}\n",
            );
            fixture.write_diff();
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

        fn write(&self, rel: &str, contents: &str) {
            let path = self.repo.join(rel);
            fs::create_dir_all(path.parent().expect("parent")).expect("dirs");
            fs::write(&path, contents).expect("write fixture file");
        }

        fn commit_all(&self, message: &str) {
            self.git(&["add", "-A"]);
            self.git(&["commit", "-q", "-m", message]);
        }

        fn write_diff(&self) {
            let output = Command::new("git")
                .arg("-C")
                .arg(&self.repo)
                .args(["diff", "--no-color"])
                .output()
                .expect("git diff runs");
            assert!(output.status.success());
            fs::write(&self.diff_path, &output.stdout).expect("write diff");
        }

        fn snapshot(&self) -> Snapshot {
            Snapshot::capture(&self.repo, &self.diff_path).expect("capture snapshot")
        }

        fn diff_text(&self) -> String {
            fs::read_to_string(&self.diff_path).expect("read diff")
        }
    }

    impl Drop for FixtureRepo {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.base);
        }
    }

    // -- input builders --------------------------------------------------------

    fn governing_contract() -> ContractResolution {
        ContractResolution::Governing {
            entry_file: "AGENTS.md".to_string(),
            content: "# Fixture repo notes\n".to_string(),
            truncated_from_lines: None,
        }
    }

    fn absent_contract() -> ContractResolution {
        ContractResolution::Absent {
            entry_file: "AGENTS.md".to_string(),
            reason: AbsentReason::Missing,
        }
    }

    fn pr_request() -> PrAgentRequest {
        PrAgentRequest {
            pr_url: "https://example.com/pr/1".to_string(),
            context_files: Vec::new(),
            repo_map: String::new(),
            entry_max_lines: 2_000,
            timeout: Duration::from_secs(60),
        }
    }

    fn generous_timeouts() -> StageTimeouts {
        StageTimeouts {
            lens: Duration::from_secs(60),
            pr_agent: Duration::from_secs(60),
            synthesis: Duration::from_secs(60),
        }
    }

    fn failing_runner(reason: &str) -> PrAgentRunner {
        let reason = reason.to_string();
        Arc::new(move |_: &PrAgentRequest| PrAgentOutcome::ProducerFailure {
            reason: reason.clone(),
        })
    }

    fn completed_runner(json: &str) -> PrAgentRunner {
        let json = json.to_string();
        Arc::new(move |_: &PrAgentRequest| PrAgentOutcome::Completed { json: json.clone() })
    }

    #[allow(clippy::too_many_arguments)]
    fn run_with(
        fixture: &FixtureRepo,
        contract: ContractResolution,
        correctness: Arc<dyn ModelTransport + Send + Sync>,
        structure: Arc<dyn ModelTransport + Send + Sync>,
        contract_transport: Arc<dyn ModelTransport + Send + Sync>,
        runner: PrAgentRunner,
        timeouts: StageTimeouts,
    ) -> FanOutResult {
        let snapshot = Arc::new(fixture.snapshot());
        let executor: Arc<dyn ToolExecutor + Send + Sync> = Arc::new(
            RepoToolExecutor::new(snapshot.root.clone(), snapshot.head_sha.clone())
                .expect("executor"),
        );
        run_fan_out(FanOutInputs {
            snapshot,
            diff_text: fixture.diff_text(),
            contract,
            executor,
            transports: LensTransports {
                correctness,
                structure,
                contract: contract_transport,
            },
            agent_config: AgentConfig::default(),
            pr_agent_request: pr_request(),
            pr_agent_runner: runner,
            timeouts,
        })
    }

    fn record_by_stage(result: &FanOutResult, stage: Stage) -> &StageRecord {
        result
            .records
            .iter()
            .find(|record| record.stage == stage)
            .unwrap_or_else(|| panic!("missing {stage:?} record"))
    }

    fn scripted(text: &str) -> Arc<dyn ModelTransport + Send + Sync> {
        Arc::new(ScriptedTransport::envelope(text))
    }

    // -- scenario (a): healthy lenses plus a failing PR-Agent ------------------

    #[test]
    fn healthy_lenses_complete_with_own_findings_and_pr_failure_is_an_outage() {
        let fixture = FixtureRepo::new("healthy");
        let correctness_text = lens_envelope("correctness");
        let structure_text = lens_envelope("structure");
        let contract_text = lens_envelope("contract");
        let result = run_with(
            &fixture,
            governing_contract(),
            scripted(&correctness_text),
            scripted(&structure_text),
            scripted(&contract_text),
            failing_runner("missing credential: GITHUB__USER_TOKEN"),
            generous_timeouts(),
        );

        assert_eq!(
            result
                .records
                .iter()
                .map(|record| record.stage)
                .collect::<Vec<_>>(),
            Stage::ALL,
            "records stay in canonical order"
        );
        for stage in [Stage::Correctness, Stage::Structure, Stage::Contract] {
            let record = record_by_stage(&result, stage);
            assert!(record.completed, "{stage:?} completes");
            assert_eq!(record.mode, None);
            assert_eq!(record.findings, Some(1));
            assert_eq!(record.note, None);
        }
        let pr = record_by_stage(&result, Stage::PrAgent);
        assert!(!pr.completed);
        assert_eq!(pr.mode, Some(FailureMode::ProducerError));
        assert!(pr.detail.contains("GITHUB__USER_TOKEN"), "{}", pr.detail);

        assert_eq!(result.lens_findings.len(), 3);
        for (stage, findings) in &result.lens_findings {
            assert_eq!(findings.len(), 1);
            assert!(
                findings[0].title.contains(stage.as_str()),
                "each lens keeps its own findings, got {} for {stage:?}",
                findings[0].title
            );
        }
        assert_eq!(result.pr_agent_json, None);
        assert!(result.any_ran_completed);
    }

    // -- scenario (b): overlapping intervals prove concurrency -----------------

    #[test]
    fn slow_producers_overlap_and_finish_well_under_serial() {
        let fixture = FixtureRepo::new("overlap");
        let delay = Duration::from_millis(300);
        let sleeping = |tag: &str| -> Arc<dyn ModelTransport + Send + Sync> {
            Arc::new(SleepingTransport {
                delay,
                text: lens_envelope(tag),
            })
        };
        let runner: PrAgentRunner = Arc::new(move |_: &PrAgentRequest| {
            std::thread::sleep(delay);
            PrAgentOutcome::Completed {
                json: "{\"review\":{} }".to_string(),
            }
        });
        let wall_start = Instant::now();
        let result = run_with(
            &fixture,
            governing_contract(),
            sleeping("correctness"),
            sleeping("structure"),
            sleeping("contract"),
            runner,
            generous_timeouts(),
        );
        let wall = wall_start.elapsed();

        let latest_start = result
            .records
            .iter()
            .map(|record| record.started_offset_ms)
            .max()
            .expect("records");
        let earliest_end = result
            .records
            .iter()
            .map(|record| record.started_offset_ms + record.elapsed_ms)
            .min()
            .expect("records");
        assert!(
            latest_start < earliest_end,
            "intervals overlap: latest start {latest_start}ms, earliest end {earliest_end}ms"
        );
        // Serial execution floors at 4 × 300ms = 1200ms; concurrent
        // producers share that sleep, so even a loaded CI box stays
        // far under the serial floor.
        assert!(
            wall < Duration::from_millis(1000),
            "concurrent producers finish well under serial: {wall:?}"
        );
        assert!(result.records.iter().all(|record| record.completed));
        assert!(result.any_ran_completed);
    }

    // -- scenario (c): a hung lens is cut at its stage timeout ---------------

    #[test]
    fn hung_lens_is_cut_at_its_stage_timeout() {
        let fixture = FixtureRepo::new("hung");
        let timeouts = StageTimeouts {
            lens: Duration::from_millis(500),
            pr_agent: Duration::from_secs(60),
            synthesis: Duration::from_secs(60),
        };
        let wall_start = Instant::now();
        let result = run_with(
            &fixture,
            governing_contract(),
            scripted(&lens_envelope("correctness")),
            Arc::new(SleepingTransport {
                delay: Duration::from_secs(2),
                text: "{\"findings\":[]}".to_string(),
            }),
            scripted(&lens_envelope("contract")),
            failing_runner("missing credential: GITHUB__USER_TOKEN"),
            timeouts,
        );
        let wall = wall_start.elapsed();

        let hung = record_by_stage(&result, Stage::Structure);
        assert!(!hung.completed);
        assert_eq!(hung.mode, Some(FailureMode::TimedOut));
        assert!(hung.detail.contains("structure"), "{}", hung.detail);
        assert!(hung.detail.contains("500ms"), "{}", hung.detail);
        assert!(
            hung.elapsed_ms < 1000,
            "the record closes at the deadline, not at the abandoned work: {}ms",
            hung.elapsed_ms
        );
        // The fan-out returns once the last deadline passes — long
        // before the abandoned 2 s sleep ends.
        assert!(
            wall < Duration::from_millis(1500),
            "fan-out returns well under the abandoned sleep: {wall:?}"
        );
        assert!(record_by_stage(&result, Stage::Correctness).completed);
        assert!(record_by_stage(&result, Stage::Contract).completed);
        assert_eq!(result.lens_findings.len(), 2);
        assert!(result.any_ran_completed);
        // The abandoned thread's eventual report lands on a dropped
        // receiver and is silently ignored — nothing to assert, but
        // the test passing without a hang is the evidence.
    }

    // -- scenario (d): a panicking lens is a crash, others intact ------------

    #[test]
    fn panicking_lens_is_a_crash_and_others_are_intact() {
        let fixture = FixtureRepo::new("panic");
        let result = run_with(
            &fixture,
            governing_contract(),
            scripted(&lens_envelope("correctness")),
            Arc::new(PanickingTransport),
            scripted(&lens_envelope("contract")),
            failing_runner("missing credential: GITHUB__USER_TOKEN"),
            generous_timeouts(),
        );

        let crashed = record_by_stage(&result, Stage::Structure);
        assert!(!crashed.completed);
        assert_eq!(crashed.mode, Some(FailureMode::Crashed));
        assert!(crashed.detail.contains("crashed"), "{}", crashed.detail);
        assert!(record_by_stage(&result, Stage::Correctness).completed);
        assert!(record_by_stage(&result, Stage::Contract).completed);
        assert_eq!(result.lens_findings.len(), 2);
        assert!(result.any_ran_completed);
    }

    // -- scenario (e): garbage output is invalid, others intact --------------

    #[test]
    fn garbage_lens_output_is_invalid_output_and_others_are_intact() {
        let fixture = FixtureRepo::new("garbage");
        let result = run_with(
            &fixture,
            governing_contract(),
            Arc::new(GarbageTransport),
            scripted(&lens_envelope("structure")),
            scripted(&lens_envelope("contract")),
            failing_runner("missing credential: GITHUB__USER_TOKEN"),
            generous_timeouts(),
        );

        let invalid = record_by_stage(&result, Stage::Correctness);
        assert!(!invalid.completed);
        assert_eq!(invalid.mode, Some(FailureMode::InvalidOutput));
        assert!(!invalid.detail.is_empty());
        // A reported failure never carries a findings count: Some(0)
        // is reserved for a completed empty review.
        assert_eq!(invalid.findings, None);
        assert!(record_by_stage(&result, Stage::Structure).completed);
        assert!(record_by_stage(&result, Stage::Contract).completed);
        assert!(result.any_ran_completed);
    }

    // -- scenario (f): direct conversion tests for every variant --------------

    #[test]
    fn lens_conversion_covers_every_outcome_variant() {
        let completed = convert_lens_outcome(LensOutcome::Completed {
            findings: Vec::new(),
            note: None,
        });
        assert!(completed.completed);
        assert_eq!(completed.mode, None);
        assert_eq!(completed.detail, "no findings");

        let one = convert_lens_outcome(LensOutcome::Completed {
            findings: vec![sample_finding()],
            note: Some("a note".to_string()),
        });
        assert!(one.completed);
        assert_eq!(one.detail, "1 finding");
        assert_eq!(one.note, Some("a note".to_string()));
        assert_eq!(one.findings.len(), 1);

        let table = [
            (
                LensOutcome::ProducerFailure {
                    reason: "boom".to_string(),
                },
                FailureMode::ProducerError,
            ),
            (
                LensOutcome::BudgetExhausted {
                    reason: "no turns left".to_string(),
                },
                FailureMode::BudgetExhausted,
            ),
            (
                LensOutcome::InvalidOutput {
                    reason: "not an envelope".to_string(),
                },
                FailureMode::InvalidOutput,
            ),
            (
                LensOutcome::TimedOut {
                    reason: "deadline passed".to_string(),
                },
                FailureMode::TimedOut,
            ),
        ];
        for (outcome, mode) in table {
            let converted = convert_lens_outcome(outcome);
            assert!(!converted.completed);
            assert_eq!(converted.mode, Some(mode));
            assert!(!converted.detail.is_empty());
            assert_eq!(converted.note, None);
            assert!(converted.findings.is_empty());
        }
    }

    #[test]
    fn pr_agent_conversion_covers_every_outcome_variant() {
        let completed = convert_pr_agent_outcome(PrAgentOutcome::Completed {
            json: "{\"review\":{}}".to_string(),
        });
        assert!(completed.completed);
        assert_eq!(completed.mode, None);
        assert_eq!(completed.detail, "PR-Agent review captured");
        assert_eq!(completed.json, Some("{\"review\":{}}".to_string()));

        let table = [
            (
                PrAgentOutcome::ProducerFailure {
                    reason: "no token".to_string(),
                },
                FailureMode::ProducerError,
            ),
            (
                PrAgentOutcome::InvalidOutput {
                    reason: "empty review.json".to_string(),
                },
                FailureMode::InvalidOutput,
            ),
            (
                PrAgentOutcome::TimedOut {
                    reason: "killed".to_string(),
                },
                FailureMode::TimedOut,
            ),
        ];
        for (outcome, mode) in table {
            let converted = convert_pr_agent_outcome(outcome);
            assert!(!converted.completed);
            assert_eq!(converted.mode, Some(mode));
            assert!(!converted.detail.is_empty());
            assert_eq!(converted.json, None);
        }
    }

    #[test]
    fn stage_spelling_and_timeout_mapping() {
        assert_eq!(
            Stage::ALL.map(Stage::as_str),
            ["correctness", "structure", "contract", "pr_agent"]
        );
        assert_eq!(
            [
                FailureMode::ProducerError,
                FailureMode::BudgetExhausted,
                FailureMode::InvalidOutput,
                FailureMode::TimedOut,
                FailureMode::Crashed,
            ]
            .map(FailureMode::as_str),
            [
                "producer_error",
                "budget_exhausted",
                "invalid_output",
                "timed_out",
                "crashed"
            ]
        );
        let timeouts = StageTimeouts::from_config(&ForgeConfig::default());
        assert_eq!(timeouts.lens, Duration::from_secs(600));
        assert_eq!(timeouts.pr_agent, Duration::from_secs(900));
        assert_eq!(timeouts.synthesis, Duration::from_secs(600));
        assert_eq!(fmt_timeout(Duration::from_millis(100)), "100ms");
        assert_eq!(fmt_timeout(Duration::from_secs(10)), "10s");
    }

    fn sample_finding() -> Finding {
        serde_json::from_value(serde_json::json!({
            "file": "src/app.rs",
            "line_start": 2,
            "line_end": 2,
            "severity": "High",
            "category": "Review",
            "title": "sample",
            "description": "a sample finding",
            "evidence": "src/app.rs line 2",
            "fix_focus_areas": ["src/app.rs[2-2]"],
            "recommended_fix": "fix it",
            "citations": ["src/app.rs:2"]
        }))
        .expect("sample finding is valid")
    }

    // -- scenario (g): every ran producer fails -------------------------------

    #[test]
    fn all_ran_producers_failing_means_nothing_ran_completed() {
        let fixture = FixtureRepo::new("all-fail");
        let timeouts = StageTimeouts {
            lens: Duration::from_millis(500),
            pr_agent: Duration::from_secs(60),
            synthesis: Duration::from_secs(60),
        };
        let result = run_with(
            &fixture,
            governing_contract(),
            Arc::new(GarbageTransport),
            Arc::new(PanickingTransport),
            Arc::new(SleepingTransport {
                delay: Duration::from_secs(2),
                text: "{\"findings\":[]}".to_string(),
            }),
            failing_runner("missing credential: GITHUB__USER_TOKEN"),
            timeouts,
        );

        assert_eq!(result.records.len(), 4);
        assert_eq!(
            result
                .records
                .iter()
                .map(|record| record.stage)
                .collect::<Vec<_>>(),
            Stage::ALL
        );
        for record in &result.records {
            assert!(!record.completed, "{:?} is an outage", record.stage);
            assert!(record.mode.is_some(), "{:?} names its mode", record.stage);
        }
        assert_eq!(
            record_by_stage(&result, Stage::Correctness).mode,
            Some(FailureMode::InvalidOutput)
        );
        assert_eq!(
            record_by_stage(&result, Stage::Structure).mode,
            Some(FailureMode::Crashed)
        );
        assert_eq!(
            record_by_stage(&result, Stage::Contract).mode,
            Some(FailureMode::TimedOut)
        );
        assert_eq!(
            record_by_stage(&result, Stage::PrAgent).mode,
            Some(FailureMode::ProducerError)
        );
        assert!(result.lens_findings.is_empty());
        assert_eq!(result.pr_agent_json, None);
        assert!(!result.any_ran_completed);
    }

    // -- scenario (h): contract stand-down accounting --------------------------

    #[test]
    fn governing_contract_empty_completion_counts_as_ran() {
        for noted in [false, true] {
            let fixture = FixtureRepo::new(if noted { "noted" } else { "empty" });
            let contract_text = if noted {
                "{\"findings\":[],\"note\":\"governing copy states nothing checkable\"}".to_string()
            } else {
                "{\"findings\":[]}".to_string()
            };
            let result = run_with(
                &fixture,
                governing_contract(),
                Arc::new(GarbageTransport),
                Arc::new(GarbageTransport),
                scripted(&contract_text),
                failing_runner("missing credential: GITHUB__USER_TOKEN"),
                generous_timeouts(),
            );

            let contract = record_by_stage(&result, Stage::Contract);
            assert!(contract.completed);
            assert_eq!(contract.mode, None);
            assert_eq!(contract.findings, Some(0));
            if noted {
                assert_eq!(
                    contract.note,
                    Some("governing copy states nothing checkable".to_string())
                );
            } else {
                assert_eq!(contract.note, None);
            }
            assert!(result.any_ran_completed, "noted={noted}");
        }
    }

    #[test]
    fn absent_contract_stand_down_does_not_count_as_ran() {
        let fixture = FixtureRepo::new("absent");
        let result = run_with(
            &fixture,
            absent_contract(),
            Arc::new(GarbageTransport),
            Arc::new(GarbageTransport),
            // The transport is never called on the absent path; any
            // script would do, including one that would fail loudly.
            Arc::new(GarbageTransport),
            failing_runner("missing credential: GITHUB__USER_TOKEN"),
            generous_timeouts(),
        );

        let contract = record_by_stage(&result, Stage::Contract);
        assert!(contract.completed);
        assert_eq!(contract.mode, None);
        assert_eq!(contract.findings, Some(0));
        assert_eq!(contract.note, None);
        assert!(!result.any_ran_completed);
    }

    // -- review round 1: reported failures record no findings count --------

    #[test]
    fn reported_lens_failures_record_no_findings_count() {
        let origin = Instant::now();
        let started = Instant::now();
        let deadline = started + Duration::from_secs(60);
        let timeout = Duration::from_secs(60);

        let collect = |outcome: LensOutcome| {
            let (tx, rx) = mpsc::channel();
            tx.send(outcome).expect("send");
            collect_lens(Stage::Correctness, origin, started, rx, deadline, timeout)
        };

        for outcome in [
            LensOutcome::ProducerFailure {
                reason: "boom".to_string(),
            },
            LensOutcome::InvalidOutput {
                reason: "not an envelope".to_string(),
            },
            LensOutcome::BudgetExhausted {
                reason: "no turns left".to_string(),
            },
            LensOutcome::TimedOut {
                reason: "deadline passed".to_string(),
            },
        ] {
            let (record, kept) = collect(outcome);
            assert!(!record.completed);
            assert!(record.mode.is_some());
            assert_eq!(record.findings, None);
            assert_eq!(kept, None);
        }

        // A completed empty review keeps Some(0): the count means
        // "ran and found nothing", distinct from a failure's None.
        let (record, kept) = collect(LensOutcome::Completed {
            findings: Vec::new(),
            note: None,
        });
        assert!(record.completed);
        assert_eq!(record.findings, Some(0));
        assert_eq!(kept, Some(Vec::new()));
    }

    #[test]
    fn host_timed_out_pr_agent_detail_names_the_stage_budget() {
        let origin = Instant::now();
        let timeouts = StageTimeouts {
            lens: Duration::from_secs(60),
            pr_agent: Duration::from_secs(900),
            synthesis: Duration::from_secs(60),
        };
        let (_tx, rx) = mpsc::channel::<PrAgentOutcome>();
        let started = Instant::now();
        let deadline = Instant::now();
        let (record, json) = collect_pr_agent(origin, (started, rx), deadline, timeouts);
        assert!(!record.completed);
        assert_eq!(record.mode, Some(FailureMode::TimedOut));
        assert!(
            record.detail.contains("900s"),
            "detail names the stage budget, got: {}",
            record.detail
        );
        assert!(
            !record.detail.contains("910s"),
            "collection grace must not leak into the budget, got: {}",
            record.detail
        );
        assert_eq!(json, None);
    }

    #[test]
    fn completed_pr_agent_json_is_captured() {
        let fixture = FixtureRepo::new("pr-json");
        let json = "{\"review\":{\"summaries\":[]}}";
        let result = run_with(
            &fixture,
            absent_contract(),
            scripted(&lens_envelope("correctness")),
            scripted(&lens_envelope("structure")),
            // Absent contract stands down without driving a session.
            scripted("{\"findings\":[]}"),
            completed_runner(json),
            generous_timeouts(),
        );

        let pr = record_by_stage(&result, Stage::PrAgent);
        assert!(pr.completed);
        assert_eq!(pr.mode, None);
        assert_eq!(pr.detail, "PR-Agent review captured");
        assert_eq!(pr.findings, None);
        assert_eq!(result.pr_agent_json.as_deref(), Some(json));
        assert!(result.any_ran_completed);
    }
}
