//! The directory Codex runs in (SEC-02).
//!
//! Codex follows instructions it finds where it runs: `AGENTS.md` in its
//! working directory and, when a directory above it holds `.git`, in each
//! directory from that one down. So Codex runs in an empty directory of its
//! own, and on POSIX nobody but its user, or root, may be able to change that
//! directory or any directory above it. Otherwise another user could plant
//! files there for Codex to follow, or put another directory in its place.
//! A sticky directory such as `/tmp` doesn't qualify: anyone can still add
//! `.git` and `AGENTS.md` to it. A directory its group can write to
//! qualifies only if the group is the user's own (see [`private_group`]).

use std::io;
use std::path::{Path, PathBuf};

/// Creates the workspace `dir` if needed and checks that only its user can
/// change what Codex finds there. Returns the path to give Codex: on POSIX,
/// `dir` with every link on the way resolved, so what was checked is what
/// Codex gets.
///
/// On POSIX, the workspace must be a directory of the user's own, not a
/// link, and it is made readable by its user alone (mode 0700). Every
/// directory above it, as named and as resolved, must belong to the user or
/// root and be writable by its owner alone, or also by the user's private
/// group, and every link on the way must belong to the user or root. Only
/// owners and permission bits are read, not access-control lists.
///
/// On Windows, where reading access-control lists needs platform bindings,
/// the workspace must be a directory, not a link or junction. It is in the
/// user's local application data, which Windows keeps private to the user.
pub fn prepare(dir: &Path) -> io::Result<PathBuf> {
    #[cfg(unix)]
    {
        prepare_for(dir, nix::unistd::geteuid().as_raw())
    }
    #[cfg(not(unix))]
    {
        std::fs::create_dir_all(dir)?;
        if !std::fs::symlink_metadata(dir)?.is_dir() {
            return Err(not_private(dir));
        }
        Ok(dir.to_path_buf())
    }
}

