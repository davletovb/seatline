//! The platform layer of the provider runtime: what every provider adapter
//! needs from the machine it runs on, and nothing about any one provider.
//!
//! - [`environment`]: the small, fixed environment a provider process gets.
//! - [`workspace`]: the private, empty directory a provider runs in, checked
//!   before every launch.
//! - [`private_fs`]: private files and directories, and small helpers for
//!   bounded buffers and deadlines.
//! - [`layout`]: where an application's workspaces, mappings and cleanup
//!   records live, all under the application's namespace.
//! - [`discovery`]: the executable search path an application can override.
//! - [`forget`]: removing what a provider saved, on a thread of its own, with
//!   retry markers for removals that fail.

pub mod discovery;
pub mod environment;
pub mod forget;
pub mod layout;
pub mod private_fs;
pub mod workspace;
