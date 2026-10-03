//! Bounded executable discovery cache. Positive hits are revalidated on every
//! lookup; newly installed higher-priority executables appear within five
//! seconds, or immediately after explicit invalidation. Misses are never kept.

use super::{SearchPath, is_executable};
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

pub const DISCOVERY_MAX_AGE: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileStamp {
    canonical: PathBuf,
    length: u64,
    modified: Option<SystemTime>,
    readonly: bool,
    #[cfg(windows)]
    identity: (u64, u64, u32),
    #[cfg(unix)]
    identity: (u64, u64, i64, i64, u32),
}

impl FileStamp {
    pub fn read(path: &Path) -> std::io::Result<Option<Self>> {
        let metadata = match fs::metadata(path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
        #[cfg(unix)]
        use std::os::unix::fs::MetadataExt;
        #[cfg(windows)]
        use std::os::windows::fs::MetadataExt;
        Ok(Some(Self {
            canonical: fs::canonicalize(path)?,
            length: metadata.len(),
            modified: metadata.modified().ok(),
            readonly: metadata.permissions().readonly(),
            #[cfg(windows)]
            identity: (
                metadata.creation_time(),
                metadata.last_write_time(),
                metadata.file_attributes(),
            ),
            #[cfg(unix)]
            identity: (
                metadata.dev(),
                metadata.ino(),
                metadata.ctime(),
                metadata.ctime_nsec(),
                metadata.mode(),
            ),
        }))
    }

    /// Track a directory's identity and permissions, without treating log or
    /// session-file creation as a change to the account configuration.
    pub fn read_directory(path: &Path) -> std::io::Result<Option<Self>> {
        let mut stamp = Self::read(path)?;
        if let Some(stamp) = &mut stamp {
            if !path.is_dir() {
                return Err(std::io::Error::other("expected a directory"));
            }
            stamp.length = 0;
            stamp.modified = None;
            #[cfg(unix)]
            {
                stamp.identity.2 = 0;
                stamp.identity.3 = 0;
            }
            #[cfg(windows)]
            {
                stamp.identity.1 = 0;
            }
        }
        Ok(stamp)
    }
}

#[derive(Debug)]
struct Found {
    path: PathBuf,
    stamp: FileStamp,
    at: Instant,
}

#[derive(Debug)]
pub struct CachedSearchPath {
    search: SearchPath,
    found: RefCell<BTreeMap<String, Found>>,
}

impl CachedSearchPath {
    pub fn new(search: SearchPath) -> Self {
        Self {
            search,
            found: RefCell::new(BTreeMap::new()),
        }
    }

    pub fn invalidate(&self) {
        self.found.borrow_mut().clear();
    }

    pub fn find(&self, name: &str) -> Option<PathBuf> {
        self.find_at(name, Instant::now())
    }

    fn find_at(&self, name: &str, now: Instant) -> Option<PathBuf> {
        let mut found = self.found.borrow_mut();
        if let Some(hit) = found.get(name) {
            if now.saturating_duration_since(hit.at) < DISCOVERY_MAX_AGE
                && is_executable(&hit.path)
                && FileStamp::read(&hit.path).ok().flatten().as_ref() == Some(&hit.stamp)
            {
                return Some(hit.path.clone());
            }
        }
        found.remove(name);
        let path = self.search.find(name)?;
        // A first-directory hit already costs one executable check. Caching
        // it would add identity/canonicalization work and make it slower.
        if path.parent() == self.search.dirs().first().map(PathBuf::as_path) {
            return Some(path);
        }
        if let Ok(Some(stamp)) = FileStamp::read(&path) {
            if found.len() >= 64 {
                found.clear();
            }
            found.insert(
                name.to_owned(),
                Found {
                    path: path.clone(),
                    stamp,
                    at: now,
                },
            );
        }
        Some(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn directory_fingerprints_ignore_history_but_notice_scope_replacement() {
        let root = super::super::tests::temp_dir("directory-stamp");
        let directory = root.join("account");
        assert_eq!(FileStamp::read_directory(&directory).unwrap(), None);
        fs::create_dir(&directory).unwrap();
        let before = FileStamp::read_directory(&directory).unwrap().unwrap();
        fs::write(directory.join("history.jsonl"), "new history").unwrap();
        fs::create_dir(directory.join("sessions")).unwrap();
        assert_eq!(
            FileStamp::read_directory(&directory).unwrap().as_ref(),
            Some(&before)
        );
        fs::rename(&directory, root.join("previous-account")).unwrap();
        fs::create_dir(&directory).unwrap();
        assert_ne!(
            FileStamp::read_directory(&directory).unwrap().as_ref(),
            Some(&before)
        );
        assert!(FileStamp::read_directory(&root.join("previous-account/history.jsonl")).is_err());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn replacement_removal_installation_and_priority_recover() {
        let root = super::super::tests::temp_dir("cache");
        let first = root.join("first");
        let second = root.join("second");
        fs::create_dir_all(&first).unwrap();
        fs::create_dir_all(&second).unwrap();
        let name = &super::super::file_names("codex")[0];
        let late = super::super::tests::install(&second, name);
        let cache = CachedSearchPath::new(SearchPath::new([first.clone(), second.clone()]));
        let now = Instant::now();
        assert_eq!(cache.find_at("codex", now), Some(late.clone()));
        fs::remove_file(&late).unwrap();
        assert_eq!(cache.find_at("codex", now), None);
        super::super::tests::install(&second, name);
        assert_eq!(cache.find_at("codex", now), Some(late.clone()));
        let early = super::super::tests::install(&first, name);
        assert_eq!(
            cache.find_at("codex", now + DISCOVERY_MAX_AGE),
            Some(early.clone())
        );
        fs::remove_file(&early).unwrap();
        assert_eq!(cache.find("codex"), Some(late.clone()));
        super::super::tests::install(&first, name);
        cache.invalidate();
        assert_eq!(cache.find("codex"), Some(early));
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn permissions_and_symlink_target_are_revalidated() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let root = super::super::tests::temp_dir("cache-link");
        let target = super::super::tests::install(&root, "target");
        symlink(&target, root.join("codex")).unwrap();
        let cache = CachedSearchPath::new(SearchPath::new([root.clone()]));
        assert!(cache.find("codex").is_some());
        fs::set_permissions(&target, fs::Permissions::from_mode(0o600)).unwrap();
        assert!(cache.find("codex").is_none());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    #[ignore = "manual microbenchmark; no timing assertion"]
    fn discovery_benchmark() {
        let root = super::super::tests::temp_dir("cache-bench");
        let dirs: Vec<_> = (0..128).map(|n| root.join(n.to_string())).collect();
        for dir in &dirs {
            fs::create_dir_all(dir).unwrap();
        }
        super::super::tests::install(dirs.last().unwrap(), &super::super::file_names("codex")[0]);
        let search = SearchPath::new(dirs);
        let cache = CachedSearchPath::new(search.clone());
        cache.find("codex");
        let start = Instant::now();
        for _ in 0..10_000 {
            std::hint::black_box(search.find("codex"));
        }
        let scans = start.elapsed();
        let start = Instant::now();
        for _ in 0..10_000 {
            std::hint::black_box(cache.find("codex"));
        }
        eprintln!(
            "discovery 128 directories, 10000 lookups: scan={scans:?}, cached={:?}",
            start.elapsed()
        );
        fs::remove_dir_all(root).unwrap();
    }
}