/// [`prepare`] for the user whose ID is `user`.
#[cfg(unix)]
fn prepare_for(dir: &Path, user: u32) -> io::Result<PathBuf> {
    use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};

    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)?;
    // Not following a link: the directory itself.
    let workspace = std::fs::symlink_metadata(dir)?;
    if !workspace.is_dir() || workspace.uid() != user {
        return Err(not_private(dir));
    }
    let resolved = std::fs::canonicalize(dir)?;
    for above in dir.ancestors().skip(1).chain(resolved.ancestors().skip(1)) {
        let entry = std::fs::symlink_metadata(above)?;
        if !trusted(
            entry.uid(),
            entry.mode(),
            entry.file_type().is_symlink(),
            user,
            || private_group(entry.gid(), user),
        ) {
            return Err(not_private(above));
        }
    }
    if workspace.mode() & 0o7777 != 0o700 {
        std::fs::set_permissions(&resolved, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(resolved)
}

/// Whether an entry on the way to the workspace, with this owner and mode,
/// is safe from everyone but `user` and root. A link's own permission bits
/// don't matter: only its owner can change it. A directory of the user's
/// that its group can write to is safe if `private_group` says the group is
/// the user's own.
#[cfg(unix)]
fn trusted(
    owner: u32,
    mode: u32,
    link: bool,
    user: u32,
    private_group: impl FnOnce() -> bool,
) -> bool {
    if owner != user && owner != 0 {
        return false;
    }
    link || (mode & 0o002 == 0 && (mode & 0o020 == 0 || (owner == user && private_group())))
}

/// Whether the group `gid` is the private group of `user`, the kind most
/// Linux distributions give each user: the user's primary group, named after
/// the user, with nobody else listed as a member. There the usual umask, 002,
/// lets the group write to new directories, and that lets nobody else write.
/// macOS's `staff`, which every user shares, isn't one.
#[cfg(unix)]
fn private_group(gid: u32, user: u32) -> bool {
    use nix::unistd::{Gid, Group, Uid, User};

    match (
        User::from_uid(Uid::from_raw(user)),
        Group::from_gid(Gid::from_raw(gid)),
    ) {
        (Ok(Some(account)), Ok(Some(group))) => own_group(
            &account.name,
            account.gid.as_raw(),
            &group.name,
            gid,
            &group.mem,
        ),
        _ => false,
    }
}

/// Whether the group named `group`, with ID `gid` and `members`, is the
/// private group of the user named `name` whose primary group is `primary`.
#[cfg(unix)]
fn own_group(name: &str, primary: u32, group: &str, gid: u32, members: &[String]) -> bool {
    primary == gid && group == name && members.iter().all(|member| member == name)
}

fn not_private(path: &Path) -> io::Error {
    io::Error::new(
        io::ErrorKind::PermissionDenied,
        format!("{} isn't private to its user", path.display()),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A new directory for one test, beside the test binary in the target
    /// directory: the temporary directory can't hold a workspace, because
    /// other users can write to it.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new(name: &str) -> Self {
            let exe = std::env::current_exe().unwrap();
            let dir = exe
                .parent()
                .and_then(Path::parent)
                .unwrap()
                .join(format!("workspace-test-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir(&dir).unwrap();
            // Whatever the umask, others can't change it.
            #[cfg(unix)]
            set_mode(&dir, 0o700);
            Self(dir)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[cfg(unix)]
    fn mode(path: &Path) -> u32 {
        use std::os::unix::fs::PermissionsExt;
        std::fs::symlink_metadata(path)
            .unwrap()
            .permissions()
            .mode()
            & 0o7777
    }

    #[cfg(unix)]
    fn set_mode(path: &Path, mode: u32) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
    }

    #[cfg(unix)]
    fn me() -> u32 {
        nix::unistd::geteuid().as_raw()
    }

    #[cfg(unix)]
    #[test]
    fn the_workspace_is_created_for_its_user_alone() {
        let scratch = Scratch::new("new");
        let dir = scratch.0.join("cache/codex-workspace");
        let resolved = prepare(&dir).unwrap();
        assert_eq!(resolved, std::fs::canonicalize(&dir).unwrap());
        for created in [&scratch.0.join("cache"), &dir] {
            assert_eq!(mode(created), 0o700, "{}", created.display());
        }
        // Already there: the same directory again.
        assert_eq!(prepare(&dir).unwrap(), resolved);
    }

    #[cfg(unix)]
    #[test]
    fn a_workspace_others_could_open_is_closed_to_them() {
        let scratch = Scratch::new("open");
        let dir = scratch.0.join("codex-workspace");
        for open in [0o755, 0o777, 0o1777, 0o770] {
            std::fs::create_dir(&dir).unwrap();
            set_mode(&dir, open);
            prepare(&dir).unwrap();
            assert_eq!(mode(&dir), 0o700, "{open:o}");
            std::fs::remove_dir(&dir).unwrap();
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_workspace_that_is_a_link_or_someone_elses_is_refused() {
        let scratch = Scratch::new("link");
        let target = scratch.0.join("elsewhere");
        std::fs::create_dir(&target).unwrap();
        set_mode(&target, 0o700);
        let dir = scratch.0.join("codex-workspace");
        std::os::unix::fs::symlink(&target, &dir).unwrap();
        let error = prepare(&dir).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        // A link to nowhere can't be made a directory either.
        std::fs::remove_file(&dir).unwrap();
        std::os::unix::fs::symlink(scratch.0.join("missing"), &dir).unwrap();
        assert!(prepare(&dir).is_err());
        std::fs::remove_file(&dir).unwrap();

        // Another user's directory, however open, stays theirs.
        std::fs::create_dir(&dir).unwrap();
        assert!(prepare_for(&dir, me().wrapping_add(1)).is_err());
        prepare_for(&dir, me()).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn a_directory_above_that_others_can_change_is_refused() {
        let scratch = Scratch::new("above");
        let cache = scratch.0.join("cache");
        let dir = cache.join("my-app/codex-workspace");
        prepare(&dir).unwrap();
        // Sticky or not, others could add `.git` and `AGENTS.md` to it.
        for open in [0o777, 0o1777, 0o757] {
            set_mode(&cache, open);
            assert_eq!(
                prepare(&dir).unwrap_err().kind(),
                io::ErrorKind::PermissionDenied,
                "{open:o}"
            );
        }
        // Its group may write to it only if that is the user's own group.
        use std::os::unix::fs::MetadataExt;
        set_mode(&cache, 0o775);
        let own = private_group(std::fs::metadata(&cache).unwrap().gid(), me());
        assert_eq!(prepare(&dir).is_ok(), own);
        set_mode(&cache, 0o755);
        prepare(&dir).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn links_on_the_way_are_resolved_and_checked_both_ways() {
        let scratch = Scratch::new("resolved");
        let private = scratch.0.join("private");
        let open = scratch.0.join("open");
        std::fs::create_dir(&private).unwrap();
        std::fs::create_dir(&open).unwrap();
        set_mode(&private, 0o700);
        set_mode(&open, 0o777);

        // Through a link to a private directory: Codex gets the real path.
        let link = scratch.0.join("to-private");
        std::os::unix::fs::symlink(&private, &link).unwrap();
        let resolved = prepare(&link.join("codex-workspace")).unwrap();
        assert_eq!(
            resolved,
            std::fs::canonicalize(&private)
                .unwrap()
                .join("codex-workspace")
        );

        // A link to a directory others can change.
        let link = scratch.0.join("to-open");
        std::os::unix::fs::symlink(&open, &link).unwrap();
        assert!(prepare(&link.join("codex-workspace")).is_err());

        // A link to a private directory, kept where others could replace it.
        let swappable = open.join("to-private");
        std::os::unix::fs::symlink(&private, &swappable).unwrap();
        assert!(prepare(&swappable.join("codex-workspace")).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn only_the_user_and_root_are_trusted_above_the_workspace() {
        let user = 501;
        let own = || true;
        let shared = || false;
        // Directories: the user's or root's, and writable by the owner alone.
        assert!(trusted(user, 0o040_755, false, user, shared));
        assert!(trusted(0, 0o040_755, false, user, shared));
        assert!(trusted(user, 0o040_700, false, user, shared));
        assert!(!trusted(502, 0o040_755, false, user, own));
        assert!(!trusted(0, 0o041_777, false, user, own));
        assert!(!trusted(user, 0o040_757, false, user, own));
        // Or also by the group, if it is the user's own.
        assert!(trusted(user, 0o040_775, false, user, own));
        assert!(trusted(user, 0o040_770, false, user, own));
        assert!(!trusted(user, 0o040_775, false, user, shared));
        assert!(!trusted(0, 0o040_775, false, user, own));
        assert!(!trusted(user, 0o040_777, false, user, own));
        // Links: the user's or root's, whatever their bits.
        assert!(trusted(0, 0o120_777, true, user, shared));
        assert!(trusted(user, 0o120_777, true, user, shared));
        assert!(!trusted(502, 0o120_777, true, user, own));
    }

    #[cfg(unix)]
    #[test]
    fn a_private_group_is_the_users_own_alone() {
        let alone: &[String] = &[];
        let listed = ["me".to_owned()];
        let others = ["me".to_owned(), "you".to_owned()];
        assert!(own_group("me", 1000, "me", 1000, alone));
        assert!(own_group("me", 1000, "me", 1000, &listed));
        // Someone else in it.
        assert!(!own_group("me", 1000, "me", 1000, &others));
        assert!(!own_group("me", 1000, "me", 1000, &others[1..]));
        // Not the user's primary group, or not named after the user, such as
        // macOS's `staff`.
        assert!(!own_group("me", 1000, "me", 1001, alone));
        assert!(!own_group("me", 20, "staff", 20, alone));
        // Some other group, which may not even exist.
        assert!(!private_group(u32::MAX - 1, me()));
    }

    #[cfg(windows)]
    #[test]
    fn a_workspace_that_is_a_link_is_refused() {
        let scratch = Scratch::new("link");
        let dir = scratch.0.join("cache").join("codex-workspace");
        assert_eq!(prepare(&dir).unwrap(), dir);
        assert_eq!(prepare(&dir).unwrap(), dir);
        // Making a link takes a privilege Windows may not grant.
        let link = scratch.0.join("link");
        if std::os::windows::fs::symlink_dir(&dir, &link).is_ok() {
            assert!(prepare(&link).is_err());
        }
    }
}
