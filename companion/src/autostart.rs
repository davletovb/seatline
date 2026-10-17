//! Starting the web service (`seatline-companion web`) when the user logs in, so
//! a website finds Seatline without anyone starting it.
//!
//! macOS gets a launch agent, Linux a systemd user service (or, without
//! systemd, an XDG autostart entry), Windows a `Run` entry. Each starts the
//! installed copy, so `install` must have run. Only the per-user installation
//! is registered: a data directory named with `SEATLINE_DATA_DIR` (tests, a
//! second setup) is left alone, as is everything when
//! `SEATLINE_NO_AUTOSTART=1`.
use crate::{config, install};
use std::io;
use std::path::{Path, PathBuf};

/// The name the service is registered under.
pub const LABEL: &str = "com.seatline.web";

/// Whether registration applies to this process's data directory.
pub fn applies() -> bool {
    std::env::var_os("SEATLINE_DATA_DIR").is_none()
        && std::env::var_os("SEATLINE_NO_AUTOSTART").is_none_or(|value| value != "1")
}

/// Whether any app is authorized for a website.
pub fn wanted(root: &Path) -> bool {
    let Ok(entries) = std::fs::read_dir(root.join("apps")) else {
        return false;
    };
    entries.flatten().any(|entry| {
        entry
            .path()
            .file_stem()
            .and_then(|app| app.to_str())
            .and_then(|app| config::load_grant(root, app).ok())
            .is_some_and(|grant| !grant.web_origins.is_empty())
    })
}

/// Registers the service when an app is authorized for a website, and removes
/// the registration when none is. `restart` replaces a running service, as
/// after `install`. Returns what to tell the user, if anything changed.
pub fn sync(root: &Path, restart: bool) -> io::Result<Option<String>> {
    if !applies() {
        return Ok(None);
    }
    if !wanted(root) {
        return Ok(disable()?.then(|| "Removed the Seatline web service from login items.".into()));
    }
    let Some(executable) = install::executable(root)? else {
        return Ok(Some(
            "Run `seatline-companion install` to keep websites connected in the background.".into(),
        ));
    };
    let registration = enable(&executable, restart)?;
    Ok(Some(format!(
        "The Seatline web service starts at login ({}), so authorized websites connect on their own.",
        registration.display()
    )))
}

/// Registers the installed service and makes sure it runs now.
pub fn enable(executable: &Path, restart: bool) -> io::Result<PathBuf> {
    platform::enable(executable, std::env::var_os("PATH").as_deref(), restart)
}

/// Removes the registration and stops the service. False when there was none.
pub fn disable() -> io::Result<bool> {
    platform::disable()
}

#[cfg(unix)]
fn home() -> io::Result<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| io::Error::other("home directory unavailable"))
}

/// Text inside an XML element or attribute.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn xml(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// A launch agent that runs `executable web`, restarting it if it fails.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn launch_agent(executable: &Path, path: Option<&std::ffi::OsStr>) -> String {
    let environment = path
        .and_then(|path| path.to_str())
        .map(|path| {
            format!(
                "<key>EnvironmentVariables</key><dict><key>PATH</key><string>{}</string></dict>",
                xml(path)
            )
        })
        .unwrap_or_default();
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
         <plist version=\"1.0\"><dict>\
         <key>Label</key><string>{LABEL}</string>\
         <key>ProgramArguments</key><array><string>{}</string><string>web</string></array>\
         {environment}\
         <key>RunAtLoad</key><true/>\
         <key>KeepAlive</key><dict><key>SuccessfulExit</key><false/></dict>\
         <key>ProcessType</key><string>Background</string>\
         </dict></plist>\n",
        xml(&executable.to_string_lossy())
    )
}

/// One systemd unit file value: quoted, with its specifiers escaped.
#[cfg_attr(any(windows, target_os = "macos"), allow(dead_code))]
fn systemd_quoted(text: &str) -> String {
    format!(
        "\"{}\"",
        text.replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace('%', "%%")
    )
}

/// A systemd user service that runs `executable web`, restarting it if it fails.
#[cfg_attr(any(windows, target_os = "macos"), allow(dead_code))]
fn systemd_unit(executable: &Path, path: Option<&std::ffi::OsStr>) -> String {
    let environment = path
        .and_then(|path| path.to_str())
        .map(|path| format!("Environment={}\n", systemd_quoted(&format!("PATH={path}"))))
        .unwrap_or_default();
    format!(
        "[Unit]\nDescription=Seatline web connection for authorized websites\n\n\
         [Service]\nExecStart={} web\n{environment}Restart=on-failure\nRestartSec=5\n\n\
         [Install]\nWantedBy=default.target\n",
        systemd_quoted(&executable.to_string_lossy())
    )
}

#[cfg(target_os = "macos")]
mod platform {
    use super::*;
    use std::ffi::OsStr;
    use std::os::unix::fs::MetadataExt;
    use std::process::{Command, Stdio};

