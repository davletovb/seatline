//! Shared installation and versioned IPC clients for Seatline.
pub const PROTOCOL_VERSION: u32 = 1;
pub mod client;
pub mod config;
pub mod hub;
pub mod wire;

pub mod web;

pub mod install;
