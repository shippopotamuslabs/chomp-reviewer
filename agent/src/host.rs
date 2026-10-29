//! Host driver for `agent_core` sessions.
//!
//! `agent_core` is pull-based: the host creates a session and repeatedly
//! calls `next_action`, performs the requested transport, tool, or
//! summarization work, and feeds the result back into the session. There
//! are no host callback traits to implement (the callback model was
//! removed from `agent_core` by ENC-285).
//!
//! [`drive_session`] is that pull loop (FRG-19). Model turns go through
//! the application port ([`ModelTransport`]), never a concrete
//! transport; transport failures are fed back into the session with the
//! port error's failure report, so the engine owns retry decisions.
//! Tool calls go through the FRG-18 [`ToolExecutor`]. Lens sessions run
//! with summarization disabled, so a `RequestSummary` action is a
//! driver-level failure, never a silent summarization of evidence.

use std::sync::Arc;

use agent_core::action::AgentAction;
use agent_core::engine::{AgentEngine, SessionHandle};
use agent_core::events::AgentEvent;
use chomp::model_transport::ModelTransport;
use chomp::tools::ToolExecutor;

/// Defensive cap on pull-loop iterations. Engine budgets
/// (`AgentConfig` turn and retry limits) bind orders of magnitude
/// earlier; the cap exists so a misbehaving session can never hang
/// the host.
const MAX_DRIVE_STEPS: usize = 1024;

/// How a driven session ended.
#[derive(Debug, PartialEq, Eq)]
pub enum DriveOutcome {
    /// The engine reached its `Finished` action. `budget_signaled` is
    /// true when the engine's event stream shows a turn started with
    /// tool calls disallowed — its turn-budget enforcement — which
    /// callers use to tell a budget-exhausted finish (the engine's
    /// fallback text instead of session output) from a real one.
    Finished { budget_signaled: bool },
    /// The engine failed the session, or the driver itself refused an
    /// action. `reason` is the rendered failure.
    Failed {
        reason: String,
        budget_signaled: bool,
    },
}

/// Drive one session to a terminal action, servicing model and tool
/// requests through the given port and executor.
pub fn drive_session(
    engine: &AgentEngine,
    handle: &SessionHandle,
    transport: &dyn ModelTransport,
    executor: &Arc<dyn ToolExecutor + Send + Sync>,
) -> DriveOutcome {
    for _ in 0..MAX_DRIVE_STEPS {
        match engine.next_action(handle) {
            AgentAction::CallModel(request) => match transport.complete_model_turn(&request) {
                Ok(response) => engine.submit_model_response(handle, response),
                Err(error) => engine.submit_model_turn_failure(handle, error.to_failure_report()),
            },
            AgentAction::ExecuteTools(calls) => {
                let results = calls.iter().map(|call| executor.execute(call)).collect();
                engine.submit_tool_results(handle, results);
            }
            AgentAction::RequestSummary(_) => {
                return DriveOutcome::Failed {
                    reason: "session requested summarization, which lens sessions disable"
                        .to_string(),
                    budget_signaled: budget_signaled(engine, handle),
                };
            }
            AgentAction::ManualCompactCompleted(_) => {
                // A completion notice, not a request: keep pulling.
            }
            AgentAction::Finished => {
                return DriveOutcome::Finished {
                    budget_signaled: budget_signaled(engine, handle),
                };
            }
            AgentAction::Failed(error) => {
                return DriveOutcome::Failed {
                    reason: error.to_string(),
                    budget_signaled: budget_signaled(engine, handle),
                };
            }
        }
    }
    DriveOutcome::Failed {
        reason: format!("host drive step cap ({MAX_DRIVE_STEPS}) exceeded"),
        budget_signaled: budget_signaled(engine, handle),
    }
}

/// Whether the engine's events for this session include a turn that
/// started with tool calls disallowed — the turn-budget signature.
fn budget_signaled(engine: &AgentEngine, handle: &SessionHandle) -> bool {
    engine.drain_events(handle).iter().any(|event| {
        matches!(
            event,
            AgentEvent::TurnStarted {
                allow_tool_call: false,
                ..
            }
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;

    use agent_core::engine::SessionIoParams;
    use agent_core::message::{AgentMessage, AgentRole, MessageMeta};
    use agent_core::session::{AgentConfig, SessionCapabilities};
    use chomp::model_transport::TransportError;
    use chomp::tools::RepoToolExecutor;
    use serde_json::Value;

    struct DeadTransport;

    impl ModelTransport for DeadTransport {
        fn complete_model_turn(
            &self,
            _request: &agent_core::turn_io::ModelRequest,
        ) -> Result<agent_core::turn_io::ModelResponse, TransportError> {
            Err(TransportError::MissingApiKey)
        }
    }

    #[test]
    fn permanent_transport_failure_fails_the_drive() {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let dir: PathBuf =
            std::env::temp_dir().join(format!("chomp-host-{}-{nonce}", std::process::id()));
        fs::create_dir_all(&dir).expect("create temp dir");
        let executor: Arc<dyn ToolExecutor + Send + Sync> =
            Arc::new(RepoToolExecutor::new(dir.clone(), "abc123".to_string()).expect("executor"));

        let engine = AgentEngine::new();
        let handle = engine
            .create_session(
                vec![AgentMessage {
                    id: "u1".to_string(),
                    role: AgentRole::User,
                    content: "review this".to_string(),
                    metadata: MessageMeta::new(),
                }],
                AgentConfig::default(),
                SessionCapabilities {
                    summarization_allowed: false,
                    ..Default::default()
                },
                Value::Null,
                SessionIoParams::new(
                    "openrouter".to_string(),
                    200_000,
                    "[]".to_string(),
                    "sys".to_string(),
                )
                .expect("io params"),
            )
            .expect("session");

        let outcome = drive_session(&engine, &handle, &DeadTransport, &executor);
        match outcome {
            DriveOutcome::Failed { reason, .. } => {
                assert!(!reason.is_empty(), "failure carries a reason");
            }
            other => panic!("expected a failed drive, got {other:?}"),
        }
        fs::remove_dir_all(dir).expect("clean temp dir");
    }
}
