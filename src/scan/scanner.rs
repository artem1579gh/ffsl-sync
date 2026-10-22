//! The tree scanner: walks a replica with fds only, and brings its index up to
//! date (design §3, §4.5, §5.2).
//!
//! - **Traversal:** each child directory is opened from its parent's fd with
//!   `openat2(RESOLVE_BENEATH | NO_SYMLINKS | NO_MAGICLINKS | NO_XDEV)` and
//!   checked against the `statx` taken from the listing. Reserved `.~ffsl.`
//!   names are skipped. The (dev, ino) of every directory on the current
//!   traversal stack is kept, so a followed link back to an ancestor becomes
//!   `Unmanaged(Loop)` instead of recursing forever.
//! - **Symlinks:** classified by [`classify`] on the canonical (unmunged)
//!   target. A followed link is opened beneath the root
//!   (`RESOLVE_BENEATH | NO_MAGICLINKS | NO_XDEV`, from the root fd and the
//!   link's own path) or, when that escapes, from the link's directory with
//!   `RESOLVE_NO_MAGICLINKS` only, as an out-of-tree referent (§4.5). The link
//!   must be unchanged (same inode, so same target) after its referent is
//!   read or opened. With `-K` ([`Scanner::keep_dirlinks`]), a link adopted
//!   earlier (or named in [`Scanner::adopt`]) is indexed as the directory it
//!   points to while that is still the pinned directory (§4.3.1). A link
//!   the replica wrote munged because the peer's target would lead out of
//!   the root (§4.6) is indexed as the plain symlink it was made for, never
//!   followed or adopted.
//! - **Files:** not rehashed when (ino, size, mtime, ctime) match the index and
//!   the entry is not `racy`; otherwise hashed with a stable read (§5.2). A
//!   file whose ctime is within the racy window of the scan start is marked
//!   `racy` and is rehashed by the next scan.
//! - **Index updates:** a logical change (kind, content, target, mode; mtime
//!   for files only) is written with `vv.bump_after(replica, max_counter)` and
//!   a new seq. A change to [`LocalMeta`] alone is written in place, keeping
//!   the seq. Indexed paths in the scanned region that are gone from disk
//!   become tombstones: after listing a directory, its children in the index
//!   that the listing lacks, with their subtrees; after finding a child gone
//!   or not a directory, what the index has beneath it. No set of the paths
//!   found is kept (issue #15). Every write checks that the entry still has
//!   the seq the scan started from; otherwise the path is reported dirty.
//! - **Unstable paths** (a file changing while it is read, a symlink on a
//!   path, a name replaced between two steps) leave their entry, and their
//!   whole subtree, unchanged and are reported in [`ScanStats::dirty`]. Other
//!   per-path errors (e.g. `EACCES`, a mount point) do the same and are
//!   reported in [`ScanStats::errors`].

use std::collections::HashSet;
use std::io;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rustix::fs::{Dir, OFlags};
use rustix::io::Errno;
use serde::{Deserialize, Serialize};

use crate::config::ReplicaId;
use crate::error::{Error, Result};
use crate::fs::hooks;
use crate::fs::root::{open_beneath, reopen_file, unstable_reason};
use crate::fs::{FileKind, Fingerprint, RelPath, Root, is_reserved};
use crate::index::{
    Entry, IndexStore, Kind, LinkInfo, LocalMeta, ReadTxn, UnmanagedReason, sync_mode,
};
use crate::scan::hasher::Hasher;
use crate::symlink::{SymlinkPolicy, Treatment, classify, is_munged, needs_referent, unmunge};

/// Default racy window: a file whose ctime is this close to the scan start
/// (or later) is rehashed by the next scan. Generous next to a kernel
/// timestamp tick (1–10 ms), to absorb the coarse clock's lag.
pub const DEFAULT_RACY_WINDOW: Duration = Duration::from_secs(1);

/// The longest path the scanner indexes, in bytes: the kernel's `PATH_MAX`
/// (4096) less its terminating NUL. A longer one could be indexed but never
/// opened again (reads and commits hand it, or its parent, to `openat2`
/// from the root), so it is an error, and nothing beneath it is scanned
/// (issue #14). It also bounds the depth of a scan, so its open fds.
pub const MAX_PATH_LEN: usize = 4095;

/// Pending index updates are committed in batches of this size.
const FLUSH_EVERY: usize = 1024;

/// What to scan.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Scope {
    /// The whole tree.
    Full,
    /// These paths and everything beneath them. The ancestors of each path
    /// are observed too (not recursively), so they are indexed.
    Paths(#[serde(deserialize_with = "crate::budget::vec")] Vec<RelPath>),
}

/// What a scan did.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScanStats {
    /// Objects examined (each name `statx`ed).
    pub scanned: u64,
    /// Files whose content was read and hashed.
    pub hashed: u64,
    /// Entries written with a version bump (tombstones included).
    pub changed: u64,
    /// Of `changed`, the new tombstones.
    pub tombstoned: u64,
    /// Paths that changed while being scanned, or whose index entry changed
    /// during the scan. Their entries (and subtrees) were left as they were;
    /// rescan them later.
    #[serde(deserialize_with = "crate::budget::vec")]
    pub dirty: Vec<RelPath>,
    /// Paths that could not be scanned for another reason (logged). Their
    /// entries and subtrees were left as they were.
    #[serde(deserialize_with = "crate::budget::vec")]
    pub errors: Vec<RelPath>,
}

/// The most paths a [`Scope::Paths`] or a list of [`ScanStats`] holds on
/// the wire ([`Scope::bounded`], [`ScanStats::bound`]; issue #5).
pub const MAX_LISTED: usize = 10_000;

/// The most bytes such a list holds on the wire, about.
pub const MAX_LISTED_BYTES: usize = 4 << 20;

impl Scope {
    /// A scope that covers this one in at most [`MAX_LISTED`] paths of
    /// [`MAX_LISTED_BYTES`] (see [`coalesce`]).
    pub fn bounded(self) -> Scope {
        match self {
            Scope::Paths(paths) => Scope::Paths(coalesce(paths)),
            full => full,
        }
    }
}

impl ScanStats {
    /// Bounds the lists to [`MAX_LISTED`] paths of [`MAX_LISTED_BYTES`]
    /// each, so the stats fit in a message whatever the tree: `dirty` is
    /// [`coalesce`]d (rescanning an ancestor rescans the paths beneath it),
    /// `errors` is cut short. Returns how many errors were left out.
    pub fn bound(&mut self) -> usize {
        self.dirty = coalesce(std::mem::take(&mut self.dirty));
        let mut bytes = 0;
        let keep = self
            .errors
            .iter()
            .take(MAX_LISTED)
            .take_while(|p| {
                bytes += listed_bytes(p);
                bytes <= MAX_LISTED_BYTES
            })
            .count();
        let left_out = self.errors.len() - keep;
        self.errors.truncate(keep);
        left_out
    }
}

/// About the bytes `p` takes in a list on the wire.
fn listed_bytes(p: &RelPath) -> usize {
    p.as_bytes().len() + 5
}

/// `paths` if they fit in [`MAX_LISTED`] paths of [`MAX_LISTED_BYTES`];
/// otherwise, sorted, the fewest ancestors needed so that every path is one
/// of them or beneath one, cut at the greatest depth at which the list
/// fits (at worst, the root alone).
pub fn coalesce(paths: Vec<RelPath>) -> Vec<RelPath> {
    let fits = |paths: &[RelPath]| {
        paths.len() <= MAX_LISTED
            && paths.iter().map(listed_bytes).sum::<usize>() <= MAX_LISTED_BYTES
    };
    if fits(&paths) {
        return paths;
    }
    let cut = |depth: usize| covering(paths.iter().map(|p| ancestor_at(p, depth)).collect());
    // The cut list shrinks with the depth; at depth 0 it is the root alone.
    let (mut lo, mut hi) = (0, paths.iter().map(RelPath::depth).max().unwrap_or(0));
    while lo < hi {
        let mid = lo + (hi - lo).div_ceil(2);
        if fits(&cut(mid)) {
            lo = mid;
        } else {
            hi = mid - 1;
        }
    }
    let mut out = cut(lo);
    out.sort();
    out
}

/// `p` if it is at most `depth` components deep, else its ancestor at
/// that depth.
fn ancestor_at(p: &RelPath, depth: usize) -> RelPath {
    let b = p.as_bytes();
    let end = match depth.checked_sub(1) {
        None => 0,
        Some(n) => match b.iter().enumerate().filter(|&(_, &c)| c == b'/').nth(n) {
            Some((i, _)) => i,
            None => return p.clone(),
        },
    };
    RelPath::new(&b[..end]).expect("a prefix of a path up to a `/` is a path")
}

