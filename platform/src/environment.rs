//! The environment a provider process starts with (SEC-02).
//!
//! A provider gets an empty environment plus a short, fixed list of variables
//! copied from the host's own, [`INHERITED`]: what command-line tools need to
//! find the user's home and settings, temporary files, text encoding, the
//! session's keyring, and the network (proxies and extra CA certificates). An
//! adapter adds its provider's own settings, such as `CODEX_HOME`, and sets
//! `PATH` itself.
//!
//! Nothing else is passed on. Credentials such as `OPENAI_API_KEY` stay
//! behind, so a provider uses its own stored sign-in, the one its status check
//! reports, however Chrome was started. So do variables that change how
//! programs load code, such as `NODE_OPTIONS`, `LD_PRELOAD`, and
//! `DYLD_INSERT_LIBRARIES`, and the application's own settings.

use std::ffi::{OsStr, OsString};

/// Variables every provider gets from the host's environment, when set.
#[cfg(unix)]
pub const INHERITED: &[&str] = &[
    // The user, and the home directory their settings and sign-ins live in.
    "HOME",
    "USER",
    "LOGNAME",
    // Temporary files: a per-user directory on macOS.
    "TMPDIR",
    // Text encoding.
    "LANG",
    "LC_ALL",
    "LC_CTYPE",
    // The session bus and runtime directory, where Linux keyrings are found.
    "DBUS_SESSION_BUS_ADDRESS",
    "XDG_RUNTIME_DIR",
    // Network proxies, in both spellings tools read.
    "HTTPS_PROXY",
    "https_proxy",
    "HTTP_PROXY",
    "http_proxy",
    "ALL_PROXY",
    "all_proxy",
    "NO_PROXY",
    "no_proxy",
    // Extra CA certificates, for networks that inspect TLS.
    "SSL_CERT_FILE",
    "SSL_CERT_DIR",
    "NODE_EXTRA_CA_CERTS",
];

/// Variables every provider gets from the host's environment, when set.
/// Windows compares their names without regard to case.
#[cfg(not(unix))]
pub const INHERITED: &[&str] = &[
    // What Windows programs, `cmd.exe` (which runs npm's `.cmd` launchers),
    // and Node need to start and find their files.
    "SystemRoot",
    "SystemDrive",
    "windir",
    "ComSpec",
    "PATHEXT",
    "TEMP",
    "TMP",
    "USERPROFILE",
    "HOMEDRIVE",
    "HOMEPATH",
    "APPDATA",
    "LOCALAPPDATA",
    "USERNAME",
    "USERDOMAIN",
    "ProgramData",
    "ProgramFiles",
    "ProgramFiles(x86)",
    "ProgramW6432",
    "CommonProgramFiles",
    "CommonProgramFiles(x86)",
    "CommonProgramW6432",
    "NUMBER_OF_PROCESSORS",
    "PROCESSOR_ARCHITECTURE",
    "OS",
    // Network proxies and extra CA certificates.
    "HTTPS_PROXY",
    "HTTP_PROXY",
    "ALL_PROXY",
    "NO_PROXY",
    "SSL_CERT_FILE",
    "SSL_CERT_DIR",
    "NODE_EXTRA_CA_CERTS",
];

/// The variables of `host` named in [`INHERITED`] or `extra`, in `host`'s
/// order. `host` is an environment as [`std::env::vars_os`] gives it.
pub fn inherit<I>(host: I, extra: &[&str]) -> Vec<(OsString, OsString)>
where
    I: IntoIterator<Item = (OsString, OsString)>,
{
    host.into_iter()
        .filter(|(name, _)| {
            INHERITED
                .iter()
                .chain(extra)
                .any(|wanted| same_name(name, wanted))
        })
        .collect()
}

/// The value of `name` in `host`, compared as [`inherit`] compares names.
/// The user's home directory in `host`: `HOME`, or `USERPROFILE` on Windows.
pub fn home_dir(host: &[(OsString, OsString)]) -> Option<std::path::PathBuf> {
    #[cfg(unix)]
    let name = "HOME";
    #[cfg(not(unix))]
    let name = "USERPROFILE";
    lookup(host, name).map(std::path::PathBuf::from)
}

