//! Where an application's provider runtime keeps its files.
//!
//! Every directory the runtime chooses for itself is under the application's
//! [`Namespace`], which is fixed when the adapters are built: workspaces in the
//! user's cache, and conversation mappings and cleanup records in the user's
//! data directory. Two applications that link the runtime therefore never
//! share a workspace or a record, so a mistake in one can't touch the other's
//! files. This protects against accidents, not attacks: any process running as
//! the user can already write to both applications' directories.
//!
//! The paths follow each platform's convention. The namespace fixes the
//! logical identity everywhere; on macOS and Windows an application may also
//! supply the exact display casing of that same namespace for its cache
//! directory. Once an application ships, both are compatibility contracts:
//! changing either can strand files that an installed application expects.

use std::ffi::OsString;
use std::fmt;
use std::hash::{BuildHasher, RandomState};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use seatline_core::turn::Namespace;

use crate::environment;

/// The directories one application's adapters use.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Layout {
    namespace: Namespace,
    /// The display casing of the namespace that macOS and Windows use for the
    /// cache directory.
    cache_title: String,
}

/// A cache title that is not the application's namespace with only its ASCII
/// case changed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CacheTitleError;

impl fmt::Display for CacheTitleError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("cache title must equal the namespace ignoring ASCII case")
    }
}

impl std::error::Error for CacheTitleError {}

impl Layout {
    pub fn new(namespace: Namespace) -> Self {
        let cache_title = default_cache_title(&namespace);
        Self {
            namespace,
            cache_title,
        }
    }

    /// Builds a layout whose macOS/Windows cache directory uses an
    /// application's exact display capitalization. The title must be the same
    /// fixed namespace with ASCII case changed only, so two namespaces cannot
    /// be made to share one cache directory; anything else, such as a
    /// different name, extra characters or a path separator, is a
    /// [`CacheTitleError`]. The check runs on every platform, even though
    /// Linux cache paths use the lowercase namespace directly.
    pub fn with_cache_title(namespace: Namespace, title: &str) -> Result<Self, CacheTitleError> {
        if !title.eq_ignore_ascii_case(namespace.as_str()) {
            return Err(CacheTitleError);
        }
        Ok(Self {
            namespace,
            cache_title: title.to_owned(),
        })
    }

    pub fn namespace(&self) -> &Namespace {
        &self.namespace
    }

    /// The environment variable that replaces the executable lookup for this
    /// application: the namespace in capitals with `_` for `-`, then
    /// `_PROVIDER_PATH` (`MY_APP_PROVIDER_PATH` for `my-app`). Set to a list
    /// of directories in `PATH` form, it names the only places providers are
    /// looked for, for unusual installs and hermetic tests.
    pub fn search_path_variable(&self) -> String {
        format!(
            "{}_PROVIDER_PATH",
            self.namespace
                .as_str()
                .to_ascii_uppercase()
                .replace('-', "_")
        )
    }

    /// A provider's private workspace: `<provider>-workspace` in the
    /// application's cache directory. Without a cache directory, a new
    /// directory with a random name in the temporary directory, which
    /// [`crate::workspace::prepare`] accepts only where the temporary
    /// directory is private to the user, as it is on macOS and Windows.
    pub fn workspace(&self, host: &[(OsString, OsString)], provider: &str) -> PathBuf {
        self.cache_dir(host).map_or_else(
            || {
                std::env::temp_dir().join(format!(
                    "{}-{provider}-{:016x}",
                    self.namespace.as_str(),
                    RandomState::new().hash_one(SystemTime::now())
                ))
            },
            |cache| cache.join(format!("{provider}-workspace")),
        )
    }

    /// The application's directory in the user's cache.
    ///
    /// macOS and Windows use the cache title: by default the namespace with
    /// its first letter capitalized (`My-app`), or the exact casing supplied
    /// by [`Layout::with_cache_title`]. Other Unix systems use the lowercase
    /// namespace directly. The data directory ([`Layout::data_dir`]) uses the
    /// namespace everywhere. Once an application ships, changing either its
    /// namespace or cache-title casing can strand files it expects.
    #[cfg(target_vendor = "apple")]
    pub fn cache_dir(&self, host: &[(OsString, OsString)]) -> Option<PathBuf> {
        absolute(host, "HOME").map(|home| home.join("Library/Caches").join(self.title()))
    }

    /// The application's directory in the user's cache.
    #[cfg(all(unix, not(target_vendor = "apple")))]
    pub fn cache_dir(&self, host: &[(OsString, OsString)]) -> Option<PathBuf> {
        absolute(host, "XDG_CACHE_HOME")
            .map(|cache| cache.join(self.namespace.as_str()))
            .or_else(|| {
                absolute(host, "HOME").map(|home| home.join(".cache").join(self.namespace.as_str()))
            })
    }

