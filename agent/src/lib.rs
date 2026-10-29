//! Library surface of the chomp host crate.
//!
//! The binary (`src/main.rs`) declares its own module tree; the
//! transport lives only here, in the lib target, so its unit tests run
//! once and integration tests can drive it end to end (currently the
//! FRG-16 transport loop tests). The FRG-19 host driver will consume it
//! from the binary through this surface. The FRG-18 read-only tool
//! executor also lives here, but stays unwired from sessions until a
//! later slice lands redaction.

pub mod model_transport;
pub mod snapshot;
pub mod tools;
pub mod transport;