/// `paths` without duplicates and without paths beneath another one, in
/// time linear in their bytes (after sorting): sorted by components, a
/// path's descendants follow it.
fn covering(mut paths: Vec<RelPath>) -> Vec<RelPath> {
    paths.sort_by(|a, b| a.components().cmp(b.components()));
    let mut out: Vec<RelPath> = Vec::new();
    for p in paths {
        // `p` is the last one kept, or beneath it.
        let covered = out.last().is_some_and(|q| {
            q.is_root()
                || p.as_bytes()
                    .strip_prefix(q.as_bytes())
                    .is_some_and(|rest| rest.first().is_none_or(|&c| c == b'/'))
        });
        if !covered {
            out.push(p);
        }
    }
    out
}

/// Scans one replica into its index.
#[derive(Debug)]
pub struct Scanner<'a> {
    root: &'a Root,
    index: &'a IndexStore,
    policy: SymlinkPolicy,
    munge_links: bool,
    keep_dirlinks: bool,
    keep_dirlinks_unsafe: bool,
    adopt: HashSet<RelPath>,
    racy_window: Duration,
    /// Paths treated as changing while they are scanned (tests).
    #[cfg(test)]
    dirty: HashSet<RelPath>,
}

impl<'a> Scanner<'a> {
    /// A scanner for `root` that records into `index` (whose replica ID is the
    /// one bumped), under the symlink `policy`.
    pub fn new(root: &'a Root, index: &'a IndexStore, policy: SymlinkPolicy) -> Scanner<'a> {
        Scanner {
            root,
            index,
            policy,
            munge_links: false,
            keep_dirlinks: false,
            keep_dirlinks_unsafe: false,
            adopt: HashSet::new(),
            racy_window: DEFAULT_RACY_WINDOW,
            #[cfg(test)]
            dirty: HashSet::new(),
        }
    }

    /// rsync `-K` on this replica (design §4.3): links adopted earlier stay
    /// adopted while they resolve to their pinned directory. With `unsafe_ok`
    /// (`--keep-dirlinks-unsafe`), directories outside the root qualify too.
    pub fn keep_dirlinks(mut self, on: bool, unsafe_ok: bool) -> Self {
        self.keep_dirlinks = on;
        self.keep_dirlinks_unsafe = unsafe_ok;
        self
    }

    /// With [`keep_dirlinks`](Self::keep_dirlinks): adopts the symlinks at
    /// `paths` that resolve to a directory (and are not followed anyway),
    /// indexing them as that directory from now on.
    pub fn adopt(mut self, paths: impl IntoIterator<Item = RelPath>) -> Self {
        self.adopt.extend(paths);
        self
    }

    /// rsync `--munge-links` on this replica: targets on disk are munged
    /// and are indexed unmunged (design §4.4).
    pub fn munge_links(mut self, on: bool) -> Self {
        self.munge_links = on;
        self
    }

    /// Overrides [`DEFAULT_RACY_WINDOW`].
    pub fn racy_window(mut self, window: Duration) -> Self {
        self.racy_window = window;
        self
    }

    /// Scans `scope` and updates the index. Fails only if the root or the
    /// index fails; per-path problems are reported in [`ScanStats`].
    pub fn scan(&self, scope: &Scope) -> Result<ScanStats> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| i64::try_from(d.as_nanos()).unwrap_or(i64::MAX));
        let window = i64::try_from(self.racy_window.as_nanos()).unwrap_or(i64::MAX);
        let mut walk = Walk {
            root: self.root,
            index: self.index,
            policy: self.policy,
            munge_links: self.munge_links,
            keep_dirlinks: self.keep_dirlinks,
            keep_dirlinks_unsafe: self.keep_dirlinks_unsafe,
            adopt: &self.adopt,
            #[cfg(test)]
            dirty: &self.dirty,
            replica: self.index.replica(),
            racy_cutoff: now.saturating_sub(window),
            snap: self.index.read()?,
            hasher: Hasher::new(),
            stack: Vec::new(),
            noted: HashSet::new(),
            protected: HashSet::new(),
            regions: Vec::new(),
            pending: Vec::new(),
            stats: ScanStats::default(),
            #[cfg(any(test, feature = "hooks"))]
            oracle: Some(Oracle::default()),
        };
        match scope {
            Scope::Full => walk.scan_path(&RelPath::root())?,
            Scope::Paths(paths) => {
                for p in outermost(paths) {
                    walk.scan_path(&p)?;
                }
            }
        }
        #[cfg(any(test, feature = "hooks"))]
        walk.check_tombstones()?;
        walk.flush()?;
        let mut stats = walk.stats;
        stats.hashed = walk.hasher.hashed();
        stats.dirty.sort();
        stats.dirty.dedup();
        stats.errors.sort();
        stats.errors.dedup();
        Ok(stats)
    }
}

/// `paths` without duplicates and without paths beneath another one.
fn outermost(paths: &[RelPath]) -> Vec<RelPath> {
    let mut sorted: Vec<&RelPath> = paths.iter().collect();
    sorted.sort_by_key(|p| (p.depth(), p.as_bytes()));
    let mut kept: HashSet<&[u8]> = HashSet::new();
    let mut out = Vec::new();
    for p in sorted {
        if !prefixes(p).any(|pre| kept.contains(pre)) {
            kept.insert(p.as_bytes());
            out.push(p.clone());
        }
    }
    out
}

/// The root (empty), every ancestor of `p`, and `p` itself, as raw bytes.
fn prefixes(p: &RelPath) -> impl Iterator<Item = &[u8]> {
    let b = p.as_bytes();
    std::iter::once(&b[..0])
        .chain(
            b.iter()
                .enumerate()
                .filter(|&(_, &c)| c == b'/')
                .map(move |(i, _)| &b[..i]),
        )
        .chain((!b.is_empty()).then_some(b))
}

/// How a visit went, for what lies beneath the name.
enum Visit {
    /// A directory, open to descend into.
    Dir(OpenDir),
    /// Something else (a file, a symlink, an unmanaged object): nothing is
    /// beneath it on disk.
    Leaf,
    /// Not there.
    Gone,
    /// Left as indexed (dirty, failed, already protected), with everything
    /// beneath it.
    Kept,
}

/// What one name turned out to be.
enum Obs {
    /// Not there (or removed before we could look at it).
    Absent,
    /// Changed while we looked; leave its entry alone and rescan later.
    Dirty(&'static str),
    /// Could not be scanned; leave its entry alone.
    Failed(Error),
    Found(Found),
}

/// An observed object, in index terms.
struct Found {
    kind: Kind,
    mode: u32,
    mtime_ns: i64,
    local: LocalMeta,
    /// For a directory: an open fd to descend into.
    dir: Option<OpenDir>,
}

/// A directory being listed by [`Walk::walk_dir`].
struct Level {
    fd: OwnedFd,
    path: RelPath,
    /// The names not visited yet.
    names: std::vec::IntoIter<Vec<u8>>,
    /// The names the index has entries beneath (sorted): only beneath these
    /// can a name found not to be a directory leave entries to tombstone.
    deep: Vec<Vec<u8>>,
}

/// The tombstone pass the scanner had before issue #15, kept as an oracle
/// (tests and the `hooks` feature): the paths found, and the tombstones the
/// walk queued, which [`Walk::check_tombstones`] compares with what the old
/// pass would have queued.
#[cfg(any(test, feature = "hooks"))]
#[derive(Default)]
struct Oracle {
    seen: HashSet<Vec<u8>>,
    tombstoned: std::collections::BTreeSet<RelPath>,
}

struct OpenDir {
    fd: OwnedFd,
    id: (u64, u64),
    /// Reached through a followed symlink.
    followed: bool,
}

/// A queued index write; `base` is the seq the decision was made from.
struct Update {
    path: RelPath,
    base: Option<u64>,
    change: Change,
}

enum Change {
    /// A logical change: a new entry with a bumped version vector.
    New {
        kind: Kind,
        mode: u32,
        mtime_ns: i64,
        local: LocalMeta,
    },
    /// Only `LocalMeta` changed: rewrite in place, same seq.
    Local(LocalMeta),
}

/// The state of one scan.
struct Walk<'a> {
    root: &'a Root,
    index: &'a IndexStore,
    policy: SymlinkPolicy,
    munge_links: bool,
    keep_dirlinks: bool,
    keep_dirlinks_unsafe: bool,
    adopt: &'a HashSet<RelPath>,
    #[cfg(test)]
    dirty: &'a HashSet<RelPath>,
    replica: ReplicaId,
    /// Files with a ctime at or after this are `racy`.
    racy_cutoff: i64,
    /// The index as of the scan start.
    snap: ReadTxn,
    hasher: Hasher,
    /// (dev, ino) of the directories on the current traversal path.
    stack: Vec<(u64, u64)>,
    /// Paths visited on the way to a scoped path (its ancestors, and where
    /// the way ended): an ancestor shared by several is recorded once.
    noted: HashSet<Vec<u8>>,
    /// Dirty or failed paths: they and their subtrees keep their entries.
    protected: HashSet<Vec<u8>>,
    /// Roots of the subtrees scanned recursively; tombstones are limited to these.
    regions: Vec<RelPath>,
    pending: Vec<Update>,
    stats: ScanStats,
    /// `None` once a write failed at commit: the old pass would have kept
    /// that path's subtree, the walk had tombstoned it already.
    #[cfg(any(test, feature = "hooks"))]
    oracle: Option<Oracle>,
}

