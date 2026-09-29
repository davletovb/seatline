//! Finding provider executables (PRO-02).
//!
//! Executables are found by platform-controlled lookup rules, never from a
//! request: a provider ID only selects an adapter, and the adapter looks for
//! its own fixed executable name in these directories.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// The directories searched for provider executables, in order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchPath(Vec<PathBuf>);

impl SearchPath {
    /// Searches `dirs`. Relative directories are dropped, so a lookup never
    /// depends on the host's working directory.
    pub fn new(dirs: impl IntoIterator<Item = PathBuf>) -> Self {
        Self(dirs.into_iter().filter(|dir| dir.is_absolute()).collect())
    }

    /// The lookup rules of an installed host: the directories in
    /// `override_variable` when it is set; otherwise the host's `PATH`,
    /// then the platform's usual install locations. The latter matter because
    /// Chrome starts the host with a minimal `PATH`.
    pub fn from_env(override_variable: &str) -> Self {
        Self::from_vars(
            std::env::var_os(override_variable),
            std::env::var_os("PATH"),
            home_dir(),
        )
    }

    fn from_vars(
        override_: Option<OsString>,
        path: Option<OsString>,
        home: Option<PathBuf>,
    ) -> Self {
        if let Some(dirs) = override_ {
            return Self::new(std::env::split_paths(&dirs));
        }
        let from_path = path
            .iter()
            .flat_map(std::env::split_paths)
            .collect::<Vec<_>>();
        Self::new(
            from_path
                .into_iter()
                .chain(usual_locations(home.as_deref())),
        )
    }

    pub fn dirs(&self) -> &[PathBuf] {
        &self.0
    }

    /// The first executable called `name` in these directories: on Windows
    /// `name.exe`, then `name.cmd`, the form npm installs. The path is kept as
    /// found, symlinks included.
    pub fn find(&self, name: &str) -> Option<PathBuf> {
        self.0.iter().find_map(|dir| {
            file_names(name)
                .into_iter()
                .map(|file| dir.join(file))
                .find(|candidate| is_executable(candidate))
        })
    }
}

#[cfg(unix)]
fn file_names(name: &str) -> Vec<String> {
    vec![name.to_owned()]
}

#[cfg(not(unix))]
fn file_names(name: &str) -> Vec<String> {
    vec![format!("{name}.exe"), format!("{name}.cmd")]
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;

    path.metadata()
        .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn is_executable(path: &Path) -> bool {
    path.is_file()
}

#[cfg(unix)]
fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

#[cfg(not(unix))]
fn home_dir() -> Option<PathBuf> {
    std::env::var_os("APPDATA").map(PathBuf::from)
}

/// Where command-line tools are usually installed, beyond `PATH`: Homebrew,
/// user-level bin directories, and the npm prefixes of Node version managers.
#[cfg(unix)]
fn usual_locations(home: Option<&Path>) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if cfg!(target_os = "macos") {
        dirs.push(PathBuf::from("/opt/homebrew/bin"));
    }
    dirs.push(PathBuf::from("/usr/local/bin"));
    if let Some(home) = home {
        for relative in [
            ".local/bin",
            ".npm-global/bin",
            ".volta/bin",
            ".bun/bin",
            "bin",
        ] {
            dirs.push(home.join(relative));
        }
        dirs.extend(nvm_bins(&home.join(".nvm/versions/node")));
    }
    dirs
}

/// On Windows, `home` is `%APPDATA%`, where npm installs global commands.
#[cfg(not(unix))]
fn usual_locations(home: Option<&Path>) -> Vec<PathBuf> {
    home.map(|appdata| appdata.join("npm"))
        .into_iter()
        .collect()
}

/// The `bin` directories of the Node versions nvm installed, newest first.
#[cfg(unix)]
fn nvm_bins(versions: &Path) -> Vec<PathBuf> {
    use std::cmp::Reverse;

    let Ok(entries) = std::fs::read_dir(versions) else {
        return Vec::new();
    };
    let mut found: Vec<(Vec<u64>, PathBuf)> = entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let name = entry.file_name().into_string().ok()?;
            let version = name
                .strip_prefix('v')?
                .split('.')
                .map(|part| part.parse().ok())
                .collect::<Option<Vec<u64>>>()?;
            Some((version, entry.path().join("bin")))
        })
        .collect();
    found.sort_by_key(|(version, _)| Reverse(version.clone()));
    found.into_iter().map(|(_, bin)| bin).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("discovery-test-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// `path` made absolute on every platform: Windows needs a drive.
    fn absolute(path: &str) -> PathBuf {
        if cfg!(windows) {
            PathBuf::from(format!("C:{path}"))
        } else {
            PathBuf::from(path)
        }
    }

    fn install(dir: &Path, file: &str) -> PathBuf {
        let path = dir.join(file);
        std::fs::write(&path, b"#!/bin/sh\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        path
    }

    #[test]
    fn the_override_replaces_every_default() {
        let dirs = std::env::join_paths([absolute("/opt/a"), absolute("/opt/b")]).unwrap();
        let search = SearchPath::from_vars(
            Some(dirs),
            Some(absolute("/usr/bin").into_os_string()),
            Some(absolute("/home/u")),
        );
        assert_eq!(search.dirs(), [absolute("/opt/a"), absolute("/opt/b")]);
    }

    #[test]
    fn path_comes_before_the_usual_locations_and_relative_entries_are_dropped() {
        let path = std::env::join_paths([
            absolute("/first"),
            PathBuf::from("."),
            PathBuf::from("relative/bin"),
            absolute("/second"),
        ])
        .unwrap();
        let search = SearchPath::from_vars(None, Some(path), Some(absolute("/home/u")));
        let dirs = search.dirs();
        assert_eq!(dirs[..2], [absolute("/first"), absolute("/second")]);
        assert!(dirs.iter().all(|dir| dir.is_absolute()));
        #[cfg(unix)]
        assert!(dirs.contains(&PathBuf::from("/home/u/.local/bin")));
    }

    #[test]
    fn the_first_directory_with_the_executable_wins() {
        let root = temp_dir("first");
        let (empty, first, second) = (root.join("empty"), root.join("first"), root.join("second"));
        for dir in [&empty, &first, &second] {
            std::fs::create_dir_all(dir).unwrap();
        }
        let expected = install(&first, &file_names("codex")[0]);
        install(&second, &file_names("codex")[0]);

        let search = SearchPath::new([empty, first, second]);
        assert_eq!(search.find("codex"), Some(expected));
        assert_eq!(search.find("claude"), None);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn a_file_that_is_not_executable_is_skipped() {
        let root = temp_dir("mode");
        std::fs::write(root.join("codex"), b"not executable").unwrap();
        std::fs::create_dir_all(root.join("dir/codex")).unwrap();
        assert_eq!(
            SearchPath::new([root.clone(), root.join("dir")]).find("codex"),
            None
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn nvm_versions_are_searched_newest_first() {
        let root = temp_dir("nvm");
        for version in ["v9.11.2", "v22.3.0", "v18.20.4", "not-a-version"] {
            std::fs::create_dir_all(root.join(version).join("bin")).unwrap();
        }
        assert_eq!(
            nvm_bins(&root),
            [
                root.join("v22.3.0/bin"),
                root.join("v18.20.4/bin"),
                root.join("v9.11.2/bin")
            ]
        );
        std::fs::remove_dir_all(root).unwrap();
    }
}
