//! Reusable provider-runtime primitives shared by provider adapters.
//!
//! Browser transport, such as Chrome Native Messaging framing, and protocol
//! policy belong to the application.

pub mod backlog;
pub mod discovery;
pub mod exchange;
pub mod process;
pub mod prompt;
pub mod protocol;
pub mod search;
pub mod stream;
pub mod telemetry;
pub mod turn;