    /// The application's directory in the user's cache.
    #[cfg(not(unix))]
    pub fn cache_dir(&self, host: &[(OsString, OsString)]) -> Option<PathBuf> {
        absolute(host, "LOCALAPPDATA").map(|local| local.join(self.title()))
    }

    /// The application's directory in the user's data directory, where
    /// conversation mappings and cleanup records live: `$XDG_DATA_HOME` or
    /// `~/.local/share` on Unix, `%LOCALAPPDATA%` (or `%APPDATA%`) on Windows.
    pub fn data_dir(&self) -> Option<PathBuf> {
        self.data_dir_in(&std::env::vars_os().collect::<Vec<_>>())
    }

    /// [`Layout::data_dir`] for the environment `host`.
    pub fn data_dir_in(&self, host: &[(OsString, OsString)]) -> Option<PathBuf> {
        #[cfg(windows)]
        let base = environment::lookup(host, "LOCALAPPDATA")
            .or_else(|| environment::lookup(host, "APPDATA"))
            .map(PathBuf::from);
        #[cfg(not(windows))]
        let base = absolute(host, "XDG_DATA_HOME").or_else(|| {
            environment::lookup(host, "HOME").map(|home| PathBuf::from(home).join(".local/share"))
        });
        base.map(|base| base.join(self.namespace.as_str()))
    }

    /// The cache-directory display casing of the namespace. Linux cache paths
    /// use the namespace as it is, so only macOS and Windows read it.
    #[cfg_attr(all(unix, not(target_vendor = "apple")), allow(dead_code))]
    fn title(&self) -> &str {
        &self.cache_title
    }
}

fn default_cache_title(namespace: &Namespace) -> String {
    let name = namespace.as_str();
    let mut title = name[..1].to_ascii_uppercase();
    title.push_str(&name[1..]);
    title
}

