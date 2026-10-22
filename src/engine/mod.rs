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
pub use reconcile::{Action, ActionKind, Resolution, SkipReason, reconcile, reconcile_paths};

use std::collections::{BTreeMap, BTreeSet};
use std::ops::Bound;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

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
///
/// Besides the entries it keeps what the engine would otherwise find by
/// walking all of them every round (issue #15): the number of live entries,
/// the tombstones, and a journal of the paths that recent updates changed
/// ([`Snapshot::changed_since`]).
#[derive(Clone, Debug)]
pub struct Snapshot {
    replica: ReplicaId,
    entries: BTreeMap<RelPath, Entry>,
    /// Live entries, the root left out.
    live: usize,
    tombstones: BTreeSet<RelPath>,
    /// Which cache this snapshot comes from: unique in the process. A
    /// snapshot built on its own ([`Snapshot::new`]) has a lineage of its
    /// own.
    lineage: u64,
    /// Updates applied since the snapshot was built.
    version: u64,
    /// The paths each update changed (put or removed), with the update's
    /// version, in version order: every change after `floor`.
    journal: Vec<(u64, RelPath)>,
    floor: u64,
}

/// Where a [`Snapshot`] stood: its lineage and version. A later snapshot of
/// the same lineage tells what changed since ([`Snapshot::changed_since`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Mark {
    lineage: u64,
    version: u64,
}

/// The journal holds this many changes, or one for every eight entries if
/// that is more. A longer one is cleared: whoever is that far behind
/// compares everything again, at a cost in proportion to the change.
const JOURNAL_MIN: usize = 4096;

impl Snapshot {
    /// A later entry for the same path replaces an earlier one.
    pub fn new(
        replica: ReplicaId,
        entries: impl IntoIterator<Item = (RelPath, Entry)>,
    ) -> Snapshot {
        static LINEAGES: AtomicU64 = AtomicU64::new(1);
        let mut snap = Snapshot {
            replica,
            entries: entries.into_iter().collect(),
            live: 0,
            tombstones: BTreeSet::new(),
            lineage: LINEAGES.fetch_add(1, Ordering::Relaxed),
            version: 0,
            journal: Vec::new(),
            floor: 0,
        };
        snap.count();
        snap
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Live entries, the root left out.
    pub fn live(&self) -> usize {
        self.live
    }

    /// The tombstones, in path order.
    pub fn tombstones(&self) -> impl Iterator<Item = (&RelPath, &Entry)> {
        self.tombstones.iter().map(|p| (p, &self.entries[p]))
    }

    /// Where this snapshot stands now.
    pub fn mark(&self) -> Mark {
        Mark {
            lineage: self.lineage,
            version: self.version,
        }
    }

    /// The paths changed (put or removed) since this snapshot's lineage
    /// stood at `mark`, in no particular order and maybe repeated. `None`
    /// if that is not known: another lineage, or the journal no longer
    /// reaches back that far.
    pub fn changed_since(&self, mark: Mark) -> Option<impl Iterator<Item = &RelPath>> {
        if mark.lineage != self.lineage || mark.version < self.floor || mark.version > self.version
        {
            return None;
        }
        let from = self.journal.partition_point(|(v, _)| *v <= mark.version);
        Some(self.journal[from..].iter().map(|(_, p)| p))
    }

    /// Counts the live entries and indexes the tombstones anew.
    fn count(&mut self) {
        self.live = self
            .entries
            .iter()
            .filter(|(p, e)| !p.is_root() && e.is_live())
            .count();
        self.tombstones = self
            .entries
            .iter()
            .filter(|(_, e)| e.is_tombstone())
            .map(|(p, _)| p.clone())
            .collect();
    }

    /// Takes `old`, the entry that was at `path`, out of the counts.
    fn uncount(&mut self, path: &RelPath, old: Option<Entry>) {
        match old {
            Some(e) if e.is_tombstone() => {
                self.tombstones.remove(path);
            }
            Some(e) if e.is_live() && !path.is_root() => self.live -= 1,
            _ => {}
        }
    }

    fn insert(&mut self, path: RelPath, entry: Entry) {
        let (tombstone, live) = (entry.is_tombstone(), entry.is_live() && !path.is_root());
        let old = self.entries.insert(path.clone(), entry);
        self.uncount(&path, old);
        if tombstone {
            self.tombstones.insert(path);
        } else if live {
            self.live += 1;
        }
    }

    fn remove(&mut self, path: &RelPath) {
        let old = self.entries.remove(path);
        self.uncount(path, old);
    }
}

/// The derived counts and the journal are left out: two snapshots are equal
/// if they hold the same entries of the same replica.
impl PartialEq for Snapshot {
    fn eq(&self, other: &Snapshot) -> bool {
        self.replica == other.replica && self.entries == other.entries
    }
}

impl Eq for Snapshot {}

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
    /// since [`SnapshotCache::seen`], in the wire form. Each update that
    /// changes something is a new version of the snapshot, whose journal
    /// notes the paths it changed.
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
        snap.version += 1;
        let version = snap.version;
        let cap = JOURNAL_MIN.max(snap.entries.len() / 8);
        let journal = self.removed.len() + changes.len() <= cap;
        for path in self.removed.drain(..) {
            snap.remove(&path);
            if journal {
                snap.journal.push((version, path));
            }
        }
        self.seen = changes.iter().fold(self.seen, |s, (_, e)| s.max(e.seq));
        if journal {
            snap.journal
                .extend(changes.iter().map(|(p, _)| (version, p.clone())));
        }
        if snap.entries.is_empty() {
            // Built in one go, the tree's nodes are full: a first read of a
            // big index takes less memory so.
            snap.entries = changes.into_iter().collect();
            snap.count();
        } else {
            for (path, entry) in changes {
                snap.insert(path, entry);
            }
        }
        if !journal || snap.journal.len() > cap {
            snap.journal.clear();
            snap.floor = version;
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
            assert_eq!(
                (self.snap.live, &self.snap.tombstones),
                (fresh.live, &fresh.tombstones),
                "the cached index snapshot of replica {} miscounts its entries",
                self.snap.replica
            );
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
