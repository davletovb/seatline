use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use seatline_core::turn::Namespace;
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
    fs2::FileExt::try_lock_exclusive(&file)?;
    Ok(file)
}