impl Walk<'_> {
    /// Observes the ancestors of `p` one by one from the root, then `p`
    /// recursively. If an ancestor is not a real directory (gone, a file, a
    /// symlink), that ancestor is scanned recursively instead.
    fn scan_path(&mut self, p: &RelPath) -> Result<()> {
        if self
            .regions
            .iter()
            .any(|r| prefixes(p).any(|pre| pre == r.as_bytes()))
        {
            return Ok(());
        }
        // The walk runs from this small frame, not from `reach`'s: in a deep
        // tree, the index writes beneath it need the stack (issue #14).
        let Some((region, dir)) = self.reach(p)? else {
            return Ok(());
        };
        if let Some(fd) = dir {
            let res = self.walk_dir(fd, &region);
            if !region.is_root() {
                self.stack.pop();
            }
            res?;
        }
        self.regions.push(region);
        Ok(())
    }

    /// Observes the ancestors of `p`, then `p`, up to the first that is not
    /// a real directory: the region to scan recursively, with its directory
    /// to walk (its id pushed on [`Walk::stack`]) if it is one. What the
    /// index has beneath a region found gone or not a directory is
    /// tombstoned here. `None` for a reserved name.
    fn reach(&mut self, p: &RelPath) -> Result<Option<(RelPath, Option<OwnedFd>)>> {
        let mut dir = self.root.open_dir(&RelPath::root())?;
        let root_fp = Fingerprint::of_fd(dir.as_fd())?;
        self.stack = vec![(root_fp.dev, root_fp.ino)];
        if p.is_root() {
            return Ok(Some((RelPath::root(), Some(dir))));
        }
        let names: Vec<&[u8]> = p.components().collect();
        let mut cur = RelPath::root();
        for (i, name) in names.iter().enumerate() {
            if is_reserved(name) {
                return Ok(None);
            }
            cur = cur.join(name)?;
            let last = i + 1 == names.len();
            let again = self.noted.contains(cur.as_bytes());
            let visit = self.visit(dir.as_fd(), &cur, name)?;
            self.noted.insert(cur.as_bytes().to_vec());
            match visit {
                Visit::Dir(d) if !last && !d.followed => {
                    self.stack.push(d.id);
                    dir = d.fd;
                    continue;
                }
                Visit::Dir(d) => {
                    self.stack.push(d.id);
                    return Ok(Some((cur, Some(d.fd))));
                }
                // A directory on the way to another scoped path a moment ago.
                Visit::Leaf | Visit::Gone if again => self.replaced(&cur),
                Visit::Leaf => self.tombstone_beneath(&cur, false)?,
                Visit::Gone => self.tombstone_beneath(&cur, true)?,
                Visit::Kept => {}
            }
            break;
        }
        Ok(Some((cur, None)))
    }

    /// Visits everything beneath the directory `fd` (at `path`, whose id is
    /// on `self.stack`), depth first. With an explicit stack of the levels
    /// being listed, not by recursion: a deep tree costs heap, not the
    /// thread's stack (a `serve` connection thread has 2 MiB; issue #14).
    /// Each level holds its directory open, at most [`MAX_PATH_LEN`] / 2.
    fn walk_dir(&mut self, fd: OwnedFd, path: &RelPath) -> Result<()> {
        let Some(top) = self.list(fd, path.clone())? else {
            return Ok(());
        };
        let mut levels = vec![top];
        while let Some(level) = levels.last_mut() {
            let Some(name) = level.names.next() else {
                levels.pop();
                // The top level's id is the caller's.
                if !levels.is_empty() {
                    self.stack.pop();
                }
                continue;
            };
            let child = level.path.join(&name)?;
            let deep = level.deep.binary_search(&name).is_ok();
            let visit = self.visit(level.fd.as_fd(), &child, &name)?;
            match visit {
                Visit::Dir(d) => {
                    self.stack.push(d.id);
                    match self.list(d.fd, child)? {
                        Some(level) => levels.push(level),
                        None => {
                            self.stack.pop();
                        }
                    }
                }
                Visit::Leaf if deep => self.tombstone_beneath(&child, false)?,
                Visit::Gone => self.tombstone_beneath(&child, true)?,
                Visit::Leaf | Visit::Kept => {}
            }
            if self.pending.len() >= FLUSH_EVERY {
                self.flush()?;
            }
        }
        Ok(())
    }

    /// The names in the directory `fd` (at `path`), to visit; `None` (and
    /// the path failed) if it cannot be read. What the index has beneath
    /// `path` under other names is gone: tombstoned.
    fn list(&mut self, fd: OwnedFd, path: RelPath) -> Result<Option<Level>> {
        match list_dir(&fd) {
            Ok(names) => {
                let mut deep = self.tombstone_unlisted(&path, &names)?;
                deep.sort();
                Ok(Some(Level {
                    fd,
                    path,
                    names: names.into_iter(),
                    deep,
                }))
            }
            Err(e) => {
                self.fail(&path, Error::io(format!("read directory {path}"), e));
                Ok(None)
            }
        }
    }

    /// Observes `parent/name` (at `path`) and queues its index update.
    fn visit(&mut self, parent: BorrowedFd<'_>, path: &RelPath, name: &[u8]) -> Result<Visit> {
        if self.protected.contains(path.as_bytes()) {
            return Ok(Visit::Kept);
        }
        if path.as_bytes().len() > MAX_PATH_LEN {
            let e = io::Error::other("path longer than PATH_MAX; not synced");
            self.fail(path, Error::io(format!("scan {path}"), e));
            return Ok(Visit::Kept);
        }
        self.stats.scanned += 1;
        let old = self.snap.get(path)?;
        #[cfg(test)]
        let observed = if self.dirty.contains(path) {
            Obs::Dirty("made dirty by the test")
        } else {
            self.observe(parent, path, name, old.as_ref())?
        };
        #[cfg(not(test))]
        let observed = self.observe(parent, path, name, old.as_ref())?;
        match observed {
            Obs::Absent => Ok(Visit::Gone),
            Obs::Dirty(reason) => {
                tracing::debug!(%path, reason, "unstable during scan; left for a rescan");
                self.stats.dirty.push(path.clone());
                self.protected.insert(path.as_bytes().to_vec());
                Ok(Visit::Kept)
            }
            Obs::Failed(e) => {
                self.fail(path, e);
                Ok(Visit::Kept)
            }
            Obs::Found(mut found) => {
                let dir = found.dir.take();
                #[cfg(any(test, feature = "hooks"))]
                if let Some(o) = &mut self.oracle {
                    o.seen.insert(path.as_bytes().to_vec());
                }
                // An ancestor shared by several scoped paths is recorded once.
                if !self.noted.contains(path.as_bytes()) {
                    self.record(path, old, found);
                }
                Ok(dir.map_or(Visit::Leaf, Visit::Dir))
            }
        }
    }

    /// `path` was a directory on the way to another scoped path a moment
    /// ago, and is not now: it changed during the scan, and is recorded as
    /// it was then. Left for a rescan, with its subtree.
    fn replaced(&mut self, path: &RelPath) {
        tracing::debug!(%path, "replaced during scan; left for a rescan");
        self.stats.dirty.push(path.clone());
        self.protected.insert(path.as_bytes().to_vec());
    }

    fn fail(&mut self, path: &RelPath, e: Error) {
        tracing::warn!(%path, error = %e, "cannot scan; index entry left unchanged");
        self.stats.errors.push(path.clone());
        self.protected.insert(path.as_bytes().to_vec());
    }

    fn observe(
        &mut self,
        parent: BorrowedFd<'_>,
        path: &RelPath,
        name: &[u8],
        old: Option<&Entry>,
    ) -> Result<Obs> {
        let fp = match Fingerprint::at(parent, name) {
            Ok(fp) => fp,
            Err(e) => return per_path(e),
        };
        hooks::point("scan.stat");
        match fp.kind {
            FileKind::File => self.observe_file(parent, name, &fp, old),
            FileKind::Dir => self.observe_dir(parent, path, name, &fp),
            FileKind::Symlink => self.observe_link(parent, path, name, &fp, old),
            FileKind::Special => Ok(Obs::Found(Found {
                kind: Kind::Unmanaged(UnmanagedReason::Special),
                mode: sync_mode(fp.mode),
                mtime_ns: fp.mtime_ns,
                local: local_of(&fp),
                dir: None,
            })),
        }
    }

    fn observe_file(
        &mut self,
        parent: BorrowedFd<'_>,
        name: &[u8],
        fp: &Fingerprint,
        old: Option<&Entry>,
    ) -> Result<Obs> {
        if let Some(old) = old.filter(|old| unchanged_file(old, fp, None)) {
            return Ok(Obs::Found(found_as(old)));
        }
        match self.hasher.hash_at(parent, name) {
            Ok((f2, hash)) => Ok(Obs::Found(self.file_found(&f2, hash, None))),
            Err(e) => per_path(e),
        }
    }

    fn file_found(&self, fp: &Fingerprint, hash: [u8; 32], via_link: Option<LinkInfo>) -> Found {
        Found {
            kind: Kind::File {
                size: fp.size,
                hash,
            },
            mode: sync_mode(fp.mode),
            mtime_ns: fp.mtime_ns,
            local: LocalMeta {
                via_link,
                racy: fp.ctime_ns >= self.racy_cutoff,
                ..local_of(fp)
            },
            dir: None,
        }
    }

    /// A real directory: opened beneath its parent, so a symlink swapped in
    /// since the `statx` is refused.
    fn observe_dir(
        &mut self,
        parent: BorrowedFd<'_>,
        path: &RelPath,
        name: &[u8],
        fp: &Fingerprint,
    ) -> Result<Obs> {
        let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC;
        let fd = match open_beneath(parent, name, flags) {
            Ok(fd) => fd,
            // Opening the name itself can only cross a mount point.
            Err(Errno::XDEV) => {
                return Ok(Obs::Failed(Error::io(
                    format!("open directory {path}"),
                    io::Error::other("mount point inside the replica; not synced"),
                )));
            }
            Err(Errno::NOTDIR) => return Ok(Obs::Dirty("directory replaced")),
            Err(e) => return Ok(errno_obs(e, path, "open directory")),
        };
        self.dir_found(fd, fp, None)
    }

    /// The directory open at `fd`, which must be the object `expect`
    /// describes. `via` is the followed link it was reached through, with
    /// the link's own local metadata (used if it turns out to be a loop).
    fn dir_found(
        &self,
        fd: OwnedFd,
        expect: &Fingerprint,
        via: Option<(LinkInfo, LocalMeta)>,
    ) -> Result<Obs> {
        let dfp = match Fingerprint::of_fd(fd.as_fd()) {
            Ok(dfp) => dfp,
            Err(e) => return per_path(e),
        };
        if !dfp.same_file(expect) || dfp.kind != FileKind::Dir {
            return Ok(Obs::Dirty("directory replaced"));
        }
        let id = (dfp.dev, dfp.ino);
        let (via_link, link_local) = via.unzip();
        if self.stack.contains(&id) {
            return Ok(Obs::Found(Found {
                kind: Kind::Unmanaged(UnmanagedReason::Loop),
                mode: sync_mode(dfp.mode),
                mtime_ns: dfp.mtime_ns,
                local: link_local.unwrap_or_else(|| local_of(&dfp)),
                dir: None,
            }));
        }
        Ok(Obs::Found(Found {
            kind: Kind::Dir,
            mode: sync_mode(dfp.mode),
            mtime_ns: dfp.mtime_ns,
            dir: Some(OpenDir {
                fd,
                id,
                followed: via_link.is_some(),
            }),
            local: LocalMeta {
                via_link,
                ..local_of(&dfp)
            },
        }))
    }

    fn observe_link(
        &mut self,
        parent: BorrowedFd<'_>,
        path: &RelPath,
        name: &[u8],
        fp: &Fingerprint,
        old: Option<&Entry>,
    ) -> Result<Obs> {
        let raw = match rustix::fs::readlinkat(parent, name, Vec::new()) {
            Ok(t) => t.into_bytes(),
            Err(Errno::INVAL) => return Ok(Obs::Dirty("symlink replaced")),
            Err(e) => return Ok(errno_obs(e, path, "read symlink")),
        };
        // Symlink targets are immutable: the same inode has the same target.
        match link_unchanged(parent, name, fp)? {
            None => {}
            Some(why) => return Ok(Obs::Dirty(why)),
        }
        // Written munged by the replica, since it would lead out of the root
        // (§4.6): the symlink the index has, whatever the policy. Recognised
        // by its target, so a copy (a materialized directory) is too.
        if !self.munge_links
            && is_munged(&raw)
            && let Some(Kind::Symlink { target }) = old.map(|o| &o.kind)
            && unmunge(&raw) == *target
        {
            return Ok(Obs::Found(Found {
                kind: Kind::Symlink {
                    target: target.clone(),
                },
                mode: sync_mode(fp.mode),
                mtime_ns: fp.mtime_ns,
                local: LocalMeta {
                    raw_target: Some(raw),
                    ..local_of(fp)
                },
                dir: None,
            }));
        }
        let target = if self.munge_links {
            if !is_munged(&raw) && old.is_none_or(|o| o.local.ino != fp.ino) {
                // Not made by us: synced with its target as it is (§4.4).
                tracing::warn!(%path, "symlink in a munged replica is not munged; synced as it is");
            }
            unmunge(&raw)
        } else {
            raw.clone()
        };
        let link_local = LocalMeta {
            raw_target: (target != raw).then(|| raw.clone()),
            ..local_of(fp)
        };

        // -K (§4.3): a link adopted earlier stays adopted while it resolves
        // to the pinned directory; `adopt` asks for a new adoption.
        let pinned = old
            .filter(|o| o.kind == Kind::Dir && o.local.via_link.as_ref().is_some_and(|v| v.adopted))
            .map(|o| (o.local.dev, o.local.ino));
        let may_adopt = self.keep_dirlinks && (pinned.is_some() || self.adopt.contains(path));

        let mut referent = None;
        let mut looped = false;
        let referent_kind = if may_adopt || needs_referent(self.policy, path, &target) {
            match self
                .root
                .open_referent(parent, path, name, OFlags::PATH | OFlags::CLOEXEC)
            {
                Ok((fd, out_of_tree)) => match Fingerprint::of_fd(fd.as_fd()) {
                    Ok(rfp) => {
                        referent = Some((fd, rfp, out_of_tree));
                        Some(rfp.kind)
                    }
                    Err(e) => return per_path(e),
                },
                Err(Errno::NOENT | Errno::NOTDIR) => None,
                // Too many levels of symlinks: a chain that loops.
                Err(Errno::LOOP) => {
                    looped = true;
                    None
                }
                Err(e) => return Ok(errno_obs(e, path, "follow symlink")),
            }
        } else {
            None
        };

        let link_found = |kind| {
            Obs::Found(Found {
                kind,
                mode: sync_mode(fp.mode),
                mtime_ns: fp.mtime_ns,
                local: link_local.clone(),
                dir: None,
            })
        };
        let via = |out_of_tree, adopted| LinkInfo {
            ino: fp.ino,
            ctime_ns: fp.ctime_ns,
            raw_target: raw.clone(),
            out_of_tree,
            adopted,
        };
        let treatment = classify(self.policy, path, &target, referent_kind);
        if may_adopt
            && matches!(
                treatment,
                Treatment::AsSymlink | Treatment::Unmanaged(UnmanagedReason::IgnoredLink)
            )
            && let Some((rfd, rfp, out_of_tree)) = &referent
            && rfp.kind == FileKind::Dir
            && (!out_of_tree || self.keep_dirlinks_unsafe)
            && pinned.is_none_or(|id| id == (rfp.dev, rfp.ino))
        {
            let via = via(*out_of_tree, true);
            return self.follow_dir(parent, path, name, fp, rfd, rfp, via, link_local.clone());
        }
        if pinned.is_some() {
            tracing::info!(%path, "symlink no longer resolves to its adopted directory; no longer adopted");
        }
        match treatment {
            Treatment::AsSymlink => Ok(link_found(Kind::Symlink { target })),
            Treatment::Unmanaged(UnmanagedReason::Dangling) if looped => {
                Ok(link_found(Kind::Unmanaged(UnmanagedReason::Loop)))
            }
            Treatment::Unmanaged(reason) => Ok(link_found(Kind::Unmanaged(reason))),
            Treatment::Follow => {
                let Some((rfd, rfp, out_of_tree)) = referent else {
                    return Ok(Obs::Dirty("symlink referent vanished"));
                };
                let via = via(out_of_tree, false);
                match rfp.kind {
                    FileKind::File => {
                        self.follow_file(parent, path, name, fp, &rfd, &rfp, via, old)
                    }
                    FileKind::Dir => {
                        self.follow_dir(parent, path, name, fp, &rfd, &rfp, via, link_local)
                    }
                    // `classify` follows only files and directories.
                    _ => Ok(Obs::Dirty("symlink referent changed type")),
                }
            }
        }
    }

    /// Opens the directory a followed (or adopted) link points to, as the
    /// `O_PATH` fd `rfd` classified it. The link must be unchanged afterwards.
    #[allow(clippy::too_many_arguments)]
    fn follow_dir(
        &self,
        parent: BorrowedFd<'_>,
        path: &RelPath,
        name: &[u8],
        link_fp: &Fingerprint,
        rfd: &OwnedFd,
        rfp: &Fingerprint,
        via: LinkInfo,
        link_local: LocalMeta,
    ) -> Result<Obs> {
        let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC;
        let fd = match open_beneath(rfd.as_fd(), b".", flags) {
            Ok(fd) => fd,
            Err(e) => return Ok(errno_obs(e, path, "open symlink referent")),
        };
        if let Some(why) = link_unchanged(parent, name, link_fp)? {
            return Ok(Obs::Dirty(why));
        }
        self.dir_found(fd, rfp, Some((via, link_local)))
    }

    /// Hashes the file a followed link points to, the one the `O_PATH` fd
    /// `rfd` classified (reopened through it, never looked up again). The
    /// link must still be the same inode after the read.
    #[allow(clippy::too_many_arguments)]
    fn follow_file(
        &mut self,
        parent: BorrowedFd<'_>,
        path: &RelPath,
        name: &[u8],
        link_fp: &Fingerprint,
        rfd: &OwnedFd,
        rfp: &Fingerprint,
        via: LinkInfo,
        old: Option<&Entry>,
    ) -> Result<Obs> {
        if let Some(old) = old.filter(|old| unchanged_file(old, rfp, Some(&via))) {
            return Ok(Obs::Found(found_as(old)));
        }
        let res = self.hasher.hash_with(
            path.as_bytes(),
            || reopen_file(rfd.as_fd()),
            |f2| {
                if !f2.same_file(rfp) {
                    return Ok(Some("symlink referent replaced"));
                }
                link_unchanged(parent, name, link_fp)
            },
        );
        match res {
            Ok((f2, hash)) => Ok(Obs::Found(self.file_found(&f2, hash, Some(via)))),
            Err(e) => per_path(e),
        }
    }

    /// Queues the index update that turns `old` into what was found.
    fn record(&mut self, path: &RelPath, old: Option<Entry>, f: Found) {
        let change = match &old {
            Some(old) if same_logical(old, &f) => {
                if old.local == f.local {
                    return;
                }
                Change::Local(f.local)
            }
            _ => Change::New {
                kind: f.kind,
                mode: f.mode,
                mtime_ns: f.mtime_ns,
                local: f.local,
            },
        };
        self.pending.push(Update {
            path: path.clone(),
            base: old.map(|e| e.seq),
            change,
        });
    }

    /// Queues tombstones for what the index has beneath `path` (and at
    /// `path` itself, with `itself`): `path` is gone, or is not a directory.
    fn tombstone_beneath(&mut self, path: &RelPath, itself: bool) -> Result<()> {
        let key = path.as_bytes();
        let Walk {
            snap,
            protected,
            pending,
            #[cfg(any(test, feature = "hooks"))]
            oracle,
            ..
        } = self;
        snap.for_each_prefix(
            path,
            |k| itself || k != key,
            |p, e| {
                queue_tombstone(
                    pending,
                    protected,
                    #[cfg(any(test, feature = "hooks"))]
                    oracle,
                    p,
                    &e,
                );
                Ok(())
            },
        )
    }

    /// Queues tombstones for what the index has beneath the directory
    /// `dir` under a name its listing, `names` (sorted), lacks: such a child
    /// is gone, with everything beneath it. Returns the listed names the
    /// index has entries beneath.
    fn tombstone_unlisted(&mut self, dir: &RelPath, names: &[Vec<u8>]) -> Result<Vec<Vec<u8>>> {
        let Walk {
            snap,
            protected,
            pending,
            #[cfg(any(test, feature = "hooks"))]
            oracle,
            ..
        } = self;
        snap.for_each_unlisted(
            dir,
            |name| names.binary_search_by(|n| n.as_slice().cmp(name)).is_ok(),
            |p, e| {
                queue_tombstone(
                    pending,
                    protected,
                    #[cfg(any(test, feature = "hooks"))]
                    oracle,
                    p,
                    &e,
                );
                Ok(())
            },
        )
    }

    /// The oracle (tests and the `hooks` feature): panics unless the walk
    /// queued exactly the tombstones the pass before issue #15 queued: every
    /// indexed path in a scanned region that was not found, and is neither a
    /// tombstone nor at or beneath a protected path.
    #[cfg(any(test, feature = "hooks"))]
    fn check_tombstones(&mut self) -> Result<()> {
        let Some(oracle) = self.oracle.take() else {
            return Ok(());
        };
        let mut old = std::collections::BTreeSet::new();
        for region in &self.regions {
            self.snap.for_each_prefix(
                region,
                |key| !oracle.seen.contains(key),
                |path, e| {
                    if !e.is_tombstone() && !prefixes(&path).any(|pre| self.protected.contains(pre))
                    {
                        old.insert(path);
                    }
                    Ok(())
                },
            )?;
        }
        if old != oracle.tombstoned {
            let new = &oracle.tombstoned;
            panic!(
                "the scan's tombstones differ from the old pass's\nonly new: {:?}\nonly old: {:?}",
                new.difference(&old).take(5).collect::<Vec<_>>(),
                old.difference(new).take(5).collect::<Vec<_>>(),
            );
        }
        Ok(())
    }

    /// Commits the pending updates in one transaction.
    fn flush(&mut self) -> Result<()> {
        if self.pending.is_empty() {
            return Ok(());
        }
        let mut txn = self.index.write()?;
        for u in std::mem::take(&mut self.pending) {
            let cur = txn.get(&u.path)?;
            if cur.as_ref().map(|e| e.seq) != u.base {
                tracing::debug!(path = %u.path, "index entry changed during the scan");
                self.stats.dirty.push(u.path);
                continue;
            }
            match u.change {
                Change::New {
                    kind,
                    mode,
                    mtime_ns,
                    local,
                } => {
                    let mut vv = cur.map(|e| e.vv).unwrap_or_default();
                    if !vv.can_bump(txn.max_counter()) {
                        // A counter past the limit could not be read back
                        // (issue #14): only a peer at the limit gets us here.
                        #[cfg(any(test, feature = "hooks"))]
                        {
                            self.oracle = None;
                        }
                        self.fail(
                            &u.path,
                            Error::InvalidOp {
                                path: u.path.as_bytes().to_vec(),
                                reason: "version counter at its limit; change not recorded",
                            },
                        );
                        continue;
                    }
                    vv.bump_after(self.replica, txn.max_counter());
                    let tombstone = kind == Kind::Tombstone;
                    let mut entry = Entry {
                        kind,
                        mode,
                        mtime_ns,
                        vv,
                        seq: 0,
                        local,
                    };
                    txn.put(&u.path, &mut entry)?;
                    self.stats.changed += 1;
                    self.stats.tombstoned += u64::from(tombstone);
                }
                Change::Local(local) => {
                    let mut entry = cur.expect("base seq matched an entry");
                    entry.local = local;
                    txn.put_local(&u.path, &entry)?;
                }
            }
        }
        txn.commit()
    }
}

