//! Per-user installation and generic Chrome registration. No product bundles.
use crate::config;
use serde_json::json;
use std::io;
use std::path::{Path, PathBuf};

pub fn executable(root: &Path) -> io::Result<Option<PathBuf>> {
    match std::fs::read_to_string(root.join("companion-executable")) {
        Ok(value) => {
            let path = PathBuf::from(value.trim());
            if !path.is_absolute() {
                return Err(io::Error::other("invalid installed companion path"));
            }
            Ok(Some(path))
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}
pub fn manifest(root: &Path, fallback: &Path) -> io::Result<String> {
    let mut origins = Vec::new();
    if let Ok(entries) = std::fs::read_dir(root.join("apps")) {
        for entry in entries.flatten() {
            if let Some(app) = entry.path().file_stem().and_then(|name| name.to_str()) {
                if let Ok(grant) = config::load_grant(root, app) {
                    origins.extend(grant.extension_origins);
                }
            }
        }
    }
    origins.sort();
    origins.dedup();
    Ok(serde_json::to_string_pretty(
        &json!({"name":"com.seatline.host","description":"Seatline shared companion",
        "path":executable(root)?.unwrap_or_else(|| fallback.to_owned()),"type":"stdio","allowed_origins":origins}),
    )?)
}
#[allow(clippy::disallowed_methods)] // Fixed per-user OS registry registration, never an app wire command.
pub fn register(root: &Path) -> io::Result<()> {
    let text = manifest(root, &std::env::current_exe()?)?;
    #[cfg(windows)]
    {
        let path = root.join("com.seatline.host.json");
        config::write_private(&path, text.as_bytes())?;
        let windows = std::env::var_os("WINDIR")
            .ok_or_else(|| io::Error::other("Windows directory unavailable"))?;
        let status = std::process::Command::new(PathBuf::from(windows).join("System32/reg.exe"))
            .args([
                "add",
                r"HKCU\Software\Google\Chrome\NativeMessagingHosts\com.seatline.host",
                "/ve",
                "/t",
                "REG_SZ",
                "/d",
            ])
            .arg(path)
            .arg("/f")
            .stdout(std::process::Stdio::null())
            .status()?;
        if !status.success() {
            return Err(io::Error::other("Chrome registration failed"));
        }
    }
    #[cfg(not(windows))]
    {
        let home = std::env::var_os("HOME").ok_or_else(|| io::Error::other("home unavailable"))?;
        #[cfg(target_os = "macos")]
        let directory = PathBuf::from(home)
            .join("Library/Application Support/Google/Chrome/NativeMessagingHosts");
        #[cfg(not(target_os = "macos"))]
        let directory = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(home).join(".config"))
            .join("google-chrome/NativeMessagingHosts");
        config::write_private(&directory.join("com.seatline.host.json"), text.as_bytes())?;
    }
    Ok(())
}
pub fn install(root: &Path) -> io::Result<PathBuf> {
    let _lock = config::lock(root, "registry.lock")?;
    let folder = root.join("versions").join(config::random_token()?);
    seatline_platform::private_fs::create_private_dir(&folder)?;
    let destination = folder.join(if cfg!(windows) {
        "seatline-companion.exe"
    } else {
        "seatline-companion"
    });
    std::fs::copy(std::env::current_exe()?, &destination)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&destination, std::fs::Permissions::from_mode(0o700))?;
    }
    config::write_private(
        &root.join("companion-executable"),
        destination
            .to_str()
            .ok_or_else(|| io::Error::other("non-UTF8 install path"))?
            .as_bytes(),
    )?;
    register(root)?;
    Ok(destination)
}
