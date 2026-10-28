//! The application port for model calls — the seam between the host's
//! application layer (the session driver, FRG-19) and infrastructure.
//!
//! The driver depends only on [`ModelTransport`]; the OpenRouter
//! implementation lives in [`crate::transport`] as the adapter behind
//! this port, and tests can substitute a fake without any HTTP. The
//! port's error vocabulary, [`TransportError`], is defined here rather
//! than in the adapter: it is part of the contract the application
//! layer programs against (in particular the failure-report mapping
//! the driver feeds to `Engine::submit_model_turn_failure`).

use std::fmt;

use agent_core::model_api_retry::{ModelApiErrorClass, ModelTurnFailureReport};
use agent_core::turn_io::{ModelRequest, ModelResponse};

/// One model turn through the port: render the request, perform the
/// provider round-trip, and map the response back. Implementations
/// never retry — retry policy belongs to the engine, driven by
/// [`TransportError::to_failure_report`].
pub trait ModelTransport {
    fn complete_model_turn(&self, request: &ModelRequest) -> Result<ModelResponse, TransportError>;
}

/// Typed transport failures. Every malformed-input and provider-failure
/// path lands in one of these variants.
#[derive(Debug)]
pub enum TransportError {
    /// `OPENROUTER_API_KEY` was not set (or was blank) in `from_env`.
    MissingApiKey,
    /// The rendered request body could not be serialized for the wire.
    Render(String),
    /// The provider answered with a non-2xx status; the body is kept for
    /// the failure-report message.
    Http {
        status: u16,
        body: String,
        retry_after_ms: Option<u64>,
    },
    /// The HTTP round-trip itself failed (DNS, connect, read, ...).
    Network(String),
    /// A 2xx response whose body could not be mapped onto a
    /// `ModelResponse` (not JSON, no choices, an `error` object, ...).
    MalformedResponse(String),
}

impl fmt::Display for TransportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TransportError::MissingApiKey => {
                write!(f, "OPENROUTER_API_KEY is not set")
            }
            TransportError::Render(detail) => {
                write!(f, "cannot render model request: {detail}")
            }
            TransportError::Http { status, body, .. } => {
                write!(f, "OpenRouter returned HTTP {status}: {body}")
            }
            TransportError::Network(detail) => {
                write!(f, "OpenRouter request failed: {detail}")
            }
            TransportError::MalformedResponse(detail) => {
                write!(f, "malformed OpenRouter response: {detail}")
            }
        }
    }
}

impl std::error::Error for TransportError {}

impl TransportError {
    /// Map onto the engine's failure report. Transient (the engine may
    /// retry the turn): 429, 5xx, network failures, and malformed
    /// responses. Permanent: other 4xx statuses, a missing API key, and
    /// unrenderable requests — retrying those cannot help.
    pub fn to_failure_report(&self) -> ModelTurnFailureReport {
        let (error_class, http_status, retry_after_ms) = match self {
            TransportError::Http {
                status,
                retry_after_ms,
                ..
            } => {
                let class = if *status == 429 || *status >= 500 {
                    ModelApiErrorClass::Transient
                } else {
                    ModelApiErrorClass::Permanent
                };
                (class, Some(*status), *retry_after_ms)
            }
            TransportError::Network(_) | TransportError::MalformedResponse(_) => {
                (ModelApiErrorClass::Transient, None, None)
            }
            TransportError::MissingApiKey | TransportError::Render(_) => {
                (ModelApiErrorClass::Permanent, None, None)
            }
        };
        ModelTurnFailureReport {
            error_class,
            http_status,
            provider_code: http_status.map(|status| status.to_string()),
            message: self.to_string(),
            had_partial_output: false,
            partial_output_chars: 0,
            retry_after_ms,
        }
    }
}
