//! Host driver for `agent_core` sessions.
//!
//! `agent_core` is pull-based: the host creates a session and repeatedly
//! calls `next_action`, performs the requested transport, tool, or
//! summarization work, and feeds the result back into the session. There are
//! no host callback traits to implement (the callback model was removed from
//! `agent_core` by ENC-285).
//!
//! This module is a scaffold only. The pull-loop driver lands incrementally
//! with the transport (FRG-16), tool executor (FRG-18), and lens sessions
//! (FRG-19). For model calls the driver depends on the application port
//! ([`crate::model_transport::ModelTransport`]), never on a concrete
//! transport implementation.
