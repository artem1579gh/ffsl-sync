//! Where a replica root lies relative to the pair's state directory and the
//! other replica's root (design §5.1, issue #6), checked every time a
//! replica is opened, served or not.
//!
//! A root that holds the state directory syncs its private key and index,
//! and the reports written there after every cycle start the next one, so a
//! daemon or `serve` never stops. A root inside the other one syncs into
//! itself. `init` checks both on the paths it is given; the config can
//! change later, symlinks can hide either, and a remote root is not checked
//! there at all. So the check here compares directories, not paths: their
//! `(st_dev, st_ino)`, up the chain of `..` from each, which is the chain
//! of real parents whatever symlinks the paths went through.
//!
//! The other replica's root is compared when this process opens it too
//! (a local pair), or else when its path here holds its root marker: then
//! it is the same directory (the loopback setup), not a namesake on another
//! host. (Under `serve`, the config's path of the client's replica is a
//! path on the client's host.) Its
//! marker in one of our root's ancestors also shows we are inside it, even
//! when its path does not lead here. One of the two replicas may be new and
//! have no marker yet; the other one's next open sees it.
//!
//! Not covered: the state directory reached through a bind mount whose
//! source lies inside the root (`..` at a mount's top leads to the mount
//! point's parent, not the source's).

use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::path::{Path, PathBuf};

use rustix::fs::{AtFlags, FileType, Mode, OFlags};

use crate::config::ReplicaConfig;
use crate::error::{Error, Result};
use crate::fs::root::Root;
use crate::fs::tmpname;

/// More levels than any real path has; a guard against a loop.
const MAX_DEPTH: usize = 4096;

/// Fails with [`Error::InvalidRoot`] if `root` holds `pair_dir` or lies
/// inside it, or overlaps the root of `peer` (the other replica of the pair;
/// `peer_here`: this process opens it too).
pub fn check(
    root: &Root,
    pair_dir: &Path,
    peer: Option<&ReplicaConfig>,
    peer_here: bool,
) -> Result<()> {
    let invalid = |reason: String| Error::InvalidRoot {
        path: root.path().to_owned(),
        reason,
    };
    let me = identity(root.fd()).ok_or_else(|| invalid("cannot stat it".into()))?;

    let state = open_dir(pair_dir)
        .map_err(|e| Error::io(format!("open state directory {}", pair_dir.display()), e))?;
    let state_id = identity(state.as_fd()).ok_or_else(|| {
        invalid(format!(
            "cannot stat state directory {}",
            pair_dir.display()
        ))
    })?;
    if find_up(state.as_fd(), |d| identity(d) == Some(me)).is_some() {
        return Err(invalid(format!(
            "contains the state directory {}",
            pair_dir.display()
        )));
    }
    if find_up(root.fd(), |d| identity(d) == Some(state_id)).is_some() {
        return Err(invalid(format!(
            "lies inside the state directory {}",
            pair_dir.display()
        )));
    }

    let Some(peer) = peer else {
        return Ok(());
    };
    let marker = tmpname::root_marker(peer.id);
    let inside = |found: Option<(usize, PathBuf)>| match found {
        Some((0, _)) => Some(invalid(format!(
            "is also the root of the other replica {}",
            peer.id
        ))),
        Some((_, dir)) => Some(invalid(format!(
            "lies inside {}, the root of the other replica {}",
            dir.display(),
            peer.id
        ))),
        None => None,
    };
    if let Some(e) = inside(find_up(root.fd(), |d| has_file(d, &marker))) {
        return Err(e);
    }
    // A peer opened here is compared even before it has its marker.
    let here = open_dir(&peer.root)
        .ok()
        .filter(|fd| peer_here || has_file(fd.as_fd(), &marker));
    if let Some(theirs) = here {
        let theirs_id = identity(theirs.as_fd());
        if let Some(e) = inside(find_up(root.fd(), |d| {
            theirs_id.is_some() && identity(d) == theirs_id
        })) {
            return Err(e);
        }
        if find_up(theirs.as_fd(), |d| identity(d) == Some(me)).is_some() {
            return Err(invalid(format!(
                "contains {}, the root of the other replica {}",
                peer.root.display(),
                peer.id
            )));
        }
    }
    Ok(())
}

