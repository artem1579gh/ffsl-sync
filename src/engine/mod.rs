//! Reconciliation, planning, execution and conflict handling (design §6).
//!
//! The engine performs no I/O itself; it talks only to the `Replica` trait.
//! [`reconcile`] compares two index views and decides what to do for each
//! path; [`plan`] turns those decisions into ordered per-replica steps;
//! [`Engine`] runs them as sync cycles against two replicas.
pub mod conflict;
pub mod executor;
pub mod plan;
pub mod reconcile;
#[cfg(test)]
mod sim;

pub use conflict::ConflictCopy;
pub use executor::{
    DEFAULT_MAX_DELETE_PERCENT, Engine, HeldBack, MASS_DELETE_MIN, MAX_ROUNDS, SyncReport,
};
pub use plan::{Phase, PhaseKind, Step, plan};
pub use reconcile::{Action, ActionKind, Resolution, SkipReason, reconcile};

use std::collections::BTreeMap;
use std::ops::Bound;
use std::sync::Arc;

use crate::config::ReplicaId;
use crate::fs::RelPath;
use crate::index::Entry;

/// One of the two replicas of a pair.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Side {
    A,
    B,
}

impl Side {
    pub fn other(self) -> Side {
        match self {
            Side::A => Side::B,
            Side::B => Side::A,
        }
    }
}

/// A read-only view of one replica's index, as the engine sees it: entries
/// in the wire form (no [`LocalMeta`](crate::index::LocalMeta) needed).
pub trait IndexView {
    /// The replica the entries belong to.
    fn replica(&self) -> ReplicaId;

    fn get(&self, path: &RelPath) -> Option<&Entry>;

    /// Every entry, in path (byte) order.
    fn entries(&self) -> Box<dyn Iterator<Item = (&RelPath, &Entry)> + '_>;

    /// The entries strictly beneath `path` (all of them for the root), in
    /// path order.
    fn descendants(&self, path: &RelPath) -> Box<dyn Iterator<Item = (&RelPath, &Entry)> + '_>;
}

/// An in-memory [`IndexView`], e.g. built from
/// [`Replica::changes_since(0)`](crate::replica::Replica::changes_since),
/// or kept up to date by a [`SnapshotCache`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Snapshot {
    replica: ReplicaId,
    entries: BTreeMap<RelPath, Entry>,
}

impl Snapshot {
    /// A later entry for the same path replaces an earlier one.
    pub fn new(
        replica: ReplicaId,
        entries: impl IntoIterator<Item = (RelPath, Entry)>,
    ) -> Snapshot {
        Snapshot {
            replica,
            entries: entries.into_iter().collect(),
        }
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// A replica's index kept in memory across rounds and cycles (design §6.4,
/// issue #15), so that a round reads only what changed since the last one:
/// the entries put since the highest seq seen (a put always takes a fresh
/// seq), and the rows the index removed (tombstone GC, the only removal),
/// which the replica notes with [`SnapshotCache::removed`]. A replica owns
/// it and hands out the snapshot shared ([`SnapshotCache::get`]); a snapshot
/// still held by a caller when the cache changes is copied first, which
/// [`CacheStats::copies`] counts.
#[derive(Debug)]
pub struct SnapshotCache {
    snap: Arc<Snapshot>,
    /// The highest seq applied.
    seen: u64,
    /// Rows removed from the index since the last update.
    removed: Vec<RelPath>,
    stats: CacheStats,
}

/// What keeping a [`SnapshotCache`] up to date took (for tests and logs).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CacheStats {
    /// Index entries read into the cache in all.
    pub read: u64,
    /// Times the whole snapshot was copied, because a caller still held it
    /// when it changed.
    pub copies: u64,
}

impl SnapshotCache {
    /// An empty cache: the first update reads the whole index.
    pub fn new(replica: ReplicaId) -> SnapshotCache {
        SnapshotCache {
            snap: Arc::new(Snapshot::new(replica, [])),
            seen: 0,
            removed: Vec::new(),
            stats: CacheStats::default(),
        }
    }