    fn agent() -> io::Result<PathBuf> {
        Ok(home()?
            .join("Library/LaunchAgents")
            .join(format!("{LABEL}.plist")))
    }

    /// The user's launchd domain, `gui/UID`: the session the user logged in to,
    /// where the keychain the providers' sign-ins live in is available.
    fn domain() -> io::Result<String> {
        Ok(format!("gui/{}", std::fs::metadata(home()?)?.uid()))
    }

    #[allow(clippy::disallowed_methods)] // The system's fixed launchctl, with arguments built here.
    fn launchctl(args: &[&OsStr]) -> io::Result<bool> {
        Ok(Command::new("/bin/launchctl")
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()?
            .success())
    }

    pub fn enable(executable: &Path, path: Option<&OsStr>, restart: bool) -> io::Result<PathBuf> {
        let agent = agent()?;
        config::write_private(&agent, launch_agent(executable, path).as_bytes())?;
        let domain = domain()?;
        let service = format!("{domain}/{LABEL}");
        let loaded = launchctl(&[OsStr::new("print"), OsStr::new(&service)])?;
        if restart || !loaded {
            // Replaces what was loaded, which may name an older copy.
            let _ = launchctl(&[OsStr::new("bootout"), OsStr::new(&service)]);
            if !launchctl(&[
                OsStr::new("bootstrap"),
                OsStr::new(&domain),
                agent.as_os_str(),
            ])? {
                return Err(io::Error::other(
                    "launchctl could not start the Seatline web service",
                ));
            }
        }
        Ok(agent)
    }

    pub fn disable() -> io::Result<bool> {
        let agent = agent()?;
        let _ = launchctl(&[
            OsStr::new("bootout"),
            OsStr::new(&format!("{}/{LABEL}", domain()?)),
        ]);
        match std::fs::remove_file(&agent) {
            Ok(()) => Ok(true),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(error),
        }
    }
}

#[cfg(all(unix, not(target_os = "macos")))]
mod platform {
    use super::*;
    use std::ffi::OsStr;
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};

    const UNIT: &str = "seatline-web.service";

    fn config_home() -> io::Result<PathBuf> {
        match std::env::var_os("XDG_CONFIG_HOME") {
            Some(path) if Path::new(&path).is_absolute() => Ok(PathBuf::from(path)),
            _ => Ok(home()?.join(".config")),
        }
    }

    fn unit() -> io::Result<PathBuf> {
        Ok(config_home()?.join("systemd/user").join(UNIT))
    }

    fn desktop_entry() -> io::Result<PathBuf> {
        Ok(config_home()?.join("autostart/seatline-web.desktop"))
    }

    #[allow(clippy::disallowed_methods)] // The system's fixed systemctl, with arguments built here.
    fn systemctl(args: &[&str]) -> bool {
        let Some(systemctl) = ["/usr/bin/systemctl", "/bin/systemctl"]
            .into_iter()
            .find(|path| Path::new(path).is_file())
        else {
            return false;
        };
        Command::new(systemctl)
            .arg("--user")
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    }

    /// Starts the service now, apart from this process. One that is already
    /// running keeps its port; the new one sees that and leaves.
    #[allow(clippy::disallowed_methods)] // Starts only the installed companion.
    fn start_detached(executable: &Path) -> io::Result<()> {
        Command::new(executable)
            .arg("web")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0)
            .spawn()
            .map(drop)
    }

    pub fn enable(executable: &Path, path: Option<&OsStr>, restart: bool) -> io::Result<PathBuf> {
        let unit = unit()?;
        config::write_private(&unit, systemd_unit(executable, path).as_bytes())?;
        if systemctl(&["daemon-reload"])
            && systemctl(&["enable", UNIT])
            && systemctl(&[if restart { "restart" } else { "start" }, UNIT])
        {
            return Ok(unit);
        }
        // No user systemd (a container, another init): an autostart entry for the
        // next login, and the service started now.
        let _ = std::fs::remove_file(&unit);
        let entry = desktop_entry()?;
        let quoted = executable
            .to_string_lossy()
            .replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace('`', "\\`")
            .replace('$', "\\$")
            .replace('%', "%%");
        config::write_private(
            &entry,
            format!(
                "[Desktop Entry]\nType=Application\nName=Seatline web connection\n\
                 Exec=\"{quoted}\" web\nNoDisplay=true\nX-GNOME-Autostart-enabled=true\n"
            )
            .as_bytes(),
        )?;
        start_detached(executable)?;
        Ok(entry)
    }

    pub fn disable() -> io::Result<bool> {
        let mut removed = false;
        let unit = unit()?;
        if unit.exists() {
            systemctl(&["disable", "--now", UNIT]);
            std::fs::remove_file(&unit)?;
            systemctl(&["daemon-reload"]);
            removed = true;
        }
        match std::fs::remove_file(desktop_entry()?) {
            Ok(()) => removed = true,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        Ok(removed)
    }
}