fn open_dir(path: &Path) -> std::io::Result<OwnedFd> {
    rustix::fs::open(
        path,
        OFlags::PATH | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(Into::into)
}

/// `(st_dev, st_ino)` of the directory open at `dir`.
fn identity(dir: BorrowedFd<'_>) -> Option<(u64, u64)> {
    let st = rustix::fs::fstat(dir).ok()?;
    #[allow(clippy::unnecessary_cast, clippy::useless_conversion)]
    Some((st.st_dev as u64, st.st_ino as u64))
}

/// `dir` holds a regular file `name` (not followed if a symlink).
fn has_file(dir: BorrowedFd<'_>, name: &[u8]) -> bool {
    rustix::fs::statat(dir, name, AtFlags::SYMLINK_NOFOLLOW)
        .is_ok_and(|st| FileType::from_raw_mode(st.st_mode) == FileType::RegularFile)
}

/// The first of `dir` and its ancestors, up to `/`, that `accept` takes: how
/// many levels up it is, and its path (for messages). `None` also when an
/// ancestor cannot be opened.
fn find_up(
    dir: BorrowedFd<'_>,
    mut accept: impl FnMut(BorrowedFd<'_>) -> bool,
) -> Option<(usize, PathBuf)> {
    let mut cur = dir.try_clone_to_owned().ok()?;
    for depth in 0..MAX_DEPTH {
        if accept(cur.as_fd()) {
            return Some((depth, path_of(cur.as_fd())));
        }
        let up = match rustix::fs::openat(
            &cur,
            "..",
            OFlags::PATH | OFlags::DIRECTORY | OFlags::CLOEXEC,
            Mode::empty(),
        ) {
            Ok(up) => up,
            Err(e) => {
                tracing::debug!(dir = %path_of(cur.as_fd()).display(), error = %e, "cannot open the parent directory; layout check stops here");
                return None;
            }
        };
        // `..` of `/` is `/`.
        if identity(up.as_fd()).is_none() || identity(up.as_fd()) == identity(cur.as_fd()) {
            return None;
        }
        cur = up;
    }
    None
}

fn path_of(dir: BorrowedFd<'_>) -> PathBuf {
    use std::os::fd::AsRawFd;
    std::fs::read_link(format!("/proc/self/fd/{}", dir.as_raw_fd()))
        .unwrap_or_else(|_| PathBuf::from("<unknown>"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ReplicaId;
    use std::fs;
    use std::os::unix::fs::symlink;

    /// The peer at `root`, and whether this process opens it.
    fn peer(root: &Path, here: bool) -> (ReplicaConfig, bool) {
        let mut r = ReplicaConfig::new(root.to_owned()).unwrap();
        r.id = ReplicaId(0x1234);
        (r, here)
    }

    fn reason(
        root: &Path,
        pair_dir: &Path,
        peer: Option<&(ReplicaConfig, bool)>,
    ) -> Option<String> {
        let here = peer.is_some_and(|p| p.1);
        match check(
            &Root::open(root).unwrap(),
            pair_dir,
            peer.map(|p| &p.0),
            here,
        ) {
            Ok(()) => None,
            Err(Error::InvalidRoot { reason, .. }) => Some(reason),
            Err(e) => panic!("{e}"),
        }
    }

    #[test]
    fn state_directory_inside_or_around_the_root() {
        let t = tempfile::tempdir().unwrap();
        let (root, state) = (t.path().join("root"), t.path().join("state"));
        fs::create_dir_all(root.join(".local/state/ffsl-sync/p")).unwrap();
        fs::create_dir_all(state.join("p/inner")).unwrap();
        assert_eq!(reason(&root, &state.join("p"), None), None);

        let r = reason(&root, &root.join(".local/state/ffsl-sync/p"), None).unwrap();
        assert!(r.starts_with("contains the state directory"), "{r}");
        // A symlinked state home does not hide it.
        symlink(root.join(".local/state"), t.path().join("xdg")).unwrap();
        let r = reason(&root, &t.path().join("xdg/ffsl-sync/p"), None).unwrap();
        assert!(r.starts_with("contains the state directory"), "{r}");
        // Nor a symlinked root.
        symlink(&root, t.path().join("home")).unwrap();
        let r = reason(
            &t.path().join("home"),
            &root.join(".local/state/ffsl-sync/p"),
            None,
        )
        .unwrap();
        assert!(r.starts_with("contains the state directory"), "{r}");
        // The root itself as the state directory.
        assert!(reason(&root, &root, None).is_some());

        let r = reason(&state.join("p/inner"), &state.join("p"), None).unwrap();
        assert!(r.starts_with("lies inside the state directory"), "{r}");
    }

    #[test]
    fn overlapping_local_roots() {
        let t = tempfile::tempdir().unwrap();
        let state = t.path().join("state");
        let (a, b) = (t.path().join("a"), t.path().join("b"));
        fs::create_dir_all(a.join("sub")).unwrap();
        fs::create_dir_all(&b).unwrap();
        fs::create_dir_all(&state).unwrap();
        assert_eq!(reason(&a, &state, Some(&peer(&b, true))), None);

        let r = reason(&a, &state, Some(&peer(&a.join("sub"), true))).unwrap();
        assert!(r.starts_with("contains"), "{r}");
        let r = reason(&a.join("sub"), &state, Some(&peer(&a, true))).unwrap();
        assert!(r.starts_with("lies inside"), "{r}");
        let r = reason(&a, &state, Some(&peer(&a, true))).unwrap();
        assert!(r.starts_with("is also the root"), "{r}");
        // Through a symlink, which a lexical check misses.
        symlink(a.join("sub"), t.path().join("link")).unwrap();
        let r = reason(&a, &state, Some(&peer(&t.path().join("link"), true))).unwrap();
        assert!(r.starts_with("contains"), "{r}");
    }

    #[test]
    fn a_remote_root_is_compared_only_with_its_marker() {
        let t = tempfile::tempdir().unwrap();
        let state = t.path().join("state");
        let a = t.path().join("a");
        fs::create_dir_all(a.join("sub")).unwrap();
        fs::create_dir_all(&state).unwrap();
        let marker = |dir: &Path| {
            dir.join(String::from_utf8(tmpname::root_marker(ReplicaId(0x1234))).unwrap())
        };

        // The same path on another host.
        assert_eq!(reason(&a, &state, Some(&peer(&a, false))), None);
        assert_eq!(reason(&a, &state, Some(&peer(&a.join("sub"), false))), None);
        // Served here: its marker shows it.
        fs::write(marker(&a.join("sub")), "").unwrap();
        let r = reason(&a, &state, Some(&peer(&a.join("sub"), false))).unwrap();
        assert!(r.starts_with("contains"), "{r}");
        fs::remove_file(marker(&a.join("sub"))).unwrap();
        // Its marker above us, whatever its configured path.
        fs::write(marker(&a), "").unwrap();
        let r = reason(
            &a.join("sub"),
            &state,
            Some(&peer(Path::new("/elsewhere"), false)),
        )
        .unwrap();
        assert!(r.starts_with("lies inside"), "{r}");
        // A marker that is not a regular file does not count.
        fs::remove_file(marker(&a)).unwrap();
        fs::create_dir(marker(&a)).unwrap();
        assert_eq!(
            reason(
                &a.join("sub"),
                &state,
                Some(&peer(Path::new("/elsewhere"), false))
            ),
            None
        );
    }
}