    /// The highest seq read: the next update needs the entries put after it.
    pub fn seen(&self) -> u64 {
        self.seen
    }

    /// Notes rows the index removed; they leave the snapshot at the next
    /// update, so a snapshot held now is not copied for them.
    pub fn removed(&mut self, paths: impl IntoIterator<Item = RelPath>) {
        self.removed.extend(paths);
    }

    /// Takes out the rows noted as removed, then puts `changes`: entries put
    /// since [`SnapshotCache::seen`], in the wire form.
    pub fn update(&mut self, changes: Vec<(RelPath, Entry)>) {
        if changes.is_empty() && self.removed.is_empty() {
            return;
        }
        self.stats.read += changes.len() as u64;
        if Arc::strong_count(&self.snap) > 1 {
            self.stats.copies += 1;
            tracing::debug!(
                entries = self.snap.len(),
                "index snapshot still in use: copied"
            );
        }
        let snap = Arc::make_mut(&mut self.snap);
        for path in self.removed.drain(..) {
            snap.entries.remove(&path);
        }
        self.seen = changes.iter().fold(self.seen, |s, (_, e)| s.max(e.seq));
        if snap.entries.is_empty() {
            // Built in one go, the tree's nodes are full: a first read of a
            // big index takes less memory so.
            snap.entries = changes.into_iter().collect();
        } else {
            snap.entries.extend(changes);
        }
    }

    /// Forgets everything read (e.g. for another index behind the same
    /// replica); the stats stay.
    pub fn reset(&mut self) {
        let replica = self.snap.replica;
        *self = SnapshotCache {
            stats: self.stats,
            ..SnapshotCache::new(replica)
        };
    }

    /// The snapshot, shared.
    pub fn get(&self) -> Arc<Snapshot> {
        self.snap.clone()
    }

    pub fn stats(&self) -> CacheStats {
        self.stats
    }

    /// The consistency check (tests and the `hooks` feature): panics unless
    /// the snapshot holds exactly `fresh`, the whole index read anew.
    #[cfg(any(test, feature = "hooks"))]
    pub fn check(&self, fresh: Vec<(RelPath, Entry)>) {
        let fresh = Snapshot::new(self.snap.replica, fresh);
        if *self.snap == fresh {
            return;
        }
        let differ: Vec<_> = self
            .snap
            .entries
            .keys()
            .chain(fresh.entries.keys())
            .filter(|p| self.snap.get(p) != fresh.get(p))
            .take(5)
            .map(|p| {
                format!(
                    "{p}: cached {:?}, index {:?}",
                    self.snap.get(p),
                    fresh.get(p)
                )
            })
            .collect();
        panic!(
            "the cached index snapshot of replica {} differs from the index:\n{}",
            self.snap.replica,
            differ.join("\n")
        );
    }
}

impl IndexView for Snapshot {
    fn replica(&self) -> ReplicaId {
        self.replica
    }

    fn get(&self, path: &RelPath) -> Option<&Entry> {
        self.entries.get(path)
    }

    fn entries(&self) -> Box<dyn Iterator<Item = (&RelPath, &Entry)> + '_> {
        Box::new(self.entries.iter())
    }

    fn descendants(&self, path: &RelPath) -> Box<dyn Iterator<Item = (&RelPath, &Entry)> + '_> {
        if path.is_root() {
            return Box::new(self.entries.iter().filter(|(p, _)| !p.is_root()));
        }
        // Everything beneath `p` sorts between `p` and `p0` (`'0'` follows
        // `'/'`), mixed with siblings such as `p-x` and `p.x`.
        let mut end = path.as_bytes().to_vec();
        end.push(b'0');
        let path = path.clone();
        Box::new(
            self.entries
                .range((Bound::Excluded(path.clone()), Bound::Unbounded))
                .take_while(move |(p, _)| p.as_bytes() < end.as_slice())
                .filter(move |(p, _)| is_beneath(p, &path)),
        )
    }
}