/// Queues a tombstone for `path`, whose entry is `e`, unless it is one
/// already or is protected (at or beneath a dirty or failed path).
fn queue_tombstone(
    pending: &mut Vec<Update>,
    protected: &HashSet<Vec<u8>>,
    #[cfg(any(test, feature = "hooks"))] oracle: &mut Option<Oracle>,
    path: RelPath,
    e: &Entry,
) {
    if e.is_tombstone() || prefixes(&path).any(|pre| protected.contains(pre)) {
        return;
    }
    #[cfg(any(test, feature = "hooks"))]
    if let Some(o) = oracle {
        o.tombstoned.insert(path.clone());
    }
    pending.push(Update {
        path,
        base: Some(e.seq),
        change: Change::New {
            kind: Kind::Tombstone,
            mode: 0,
            mtime_ns: 0,
            local: LocalMeta::default(),
        },
    });
}

/// Names in the directory `fd`, sorted, without `.`, `..` and reserved names.
fn list_dir(fd: &OwnedFd) -> io::Result<Vec<Vec<u8>>> {
    let mut names = Vec::new();
    for entry in Dir::new(fd.try_clone()?)? {
        let name = entry?.file_name().to_bytes().to_vec();
        if name != b"." && name != b".." && !is_reserved(&name) {
            names.push(name);
        }
    }
    names.sort();
    Ok(names)
}