pub fn lookup<'a>(host: &'a [(OsString, OsString)], name: &str) -> Option<&'a OsStr> {
    host.iter()
        .find(|(candidate, _)| same_name(candidate, name))
        .map(|(_, value)| value.as_os_str())
}

/// Provider PATH with the executable's directory first. If an inherited PATH
/// contains a platform-invalid entry that `join_paths` cannot reconstruct,
/// preserve the inherited value rather than replacing PATH with an empty one.
pub fn search_path_for(executable: &std::path::Path, inherited: Option<&OsStr>) -> OsString {
    let dirs = executable
        .parent()
        .map(std::path::Path::to_path_buf)
        .into_iter()
        .chain(inherited.into_iter().flat_map(std::env::split_paths));
    std::env::join_paths(dirs)
        .unwrap_or_else(|_| inherited.map(OsStr::to_os_string).unwrap_or_default())
}

#[cfg(unix)]
fn same_name(name: &OsStr, wanted: &str) -> bool {
    name == wanted
}

#[cfg(not(unix))]
fn same_name(name: &OsStr, wanted: &str) -> bool {
    name.to_str()
        .is_some_and(|name| name.eq_ignore_ascii_case(wanted))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vars(pairs: &[(&str, &str)]) -> Vec<(OsString, OsString)> {
        pairs
            .iter()
            .map(|(name, value)| (OsString::from(name), OsString::from(value)))
            .collect()
    }

    #[test]
    fn only_listed_variables_are_inherited() {
        let home = if cfg!(unix) { "HOME" } else { "USERPROFILE" };
        let host = vars(&[
            ("OPENAI_API_KEY", "sk-live-secret"),
            (home, "/home/user"),
            ("AWS_SECRET_ACCESS_KEY", "secret"),
            ("GITHUB_TOKEN", "ghp_secret"),
            ("NODE_OPTIONS", "--require /tmp/evil.js"),
            ("LD_PRELOAD", "/tmp/evil.so"),
            ("DYLD_INSERT_LIBRARIES", "/tmp/evil.dylib"),
            ("MY_APP_PROVIDER_PATH", "/opt/bin"),
            ("PATH", "/usr/bin"),
            ("HTTPS_PROXY", "http://proxy:3128"),
            ("SSL_CERT_FILE", "/etc/corp.pem"),
            ("NODE_EXTRA_CA_CERTS", "/etc/node-corp.pem"),
            ("CODEX_HOME", "/home/user/.codex"),
            ("CODEX_API_KEY", "sk-live-secret"),
        ]);
        assert_eq!(
            inherit(host, &["CODEX_HOME"]),
            vars(&[
                (home, "/home/user"),
                ("HTTPS_PROXY", "http://proxy:3128"),
                ("SSL_CERT_FILE", "/etc/corp.pem"),
                ("NODE_EXTRA_CA_CERTS", "/etc/node-corp.pem"),
                ("CODEX_HOME", "/home/user/.codex"),
            ])
        );
    }

    #[test]
    fn nothing_listed_means_nothing_inherited() {
        assert!(inherit(vars(&[("SECRET_TOKEN", "x"), ("PATH", "/bin")]), &[]).is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn names_match_exactly_on_posix() {
        let host = vars(&[("home", "/nope"), ("Home", "/nope"), ("HOME", "/home/user")]);
        assert_eq!(inherit(host.clone(), &[]), vars(&[("HOME", "/home/user")]));
        assert_eq!(lookup(&host, "HOME"), Some(OsStr::new("/home/user")));
        assert_eq!(lookup(&host, "PATH"), None);
    }

    #[cfg(not(unix))]
    #[test]
    fn names_match_regardless_of_case_on_windows() {
        let host = vars(&[("SYSTEMROOT", "C:\\Windows"), ("Path", "C:\\bin")]);
        assert_eq!(
            inherit(host.clone(), &[]),
            vars(&[("SYSTEMROOT", "C:\\Windows")])
        );
        assert_eq!(lookup(&host, "PATH"), Some(OsStr::new("C:\\bin")));
    }
}
