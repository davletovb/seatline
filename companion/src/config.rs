use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use seatline_core::turn::Namespace;
use seatline_platform::layout::Layout;
use seatline_platform::private_fs::create_private_dir;
use serde::{Deserialize, Serialize};

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Grant {
    pub app: String,
    pub token: String,
    pub providers: Vec<String>,
    pub allow_provider_default: bool,
    #[serde(default)]
    pub extension_origins: Vec<String>,
    #[serde(default)]
    pub web_origins: Vec<String>,
    #[serde(default)]
    pub web_relays: Vec<String>,
    #[serde(default)]
    pub cache_title: Option<String>,
    #[serde(default)]
    pub native_adapter: Option<NativeAdapter>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NativeAdapter {
    pub executable: PathBuf,
    pub args: Vec<String>,
}

pub const PROVIDERS: [&str; 4] = ["codex", "claude", "gemini", "grok"];

/// A Chrome extension origin: `chrome-extension://` + 32 letters a-p + `/`.
pub fn valid_extension_origin(origin: &str) -> bool {
    origin
        .strip_prefix("chrome-extension://")
        .and_then(|s| s.strip_suffix('/'))
        .is_some_and(|id| id.len() == 32 && id.bytes().all(|b| (b'a'..=b'p').contains(&b)))
}

/// A bare HTTPS origin such as `https://app.example.com` or `https://host:8443`,
/// returned in the form `pair` compares against. Paths, queries, fragments and
/// credentials are refused rather than silently dropped.
pub fn https_origin(value: &str) -> io::Result<String> {
    let invalid = || {
        io::Error::other(format!(
            "`{value}` is not a bare https:// origin (no path, query or credentials)"
        ))
    };
    let url = url::Url::parse(value).map_err(|_| invalid())?;
    if url.scheme() != "https"
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || url.path() != "/"
    {
        return Err(invalid());
    }
    Ok(url.origin().ascii_serialization())
}

/// Builds an app grant from `authorize APP PROVIDERS [ARG...]`. Every argument
/// must be understood: a mistyped origin must fail, not authorize nothing.
pub fn grant_from_args(app: &str, providers: &str, rest: &[String]) -> io::Result<Grant> {
    let namespace =
        Namespace::fixed(app).map_err(|_| io::Error::other("invalid app identifier"))?;
    let providers: Vec<String> = providers.split(',').map(str::to_owned).collect();
    let mut unique = Vec::new();
    for provider in &providers {
        if !PROVIDERS.contains(&provider.as_str()) {
            return Err(io::Error::other(format!(
                "unknown provider `{provider}` (expected: {})",
                PROVIDERS.join(", ")
            )));
        }
        if unique.contains(provider) {
            return Err(io::Error::other(format!(
                "provider `{provider}` is listed twice"
            )));
        }
        unique.push(provider.clone());
    }
    let mut grant = Grant {
        app: app.to_owned(),
        token: random_token()?,
        providers,
        allow_provider_default: false,
        extension_origins: Vec::new(),
        web_origins: Vec::new(),
        web_relays: Vec::new(),
        cache_title: None,
        native_adapter: None,
    };
    for arg in rest {
        if let Some(relay) = arg.strip_prefix("--relay=") {
            let relay = https_origin(relay)?;
            if !grant.web_relays.contains(&relay) {
                grant.web_relays.push(relay);
            }
        } else if let Some(title) = arg.strip_prefix("--cache-title=") {
            if grant.cache_title.is_some() {
                return Err(io::Error::other("--cache-title was given twice"));
            }
            Layout::with_cache_title(namespace.clone(), title).map_err(|_| {
                io::Error::other("--cache-title must match the app identifier, ignoring case")
            })?;
            grant.cache_title = Some(title.to_owned());
        } else if arg == "--allow-provider-default" {
            grant.allow_provider_default = true;
        } else if arg.starts_with("--") {
            return Err(io::Error::other(format!("unknown option `{arg}`")));
        } else if valid_extension_origin(arg) {
            if !grant.extension_origins.contains(arg) {
                grant.extension_origins.push(arg.clone());
            }
        } else if arg.starts_with("https://") {
            let origin = https_origin(arg)?;
            if !grant.web_origins.contains(&origin) {
                grant.web_origins.push(origin);
            }
        } else if arg.starts_with("chrome-extension://") {
            return Err(io::Error::other(format!(
                "`{arg}` is not a valid extension origin (chrome-extension://<32 letters a-p>/)"
            )));
        } else if arg.starts_with("http://") {
            return Err(io::Error::other(format!(
                "`{arg}`: only https:// website origins are supported"
            )));
        } else {
            return Err(io::Error::other(format!(
                "unrecognized argument `{arg}` (expected an extension origin, an https:// origin or an option)"
            )));
        }
    }
    Ok(grant)
}

pub fn data_dir() -> io::Result<PathBuf> {
    if let Some(path) = std::env::var_os("SEATLINE_DATA_DIR") {
        return Ok(PathBuf::from(path));
    }
    #[cfg(windows)]
    let base = std::env::var_os("LOCALAPPDATA").map(PathBuf::from);
    #[cfg(not(windows))]
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/share")));
    base.map(|base| base.join("seatline"))
        .ok_or_else(|| io::Error::other("Seatline user directory is unavailable"))
}

pub fn random_token() -> io::Result<String> {
    let mut bytes = [0_u8; 32];
    getrandom::fill(&mut bytes).map_err(|_| io::Error::other("OS randomness unavailable"))?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

pub fn app_path(root: &Path, app: &str) -> io::Result<PathBuf> {
    Namespace::fixed(app).map_err(|_| io::Error::other("invalid app identifier"))?;
    Ok(root.join("apps").join(format!("{app}.json")))
}

pub fn write_private(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::other("missing parent"))?;
    create_private_dir(parent)?;
    let temp = parent.join(format!(".{}.tmp", random_token()?));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let result = (|| {
        let mut file = options.open(&temp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        // Windows cannot rename over an existing file. Registry writes are
        // serialized by the administrative lock; readers fail closed during
        // replacement rather than accepting a partially written credential.
        #[cfg(windows)]
        if path.exists() {
            fs::remove_file(path)?;
        }
        fs::rename(&temp, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(temp);
    }
    result
}

pub fn load_grant(root: &Path, app: &str) -> io::Result<Grant> {
    let path = app_path(root, app)?;
    if fs::symlink_metadata(&path)?.file_type().is_symlink() {
        return Err(io::Error::other("symlink grant rejected"));
    }
    let grant: Grant = serde_json::from_slice(&fs::read(path)?)?;
    if grant.app != app || grant.token.len() != 64 {
        return Err(io::Error::other("invalid grant"));
    }
    Ok(grant)
}

pub fn same_token(a: &str, b: &str) -> bool {
    a.len() == 64
        && b.len() == 64
        && a.bytes()
            .zip(b.bytes())
            .fold(0_u8, |different, (a, b)| different | (a ^ b))
            == 0
}

pub fn endpoint(root: &Path) -> io::Result<String> {
    #[cfg(not(windows))]
    {
        root.join("broker.sock")
            .to_str()
            .map(str::to_owned)
            .ok_or_else(|| io::Error::other("non-UTF8 IPC path"))
    }
    #[cfg(windows)]
    {
        let file = root.join("instance-id");
        if !file.exists() {
            write_private(&file, random_token()?.as_bytes())?;
        }
        Ok(format!("seatline-{}", fs::read_to_string(file)?.trim()))
    }
}

pub fn lock(root: &Path, name: &str) -> io::Result<fs::File> {
    create_private_dir(root)?;
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(root.join(name))?;
    match fs2::FileExt::try_lock_exclusive(&file) {
        Ok(()) => {}
        Err(error) if error.raw_os_error() == fs2::lock_contended_error().raw_os_error() => {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "Seatline lock is in use",
            ));
        }
        Err(error) => return Err(error),
    }
    Ok(file)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_owned()).collect()
    }
    const EXTENSION: &str = "chrome-extension://abcdefghijklmnopabcdefghijklmnop/";

    #[test]
    fn https_origins_are_normalized_and_anything_else_is_refused() {
        assert_eq!(
            https_origin("https://App.Example.com").unwrap(),
            "https://app.example.com"
        );
        assert_eq!(
            https_origin("https://app.example.com:443/").unwrap(),
            "https://app.example.com"
        );
        assert_eq!(
            https_origin("https://localhost:8443").unwrap(),
            "https://localhost:8443"
        );
        for bad in [
            "http://app.example.com",
            "https://app.example.com/path",
            "https://app.example.com/?q=1",
            "https://app.example.com/#fragment",
            "https://user:pass@app.example.com",
            "app.example.com",
            "https://",
        ] {
            assert!(https_origin(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn authorize_collects_every_recognized_argument() {
        let grant = grant_from_args(
            "tabbeam",
            "codex,claude",
            &args(&[
                EXTENSION,
                "https://app.example.com",
                "--relay=https://relay.example.com",
                "--cache-title=TabBeam",
                "--allow-provider-default",
            ]),
        )
        .unwrap();
        assert_eq!(grant.providers, ["codex", "claude"]);
        assert_eq!(grant.extension_origins, [EXTENSION]);
        assert_eq!(grant.web_origins, ["https://app.example.com"]);
        assert_eq!(grant.web_relays, ["https://relay.example.com"]);
        assert_eq!(grant.cache_title.as_deref(), Some("TabBeam"));
        assert!(grant.allow_provider_default);
        assert_eq!(grant.token.len(), 64);
    }

    #[test]
    fn provider_default_tools_stay_off_unless_asked_for() {
        let grant = grant_from_args("tabbeam", "codex", &[]).unwrap();
        assert!(!grant.allow_provider_default);
    }

    #[test]
    fn mistakes_fail_instead_of_authorizing_nothing() {
        for bad in [
            "http://localhost:5173",
            "https://app.example.com/app",
            "chrome-extension://short/",
            "--allow-everything",
            "--relay=http://relay.example.com",
            "--cache-title=SomethingElse",
            "app.example.com",
        ] {
            assert!(
                grant_from_args("tabbeam", "codex", &args(&[bad])).is_err(),
                "{bad}"
            );
        }
        assert!(grant_from_args("tabbeam", "codex,codex", &[]).is_err());
        assert!(grant_from_args("tabbeam", "codex,unknown", &[]).is_err());
        assert!(grant_from_args("tabbeam", "", &[]).is_err());
        assert!(grant_from_args("bad app", "codex", &[]).is_err());
    }

    #[test]
    fn repeated_origins_are_listed_once() {
        let grant = grant_from_args(
            "tabbeam",
            "codex",
            &args(&[
                EXTENSION,
                EXTENSION,
                "https://a.example.com",
                "https://a.example.com/",
            ]),
        )
        .unwrap();
        assert_eq!(grant.extension_origins.len(), 1);
        assert_eq!(grant.web_origins.len(), 1);
    }
}