/// `None` if `parent/name` is still the symlink `fp`, else why not.
fn link_unchanged(
    parent: BorrowedFd<'_>,
    name: &[u8],
    fp: &Fingerprint,
) -> Result<Option<&'static str>> {
    match Fingerprint::at(parent, name) {
        Ok(now) if now.unchanged(fp) => Ok(None),
        Ok(_) => Ok(Some("symlink replaced")),
        Err(Error::Io { source, .. }) if source.kind() == io::ErrorKind::NotFound => {
            Ok(Some("symlink removed"))
        }
        Err(e) => Err(e),
    }
}

/// The rehash shortcut (design §3): the indexed file has the same inode,
/// size, mtime and ctime (and was reached the same way) and is not racy.
fn unchanged_file(old: &Entry, fp: &Fingerprint, via: Option<&LinkInfo>) -> bool {
    matches!(old.kind, Kind::File { size, .. } if size == fp.size)
        && !old.local.racy
        && old.local.dev == fp.dev
        && old.local.ino == fp.ino
        && old.local.ctime_ns == fp.ctime_ns
        && old.mtime_ns == fp.mtime_ns
        && old.local.via_link.as_ref() == via
}

/// Whether `f` is the same as `old` for the peer. Directory and symlink
/// mtimes are not synced state: a directory's mtime moves with every child,
/// and a symlink's is set when it is created.
fn same_logical(old: &Entry, f: &Found) -> bool {
    old.kind == f.kind
        && match f.kind {
            Kind::File { .. } => old.mode == f.mode && old.mtime_ns == f.mtime_ns,
            Kind::Dir => old.mode == f.mode,
            Kind::Symlink { .. } | Kind::Unmanaged(_) | Kind::Tombstone => true,
        }
}