#[cfg(windows)]
mod platform {
    use super::*;
    use std::ffi::OsStr;
    use std::os::windows::process::CommandExt;
    use std::process::{Command, Stdio};

    const RUN: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run";
    const VALUE: &str = "Seatline Web";
    /// No console window for a process started in the background.
    pub const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;

    #[allow(clippy::disallowed_methods)] // The system's fixed reg.exe, with arguments built here.
    fn reg(args: &[&OsStr]) -> io::Result<bool> {
        let windows = std::env::var_os("WINDIR")
            .ok_or_else(|| io::Error::other("Windows directory unavailable"))?;
        Ok(
            Command::new(PathBuf::from(windows).join("System32/reg.exe"))
                .args(args)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()?
                .success(),
        )
    }

    /// Starts the service with no console window, apart from this process.
    #[allow(clippy::disallowed_methods)] // Starts only the installed companion.
    pub fn start_detached(executable: &Path) -> io::Result<()> {
        Command::new(executable)
            .arg("web")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .creation_flags(CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP)
            .spawn()
            .map(drop)
    }

    pub fn enable(executable: &Path, _path: Option<&OsStr>, _restart: bool) -> io::Result<PathBuf> {
        // At login Windows starts the command with a console; `--detach` starts the
        // service again without one and leaves at once.
        let command = format!("\"{}\" web --detach", executable.display());
        if !reg(&[
            OsStr::new("add"),
            OsStr::new(RUN),
            OsStr::new("/v"),
            OsStr::new(VALUE),
            OsStr::new("/t"),
            OsStr::new("REG_SZ"),
            OsStr::new("/d"),
            OsStr::new(&command),
            OsStr::new("/f"),
        ])? {
            return Err(io::Error::other(
                "could not add the Seatline web service to login items",
            ));
        }
        start_detached(executable)?;
        Ok(PathBuf::from(format!(r"{RUN}\{VALUE}")))
    }

    pub fn disable() -> io::Result<bool> {
        reg(&[
            OsStr::new("delete"),
            OsStr::new(RUN),
            OsStr::new("/v"),
            OsStr::new(VALUE),
            OsStr::new("/f"),
        ])
    }
}

/// Starts the service again with no console window, for `web --detach`.
#[cfg(windows)]
pub fn detach() -> io::Result<()> {
    platform::start_detached(&std::env::current_exe()?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_launch_agent_runs_the_installed_web_service_and_escapes_what_it_names() {
        let agent = launch_agent(
            Path::new("/Users/a&b/seatline/seatline-companion"),
            Some(std::ffi::OsStr::new("/opt/homebrew/bin:/usr/bin")),
        );
        assert!(agent.contains("<key>Label</key><string>com.seatline.web</string>"));
        assert!(agent.contains(
            "<array><string>/Users/a&amp;b/seatline/seatline-companion</string><string>web</string></array>"
        ));
        assert!(agent.contains("<key>PATH</key><string>/opt/homebrew/bin:/usr/bin</string>"));
        // A clean exit (the service found another running) is not restarted.
        assert!(agent.contains("<key>SuccessfulExit</key><false/>"));
    }

    #[test]
    fn the_systemd_unit_quotes_paths_and_escapes_specifiers() {
        let unit = systemd_unit(
            Path::new("/home/u/my seat%line/seatline-companion"),
            Some(std::ffi::OsStr::new("/usr/bin:/bin")),
        );
        assert!(unit.contains("ExecStart=\"/home/u/my seat%%line/seatline-companion\" web\n"));
        assert!(unit.contains("Environment=\"PATH=/usr/bin:/bin\"\n"));
        assert!(unit.contains("Restart=on-failure"));
    }

    #[test]
    fn a_data_directory_chosen_for_tests_is_never_registered() {
        // The test suite runs with SEATLINE_DATA_DIR set in some jobs and not in
        // others; when it is set, nothing may be registered.
        if std::env::var_os("SEATLINE_DATA_DIR").is_some() {
            assert!(!applies());
        }
        let root = std::env::temp_dir().join(format!(
            "seatline-autostart-{}",
            &config::random_token().unwrap()[..12]
        ));
        assert!(!wanted(&root));
        let grant = config::grant_from_args("my_app", "codex", &[]).unwrap();
        config::write_private(
            &config::app_path(&root, "my_app").unwrap(),
            &serde_json::to_vec(&grant).unwrap(),
        )
        .unwrap();
        assert!(
            !wanted(&root),
            "an app with no website does not need the service"
        );
        let site = config::grant_from_args(
            "my_site",
            "codex",
            &[
                "https://app.example.com".into(),
                "--relay=https://relay.example.com".into(),
            ],
        )
        .unwrap();
        config::write_private(
            &config::app_path(&root, "my_site").unwrap(),
            &serde_json::to_vec(&site).unwrap(),
        )
        .unwrap();
        assert!(wanted(&root));
        let _ = std::fs::remove_dir_all(root);
    }
}
