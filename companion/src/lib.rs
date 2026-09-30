//! Shared installation and versioned IPC clients for Seatline.
pub const PROTOCOL_VERSION: u32 = 1;
pub mod client;
pub mod config;
pub mod hub;
pub mod wire;

/// Hosted-app transport; needs the `web` feature (on by default).
#[cfg(feature = "web")]
pub mod secure;
#[cfg(feature = "web")]
pub mod web;

pub mod install;