/// The indexed entry, unchanged.
fn found_as(old: &Entry) -> Found {
    Found {
        kind: old.kind.clone(),
        mode: old.mode,
        mtime_ns: old.mtime_ns,
        local: old.local.clone(),
        dir: None,
    }
}

fn local_of(fp: &Fingerprint) -> LocalMeta {
    LocalMeta {
        dev: fp.dev,
        ino: fp.ino,
        ctime_ns: fp.ctime_ns,
        ..LocalMeta::default()
    }
}

/// Sorts a per-path library error: unstable → dirty, gone → absent, other
/// I/O → failed. Index and other errors abort the scan.
fn per_path(e: Error) -> Result<Obs> {
    match e {
        Error::Unstable { reason, .. } => Ok(Obs::Dirty(reason)),
        Error::Io { ref source, .. } if source.kind() == io::ErrorKind::NotFound => Ok(Obs::Absent),
        Error::Io { .. } | Error::InvalidPath { .. } => Ok(Obs::Failed(e)),
        e => Err(e),
    }
}

/// Sorts a per-path errno like [`per_path`].
fn errno_obs(e: Errno, path: &RelPath, what: &str) -> Obs {
    if e == Errno::NOENT {
        return Obs::Absent;
    }
    match unstable_reason(e) {
        Some(reason) => Obs::Dirty(reason),
        None => Obs::Failed(Error::io(format!("{what} {path}"), e.into())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use std::cell::Cell;
    use std::collections::BTreeMap;
    use std::fs;
    use std::io::Write;
    use std::rc::Rc;

    const ME: ReplicaId = ReplicaId(0xfeed);

    struct Fixture {
        dir: tempfile::TempDir,
        _state: tempfile::TempDir,
        root: Root,
        index: Rc<IndexStore>,
    }

    fn fixture() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        let root = Root::open(dir.path()).unwrap();
        let index = Rc::new(IndexStore::open(&state.path().join("i.redb"), ME).unwrap());
        Fixture {
            dir,
            _state: state,
            root,
            index,
        }
    }

    impl Fixture {
        fn scan(&self) -> ScanStats {
            Scanner::new(&self.root, &self.index, SymlinkPolicy::Links)
                .scan(&Scope::Full)
                .unwrap()
        }

        fn get(&self, p: &str) -> Entry {
            self.index.get(&RelPath::new(p).unwrap()).unwrap().unwrap()
        }
    }

    fn rp(s: &str) -> RelPath {
        RelPath::new(s).unwrap()
    }

    #[test]
    fn prefixes_and_outermost() {
        let p = rp("a/bb/c");
        let pre: Vec<&[u8]> = prefixes(&p).collect();
        assert_eq!(pre, [&b""[..], b"a", b"a/bb", b"a/bb/c"]);
        assert_eq!(prefixes(&RelPath::root()).collect::<Vec<_>>(), [&b""[..]]);
        let paths = ["a/b", "a.b", "a", "c/d", "c/d", "a/b/c"].map(rp);
        assert_eq!(outermost(&paths), ["a", "a.b", "c/d"].map(rp));
        assert_eq!(outermost(&[rp("x"), RelPath::root()]), [RelPath::root()]);
    }

    /// A file that keeps changing while it is read stays as indexed, and is
    /// reported dirty.
    #[test]
    fn unstable_read_is_dirty_and_unchanged() {
        let f = fixture();
        let path = f.dir.path().join("f");
        fs::write(&path, b"version one").unwrap();
        f.scan();
        let before = f.get("f");
        fs::write(&path, b"version two").unwrap();

        let appends = Rc::new(Cell::new(0));
        let n = appends.clone();
        let _g = hooks::on("scan.read_chunk", move || {
            let mut file = fs::OpenOptions::new().append(true).open(&path).unwrap();
            file.write_all(b"+").unwrap();
            n.set(n.get() + 1);
        });
        let stats = f.scan();
        assert_eq!(stats.dirty, [rp("f")]);
        assert_eq!((stats.changed, stats.hashed), (0, 1));
        assert_eq!(appends.get(), 1 + crate::fs::stat::STABLE_READ_RETRIES);
        assert_eq!(f.get("f"), before);
    }

    /// A directory swapped for a symlink between its `statx` and its open is
    /// dirty; its entry and subtree are kept, and nothing outside is read.
    #[test]
    fn directory_swapped_for_symlink_is_dirty() {
        let f = fixture();
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("secret"), b"s").unwrap();
        fs::create_dir(f.dir.path().join("d")).unwrap();
        fs::write(f.dir.path().join("d/child"), b"c").unwrap();
        f.scan();
        let (d, child) = (f.get("d"), f.get("d/child"));

        let root = f.dir.path().to_owned();
        let out = outside.path().to_owned();
        let _g = hooks::once("scan.stat", move || {
            fs::rename(root.join("d"), root.join("moved")).unwrap();
            std::os::unix::fs::symlink(&out, root.join("d")).unwrap();
        });
        let stats = f.scan();
        assert_eq!(stats.dirty, [rp("d")], "{stats:?}");
        assert_eq!((f.get("d"), f.get("d/child")), (d, child.clone()));
        assert!(f.index.get(&rp("d/secret")).unwrap().is_none());

        // The next scan sees the symlink, and the directory under its new
        // name (listed after the rename).
        let stats = f.scan();
        assert!(stats.dirty.is_empty());
        assert!(matches!(f.get("d").kind, Kind::Symlink { .. }));
        assert_eq!(f.get("d/child").kind, Kind::Tombstone);
        assert_eq!(f.get("moved/child").kind, child.kind);
    }

    /// An index entry written by someone else while the scan runs is not
    /// overwritten; the path is reported dirty.
    #[test]
    fn index_changed_during_scan_is_dirty() {
        let f = fixture();
        fs::write(f.dir.path().join("f"), b"one").unwrap();
        f.scan();
        fs::write(f.dir.path().join("f"), b"two").unwrap();

        let index = f.index.clone();
        let mut theirs = f.get("f");
        theirs.vv.set(ReplicaId(7), 99);
        let written = Rc::new(Cell::new(0));
        let w = written.clone();
        let mut t = theirs.clone();
        let _g = hooks::once("scan.read_chunk", move || {
            w.set(index.put(&RelPath::new("f").unwrap(), &mut t).unwrap());
        });
        let stats = f.scan();
        assert_eq!(stats.dirty, [rp("f")]);
        assert_eq!(stats.changed, 0);
        let now = f.get("f");
        assert_eq!((now.vv, now.seq), (theirs.vv, written.get()));
    }

    /// A followed link whose target climbs with `..` (`d/e/l -> ../x`, the
    /// crash suite's `materialize` scenario) is not reported dirty when a
    /// rename elsewhere on the system races the referent's lookup (`openat2`'s
    /// `EAGAIN`), neither while classifying the link nor while reading the
    /// referent (T23).
    #[test]
    fn followed_dotdot_link_survives_unrelated_renames() {
        let f = fixture();
        fs::create_dir_all(f.dir.path().join("d/e")).unwrap();
        fs::write(f.dir.path().join("d/x"), b"x").unwrap();
        std::os::unix::fs::symlink("../x", f.dir.path().join("d/e/l")).unwrap();
        let mut races = 0;
        let _g = hooks::on_fault("root.in_tree_lookup", move || {
            // Every first try of a lookup is raced.
            races += 1;
            (races % 2 == 1).then_some(Errno::AGAIN)
        });
        for _ in 0..2 {
            let stats = Scanner::new(&f.root, &f.index, SymlinkPolicy::CopyLinks)
                .scan(&Scope::Full)
                .unwrap();
            assert!(stats.dirty.is_empty(), "{stats:?}");
        }
        let l = f.get("d/e/l");
        assert_eq!(l.kind, f.get("d/x").kind);
        assert!(l.local.via_link.is_some());
    }

    /// Issue #14: a followed file is read through the fd that classified
    /// it, never looked up again. A link on the chain swapped meanwhile (to
    /// a FIFO here; as root, a device node) is never opened.
    #[test]
    fn a_followed_file_is_not_looked_up_again() {
        let f = fixture();
        let d = f.dir.path().to_owned();
        fs::write(d.join("x"), b"x").unwrap();
        rustix::fs::mknodat(
            rustix::fs::CWD,
            d.join("p").as_path(),
            rustix::fs::FileType::Fifo,
            rustix::fs::Mode::from_bits_truncate(0o600),
            0,
        )
        .unwrap();
        std::os::unix::fs::symlink("x", d.join("m")).unwrap();
        std::os::unix::fs::symlink("m", d.join("l")).unwrap();
        let mut lookups = 0;
        let _g = hooks::on_fault("root.in_tree_lookup", move || {
            lookups += 1;
            // `l` is classified first. A second lookup (to read it) would
            // open the FIFO, and find it is not the classified file.
            if lookups == 2 {
                fs::remove_file(d.join("m")).unwrap();
                std::os::unix::fs::symlink("p", d.join("m")).unwrap();
            }
            None
        });
        let stats = Scanner::new(&f.root, &f.index, SymlinkPolicy::CopyLinks)
            .scan(&Scope::Full)
            .unwrap();
        assert!(
            stats.dirty.is_empty() && stats.errors.is_empty(),
            "{stats:?}"
        );
        assert_eq!(f.get("l").kind, f.get("x").kind);
    }

    /// Issue #14: a deep tree is walked without recursion, so in a small
    /// stack, and a path longer than `PATH_MAX` is an error, not indexed.
    #[test]
    fn a_deep_tree_fits_a_small_stack_and_paths_stop_at_path_max() {
        use rustix::fs::Mode;
        use rustix::process::{Resource, getrlimit, setrlimit};
        // One open fd per level.
        let mut limit = getrlimit(Resource::Nofile);
        limit.current = limit.maximum;
        let _ = setrlimit(Resource::Nofile, limit);
        let f = fixture();
        let depth = MAX_PATH_LEN / 2 + 10;
        let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC;
        let mut fd = rustix::fs::open(f.dir.path(), flags, Mode::empty()).unwrap();
        for _ in 0..depth {
            rustix::fs::mkdirat(&fd, "d", Mode::from_bits_truncate(0o755)).unwrap();
            fd = rustix::fs::openat(&fd, "d", flags, Mode::empty()).unwrap();
        }
        drop(fd);
        let deepest = |n: usize| rp(&vec!["d"; n].join("/"));
        let (root, index): (&Root, &IndexStore) = (&f.root, &f.index);
        let stats = std::thread::scope(|s| {
            std::thread::Builder::new()
                .stack_size(256 << 10)
                .spawn_scoped(s, || {
                    Scanner::new(root, index, SymlinkPolicy::Links)
                        .scan(&Scope::Full)
                        .unwrap()
                })
                .unwrap()
                .join()
                .unwrap()
        });
        let longest = MAX_PATH_LEN.div_ceil(2);
        assert_eq!(deepest(longest).as_bytes().len(), MAX_PATH_LEN);
        assert_eq!(stats.errors, [deepest(longest + 1)]);
        assert_eq!(stats.changed, longest as u64);
        let entry = f.index.get(&deepest(longest)).unwrap().unwrap();
        assert_eq!(entry.kind, Kind::Dir);
    }

    /// Issue #12: a followed link whose path is too long to look up beneath
    /// the root (`ENAMETOOLONG`, beneath followed directories) cannot be
    /// scanned. That is an error, not a dirty path, which a daemon retried
    /// every second.
    #[test]
    fn followed_link_with_a_too_long_path_is_an_error() {
        let f = fixture();
        fs::write(f.dir.path().join("x"), b"x").unwrap();
        std::os::unix::fs::symlink("x", f.dir.path().join("l")).unwrap();
        let _g = hooks::on_fault("root.in_tree_lookup", || Some(Errno::NAMETOOLONG));
        let stats = Scanner::new(&f.root, &f.index, SymlinkPolicy::CopyLinks)
            .scan(&Scope::Full)
            .unwrap();
        assert!(stats.dirty.is_empty(), "{stats:?}");
        assert_eq!(stats.errors, [rp("l")]);
    }

    /// Names that sort among a directory's children in the index: `-`,
    /// `.`, `!`, space and `\x01` come before `/`, `0` right after it, and
    /// `\xff` (not UTF-8) last.
    const NAMES: [&[u8]; 9] = [
        b"a", b"a-b", b"a.b", b"a!", b"a \x01", b"a0", b"a\xff", b"b", b"\xff",
    ];

    fn arb_rel() -> impl Strategy<Value = Vec<u8>> {
        prop::collection::vec(prop::sample::select(NAMES.to_vec()), 1..4)
            .prop_map(|c| c.join(&b'/'))
    }

    #[derive(Clone, Debug)]
    enum Edit {
        Write(Vec<u8>, u8),
        Mkdir(Vec<u8>),
        Symlink(Vec<u8>),
        Remove(Vec<u8>),
    }

    fn arb_edit() -> impl Strategy<Value = Edit> {
        prop_oneof![
            3 => (arb_rel(), any::<u8>()).prop_map(|(p, c)| Edit::Write(p, c)),
            2 => arb_rel().prop_map(Edit::Mkdir),
            1 => arb_rel().prop_map(Edit::Symlink),
            3 => arb_rel().prop_map(Edit::Remove),
        ]
    }

    /// Applies `edit` beneath `root` as a user would; an edit that does not
    /// fit the tree (a parent is a file) does nothing.
    fn edit(root: &std::path::Path, edit: &Edit) {
        use std::os::unix::ffi::OsStrExt;
        let at = |p: &[u8]| root.join(std::ffi::OsStr::from_bytes(p));
        let parent = |p: &[u8]| match p.iter().rposition(|&c| c == b'/') {
            Some(i) => fs::create_dir_all(at(&p[..i])).is_ok(),
            None => true,
        };
        let clear = |p: &std::path::Path| match fs::symlink_metadata(p) {
            Ok(m) if m.is_dir() => drop(fs::remove_dir_all(p)),
            Ok(_) => drop(fs::remove_file(p)),
            Err(_) => {}
        };
        match edit {
            Edit::Write(p, c) if parent(p) => {
                clear(&at(p));
                fs::write(at(p), [*c]).unwrap();
            }
            Edit::Mkdir(p) if parent(p) => {
                if !at(p).is_dir() {
                    clear(&at(p));
                    fs::create_dir(at(p)).unwrap();
                }
            }
            Edit::Symlink(p) if parent(p) => {
                clear(&at(p));
                std::os::unix::fs::symlink("t", at(p)).unwrap();
            }
            Edit::Remove(p) => clear(&at(p)),
            _ => {}
        }
    }

    /// Every path beneath `root` (not into unreadable directories), as
    /// `lstat` sees it.
    fn disk(root: &std::path::Path) -> HashSet<RelPath> {
        use std::os::unix::ffi::OsStrExt;
        let mut out = HashSet::new();
        let mut todo = vec![RelPath::root()];
        while let Some(dir) = todo.pop() {
            let Ok(list) = fs::read_dir(root.join(std::ffi::OsStr::from_bytes(dir.as_bytes())))
            else {
                continue;
            };
            for e in list {
                let e = e.unwrap();
                let p = dir.join(e.file_name().as_bytes()).unwrap();
                if e.file_type().unwrap().is_dir() {
                    todo.push(p.clone());
                }
                out.insert(p);
            }
        }
        out
    }

    proptest::proptest! {
        #![proptest_config(proptest::prelude::ProptestConfig::with_cases(512))]

        /// Issue #15: the merge-walk tombstones exactly what the old pass
        /// over the set of found paths did (the oracle runs inside every
        /// scan and panics on a difference), across names that sort among
        /// a directory's children, stale and orphaned index entries,
        /// unreadable directories, dirty paths and scoped scans. A full
        /// scan also leaves the index matching the disk, but beneath the
        /// paths it could not read, which keep their entries.
        #[test]
        fn merge_walk_tombstones_what_the_old_pass_did(
            before in prop::collection::vec(arb_edit(), 0..16),
            stale in prop::collection::vec((arb_rel(), any::<bool>()), 0..4),
            after in prop::collection::vec(arb_edit(), 0..10),
            unreadable in prop::collection::vec(arb_rel(), 0..2),
            dirty in prop::collection::vec(arb_rel(), 0..2),
            scope in prop::option::of(prop::collection::vec(arb_rel(), 1..4)),
        ) {
            use std::os::unix::fs::PermissionsExt;
            use std::os::unix::ffi::OsStrExt;
            let f = fixture();
            let root = f.dir.path();
            for e in &before {
                edit(root, e);
            }
            f.scan();
            // Index rows the disk does not explain: a file or a tombstone
            // somewhere, maybe beneath a path the index lacks.
            for (p, tomb) in stale {
                let kind = if tomb {
                    Kind::Tombstone
                } else {
                    Kind::File { size: 1, hash: [9; 32] }
                };
                let mut e = Entry::new(kind, 0o644, 1, crate::index::VersionVector::new());
                f.index.put(&RelPath::new(p).unwrap(), &mut e).unwrap();
            }
            for e in &after {
                edit(root, e);
            }
            let locked: Vec<std::path::PathBuf> = unreadable
                .iter()
                .map(|p| root.join(std::ffi::OsStr::from_bytes(p)))
                .filter(|p| fs::symlink_metadata(p).is_ok_and(|m| m.is_dir()))
                .collect();
            for p in &locked {
                fs::set_permissions(p, fs::Permissions::from_mode(0o000)).unwrap();
            }
            let index_before: BTreeMap<RelPath, Entry> = f
                .index
                .read()
                .unwrap()
                .iter_prefix(&RelPath::root())
                .unwrap()
                .into_iter()
                .collect();

            let mut scanner = Scanner::new(&f.root, &f.index, SymlinkPolicy::Links);
            scanner.dirty = dirty.iter().map(|p| RelPath::new(p.clone()).unwrap()).collect();
            let scope = match &scope {
                None => Scope::Full,
                Some(paths) => Scope::Paths(paths.iter().map(|p| RelPath::new(p.clone()).unwrap()).collect()),
            };
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| scanner.scan(&scope)));
            for p in &locked {
                fs::set_permissions(p, fs::Permissions::from_mode(0o755)).unwrap();
            }
            let stats = match result {
                Ok(stats) => stats.unwrap(),
                Err(panic) => std::panic::resume_unwind(panic),
            };

            if scope == Scope::Full {
                // The paths it could not read keep their entries, and so
                // does everything beneath them.
                let kept: Vec<&RelPath> = stats.dirty.iter().chain(&stats.errors).collect();
                let beneath_kept = |p: &RelPath| kept.iter().any(|k| *k == p || crate::engine::is_beneath(p, k));
                let on_disk = disk(root);
                let index: BTreeMap<RelPath, Entry> = f
                    .index
                    .read()
                    .unwrap()
                    .iter_prefix(&RelPath::root())
                    .unwrap()
                    .into_iter()
                    .collect();
                for (p, e) in &index {
                    if beneath_kept(p) {
                        prop_assert_eq!(Some(e), index_before.get(p), "{} changed", p);
                    } else {
                        prop_assert_eq!(!e.is_tombstone(), on_disk.contains(p), "{}: {:?}", p, e);
                    }
                }
                for p in &on_disk {
                    prop_assert!(beneath_kept(p) || index.contains_key(p), "{} not indexed", p);
                }
            }
        }
    }

    #[test]
    fn long_lists_are_coalesced_or_cut_short() {
        // Short lists are left as they are.
        let few = vec![rp("b"), rp("a/x"), rp("a")];
        assert_eq!(coalesce(few.clone()), few);
        assert_eq!(Scope::Paths(few.clone()).bounded(), Scope::Paths(few));

        // Too many paths: the deepest are lifted to their parents until the
        // list fits; every path is still covered.
        let many: Vec<RelPath> = (0..3 * MAX_LISTED)
            .map(|i| rp(&format!("d{}/e{}/f{i}", i % 3, i % 300)))
            .chain([rp("top")])
            .collect();
        let out = coalesce(many.clone());
        assert_eq!(out.len(), 300 + 1);
        assert!(out.contains(&rp("d0/e0")) && out.contains(&rp("top")));
        assert!(out.is_sorted());
        let covered: HashSet<&[u8]> = out.iter().map(RelPath::as_bytes).collect();
        assert!(
            many.iter()
                .all(|p| prefixes(p).any(|pre| covered.contains(pre)))
        );

        // Too many bytes: cut deep enough to fit.
        let deep = "x/".repeat(2000);
        let long: Vec<RelPath> = (0..1100).map(|i| rp(&format!("d{i}/{deep}f"))).collect();
        let cut = coalesce(long.clone());
        assert_eq!(cut.len(), 1100);
        assert!(cut.iter().map(listed_bytes).sum::<usize>() <= MAX_LISTED_BYTES);
        assert!(cut.iter().all(|p| p.depth() > 1000 && p.depth() < 2000));
        let covered: HashSet<&[u8]> = cut.iter().map(RelPath::as_bytes).collect();
        assert!(
            long.iter()
                .all(|p| prefixes(p).any(|pre| covered.contains(pre)))
        );
        // Too many names at the top: the root alone.
        let wide = (0..2 * MAX_LISTED).map(|i| rp(&format!("t{i}"))).collect();
        assert_eq!(coalesce(wide), [RelPath::root()]);

        // Stats: dirty paths coalesced, errors cut short and counted.
        let mut stats = ScanStats {
            dirty: many,
            errors: (0..MAX_LISTED + 5)
                .map(|i| rp(&format!("e{i:06}")))
                .collect(),
            ..ScanStats::default()
        };
        assert_eq!(stats.bound(), 5);
        assert_eq!(stats.dirty, out);
        assert_eq!(stats.errors.len(), MAX_LISTED);
        assert_eq!(stats.errors[0], rp("e000000"));
        let encoded = postcard::to_stdvec(&stats).unwrap();
        assert!(encoded.len() < 2 * MAX_LISTED_BYTES, "{}", encoded.len());
    }
}