/// The variable `name` as a path, if it is set to an absolute one.
fn absolute(host: &[(OsString, OsString)], name: &str) -> Option<PathBuf> {
    environment::lookup(host, name)
        .map(PathBuf::from)
        .filter(|path| Path::new(path).is_absolute())
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

    fn layout(name: &str) -> Layout {
        Layout::new(Namespace::fixed(name).unwrap())
    }

    /// A shipped application keeps finding the cache layout it established.
    #[cfg(target_vendor = "apple")]
    #[test]
    fn the_cache_directory_follows_the_platforms_convention() {
        assert_eq!(
            layout("my-app").workspace(&vars(&[("HOME", "/Users/me")]), "codex"),
            PathBuf::from("/Users/me/Library/Caches/My-app/codex-workspace")
        );
    }

    #[cfg(all(unix, not(target_vendor = "apple")))]
    #[test]
    fn the_cache_directory_follows_the_platforms_convention() {
        let app = layout("my-app");
        assert_eq!(
            app.workspace(&vars(&[("HOME", "/home/me")]), "codex"),
            PathBuf::from("/home/me/.cache/my-app/codex-workspace")
        );
        assert_eq!(
            app.workspace(
                &vars(&[("HOME", "/home/me"), ("XDG_CACHE_HOME", "/cache")]),
                "claude"
            ),
            PathBuf::from("/cache/my-app/claude-workspace")
        );
        // A relative cache directory would depend on the working directory.
        assert_eq!(
            app.workspace(
                &vars(&[("HOME", "/home/me"), ("XDG_CACHE_HOME", "cache")]),
                "codex"
            ),
            PathBuf::from("/home/me/.cache/my-app/codex-workspace")
        );
    }

    fn branded(name: &str, title: &str) -> Layout {
        Layout::with_cache_title(Namespace::fixed(name).unwrap(), title).unwrap()
    }

    #[test]
    fn the_default_cache_title_capitalizes_only_the_first_letter() {
        assert_eq!(layout("myapp").title(), "Myapp");
        assert_eq!(layout("my-app").title(), "My-app");
    }

    #[test]
    fn a_custom_cache_title_preserves_application_branding() {
        assert_eq!(branded("myapp", "MyApp").title(), "MyApp");
        assert_eq!(branded("my-app", "My-App").title(), "My-App");
    }

    #[test]
    fn the_default_casing_is_the_same_layout_as_no_title() {
        assert_eq!(branded("myapp", "Myapp"), layout("myapp"));
    }

    #[test]
    fn a_cache_title_must_be_the_namespace_with_only_its_case_changed() {
        let namespace = || Namespace::fixed("myapp").unwrap();
        for title in [
            "OtherApp", "MyApps", "MyApp/", "My/App", r"My\App", "My App", " MyApp", "MyApp ",
            "MyApp:x", "", ".", "..",
        ] {
            assert_eq!(
                Layout::with_cache_title(namespace(), title),
                Err(CacheTitleError),
                "{title:?}"
            );
        }
    }

    #[cfg(target_vendor = "apple")]
    #[test]
    fn a_custom_cache_title_names_the_cache_directory() {
        assert_eq!(
            branded("myapp", "MyApp").workspace(&vars(&[("HOME", "/Users/me")]), "codex"),
            PathBuf::from("/Users/me/Library/Caches/MyApp/codex-workspace")
        );
    }

    /// Other Unix systems use the namespace as it is, whatever the title.
    #[cfg(all(unix, not(target_vendor = "apple")))]
    #[test]
    fn a_custom_cache_title_does_not_change_the_linux_cache_directory() {
        assert_eq!(
            branded("myapp", "MyApp").workspace(&vars(&[("HOME", "/home/me")]), "codex"),
            PathBuf::from("/home/me/.cache/myapp/codex-workspace")
        );
    }

    #[cfg(not(unix))]
    #[test]
    fn a_custom_cache_title_names_the_cache_directory() {
        assert_eq!(
            branded("myapp", "MyApp").workspace(
                &vars(&[("LOCALAPPDATA", r"C:\Users\me\AppData\Local")]),
                "codex"
            ),
            PathBuf::from(r"C:\Users\me\AppData\Local\MyApp\codex-workspace")
        );
    }

    #[test]
    fn a_custom_cache_title_leaves_the_data_directory_alone() {
        let host = vars(&[("HOME", "/home/me"), ("LOCALAPPDATA", r"C:\Local")]);
        assert_eq!(
            branded("myapp", "MyApp").data_dir_in(&host),
            layout("myapp").data_dir_in(&host)
        );
    }

    #[cfg(not(unix))]
    #[test]
    fn the_cache_directory_follows_the_platforms_convention() {
        assert_eq!(
            layout("my-app").workspace(
                &vars(&[("LOCALAPPDATA", r"C:\Users\me\AppData\Local")]),
                "codex"
            ),
            PathBuf::from(r"C:\Users\me\AppData\Local\My-app\codex-workspace")
        );
    }

    #[cfg(not(windows))]
    #[test]
    fn the_data_directory_follows_the_platforms_convention() {
        let app = layout("my-app");
        assert_eq!(
            app.data_dir_in(&vars(&[("HOME", "/home/me")])),
            Some(PathBuf::from("/home/me/.local/share/my-app"))
        );
        assert_eq!(
            app.data_dir_in(&vars(&[("HOME", "/home/me"), ("XDG_DATA_HOME", "/data")])),
            Some(PathBuf::from("/data/my-app"))
        );
        // A relative data directory would depend on the working directory.
        assert_eq!(
            app.data_dir_in(&vars(&[("HOME", "/home/me"), ("XDG_DATA_HOME", "data")])),
            Some(PathBuf::from("/home/me/.local/share/my-app"))
        );
        assert_eq!(app.data_dir_in(&[]), None);
    }

    #[cfg(windows)]
    #[test]
    fn the_data_directory_follows_the_platforms_convention() {
        let app = layout("my-app");
        assert_eq!(
            app.data_dir_in(&vars(&[("LOCALAPPDATA", r"C:\Users\me\AppData\Local")])),
            Some(PathBuf::from(r"C:\Users\me\AppData\Local\my-app"))
        );
        assert_eq!(
            app.data_dir_in(&vars(&[("APPDATA", r"C:\Users\me\AppData\Roaming")])),
            Some(PathBuf::from(r"C:\Users\me\AppData\Roaming\my-app"))
        );
    }

    #[test]
    fn a_namespace_separates_every_directory() {
        let host = vars(&[("HOME", "/home/me"), ("LOCALAPPDATA", r"C:\Local")]);
        let (app, other) = (layout("my-app"), layout("other-app"));
        assert_ne!(
            app.workspace(&host, "claude"),
            other.workspace(&host, "claude")
        );
        assert_ne!(app.data_dir_in(&host), other.data_dir_in(&host));
        for layout in [&app, &other] {
            let name = layout.namespace().as_str();
            let contains = |path: PathBuf| path.to_string_lossy().to_lowercase().contains(name);
            assert!(contains(layout.workspace(&host, "claude")));
            assert!(contains(layout.data_dir_in(&host).unwrap()));
        }
    }

    #[test]
    fn without_a_cache_the_workspace_gets_a_new_name() {
        let app = layout("my-app");
        let first = app.workspace(&vars(&[("HOME", "relative")]), "codex");
        let second = app.workspace(&[], "codex");
        for dir in [&first, &second] {
            assert_eq!(dir.parent(), Some(std::env::temp_dir().as_path()));
            let name = dir.file_name().unwrap().to_str().unwrap();
            assert!(name.starts_with("my-app-codex-"), "{name}");
        }
        assert_ne!(first, second);
    }
}