/// `p` lies strictly beneath `ancestor`.
pub fn is_beneath(p: &RelPath, ancestor: &RelPath) -> bool {
    if ancestor.is_root() {
        return !p.is_root();
    }
    p.as_bytes()
        .strip_prefix(ancestor.as_bytes())
        .is_some_and(|rest| rest.first() == Some(&b'/'))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::{Kind, VersionVector};

    fn rp(s: &str) -> RelPath {
        RelPath::new(s).unwrap()
    }

    #[test]
    fn descendants_skip_siblings_that_sort_inside() {
        let e = Entry::new(Kind::Dir, 0o755, 0, VersionVector::new());
        let paths = [
            "d", "d!", "d-x", "d.x", "d/a", "d/a/b", "d/z", "d0", "e", "dd/x",
        ];
        let s = Snapshot::new(ReplicaId(1), paths.iter().map(|p| (rp(p), e.clone())));
        let under = |p: &str| {
            s.descendants(&rp(p))
                .map(|(p, _)| String::from_utf8(p.as_bytes().to_vec()).unwrap())
                .collect::<Vec<_>>()
        };
        assert_eq!(under("d"), ["d/a", "d/a/b", "d/z"]);
        assert_eq!(under("d/a"), ["d/a/b"]);
        assert_eq!(under("e"), Vec::<String>::new());
        assert_eq!(under("").len(), paths.len());
        assert!(is_beneath(&rp("d/a"), &rp("d")));
        assert!(!is_beneath(&rp("d"), &rp("d")));
        assert!(!is_beneath(&rp("dd/x"), &rp("d")));
    }

    fn at(seq: u64) -> Entry {
        let mut e = Entry::new(Kind::Dir, 0o755, 0, VersionVector::new());
        e.seq = seq;
        e
    }

    #[test]
    fn cache_reads_only_what_changed_and_copies_only_a_held_snapshot() {
        let mut c = SnapshotCache::new(ReplicaId(1));
        c.update(vec![(rp("a"), at(1)), (rp("b"), at(2)), (rp("c"), at(3))]);
        assert_eq!(c.seen(), 3);
        let held = c.get();
        // Nothing changed: no copy, although a snapshot is held.
        c.update(Vec::new());
        assert!(Arc::ptr_eq(&held, &c.get()));
        // A removal waits for the next update.
        c.removed([rp("b")]);
        assert_eq!(c.get().len(), 3);
        c.update(vec![(rp("a"), at(5)), (rp("d"), at(4))]);
        assert_eq!(c.seen(), 5);
        let snap = c.get();
        let paths: Vec<_> = snap
            .entries()
            .map(|(p, e)| (p.to_string(), e.seq))
            .collect();
        assert_eq!(paths, [("a".into(), 5), ("c".into(), 3), ("d".into(), 4)]);
        // The held one was copied, not changed.
        assert_eq!(held.len(), 3);
        assert_eq!(c.stats(), CacheStats { read: 5, copies: 1 });
        drop((held, snap));
        c.update(vec![(rp("e"), at(6))]);
        assert_eq!(c.stats(), CacheStats { read: 6, copies: 1 });
        c.check(vec![
            (rp("a"), at(5)),
            (rp("c"), at(3)),
            (rp("d"), at(4)),
            (rp("e"), at(6)),
        ]);
        c.reset();
        assert_eq!((c.seen(), c.get().len(), c.stats().read), (0, 0, 6));
    }

    #[test]
    #[should_panic(expected = "differs from the index")]
    fn cache_check_fails_loudly() {
        let mut c = SnapshotCache::new(ReplicaId(1));
        c.update(vec![(rp("a"), at(1))]);
        c.check(vec![(rp("a"), at(1)), (rp("b"), at(2))]);
    }
}
