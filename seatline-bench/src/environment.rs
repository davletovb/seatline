//! What a result was measured on and with, so that two results can be told
//! apart. Nothing here names a person or a machine: no user name, host name or
//! path.

use std::path::Path;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Environment {
    pub os: String,
    pub arch: String,
    /// Logical processors the harness could use.
    pub cpus: usize,
    /// Whether the harness was built with optimizations. A debug build says
    /// nothing about release timings.
    pub harness_profile: String,
    /// The compiler the machine has, which is not necessarily the one that
    /// built the binaries.
    pub rustc: Option<String>,
    /// The commit the tree is at, and whether it has uncommitted changes.
    pub revision: Option<String>,
    pub dirty: Option<bool>,
    pub companion_version: Option<String>,
    /// `release` or `debug`, guessed from the directory the companion is in.
    pub companion_profile: Option<String>,
}

/// The output of `program args`, trimmed, if it ran and succeeded. This is a
/// reporting tool asking tools it was pointed at for their versions, never
/// running anything a request supplied.
#[allow(clippy::disallowed_methods)]
pub fn output(program: impl AsRef<std::ffi::OsStr>, args: &[&str]) -> Option<String> {
    let output = std::process::Command::new(program)
        .args(args)
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
        .filter(|text| !text.is_empty())
}

pub fn capture(companion: &Path) -> Environment {
    let in_repository = output("git", &["rev-parse", "--is-inside-work-tree"]).is_some();
    // `output` is `None` for an empty status, which is a clean tree. Untracked
    // files do not count: they cannot change what was built.
    let dirty = in_repository
        .then(|| output("git", &["status", "--porcelain", "--untracked-files=no"]).is_some());
    Environment {
        os: std::env::consts::OS.to_owned(),
        arch: std::env::consts::ARCH.to_owned(),
        cpus: std::thread::available_parallelism().map_or(1, usize::from),
        harness_profile: if cfg!(debug_assertions) {
            "debug"
        } else {
            "release"
        }
        .to_owned(),
        rustc: output("rustc", &["--version"]),
        revision: output("git", &["rev-parse", "HEAD"]),
        dirty,
        companion_version: output(companion, &["--version"]),
        companion_profile: companion
            .parent()
            .and_then(Path::file_name)
            .and_then(|name| name.to_str())
            .filter(|name| matches!(*name, "release" | "debug"))
            .map(str::to_owned),
    }
}
