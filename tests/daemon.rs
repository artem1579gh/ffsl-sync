//! The daemon on two real tempdir replicas, driven by inotify (T16).
//!
//! Each test runs `Daemon::run` on a thread while the test edits the trees
//! with plain `std::fs`, then stops it and checks `assert_converged`.

mod harness;

use std::collections::BTreeMap;
use std::fs;
use std::io::Read;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crossbeam_channel::Receiver;
use ffsl_sync::config::{ReplicaId, SymlinkPolicy};
use ffsl_sync::daemon::{CycleReport, Daemon, DaemonStats};
use ffsl_sync::engine::Snapshot;
use ffsl_sync::fs::{RelPath, is_reserved};
use ffsl_sync::index::{Entry, PeerState, VersionVector};
use ffsl_sync::replica::{
    ContentReader, Housekeeping, LocalReplica, Op, Outcome, Precondition, Replica,
};
use ffsl_sync::scan::{ScanStats, Scope};
use ffsl_sync::status::ReplicaStatus;
use ffsl_sync::watch::{ChannelSource, Event, Hint};
use harness::{ID_A, ID_B, Pair, open_replica};

/// What a test sees while the daemon runs.
struct Ctl {
    a: PathBuf,
    b: PathBuf,
    reports: Receiver<CycleReport>,
}

impl Ctl {
    /// Waits for the next cycle report.
    fn next_cycle(&self, timeout: Duration) -> CycleReport {
        self.reports
            .recv_timeout(timeout)
            .unwrap_or_else(|_| panic!("no sync cycle within {timeout:?}"))
    }

    /// The cycle reports that arrive within `d`.
    fn cycles_for(&self, d: Duration) -> Vec<CycleReport> {
        let end = Instant::now() + d;
        let mut out = Vec::new();
        while let Ok(r) = self
            .reports
            .recv_timeout(end.saturating_duration_since(Instant::now()))
        {
            out.push(r);
        }
        out
    }

    /// Waits until no cycle has run for `quiet`, returning the cycles seen.
    fn settle(&self, quiet: Duration) -> Vec<CycleReport> {
        let mut out = Vec::new();
        while let Ok(r) = self.reports.recv_timeout(quiet) {
            out.push(r);
        }
        out
    }
}

/// Runs the daemon on `p` while `f` runs; returns `f`'s result and the
/// daemon's totals. The daemon stops when `f` returns or panics.
fn with_daemon<R>(p: &mut Pair, daemon: Daemon, f: impl FnOnce(&Ctl) -> R) -> (R, DaemonStats) {
    let (tx, reports) = crossbeam_channel::unbounded();
    let ctl = Ctl {
        a: p.a.root().to_path_buf(),
        b: p.b.root().to_path_buf(),
        reports,
    };
    let daemon = daemon.reports(tx);
    let (ra, rb) = (&mut p.a.replica, &mut p.b.replica);
    std::thread::scope(|s| {
        // Dropped on return or unwind, which stops the daemon.
        let (stop, stopped) = crossbeam_channel::bounded::<()>(1);
        let running = s.spawn(move || daemon.run(ra, rb, &stopped));
        let out = f(&ctl);
        drop(stop);
        let stats = running.join().unwrap().expect("daemon failed");
        (out, stats)
    })
}

/// Polls `cond` until it holds; panics after `timeout`. Returns the time it
/// took.
fn wait_until(what: &str, timeout: Duration, mut cond: impl FnMut() -> bool) -> Duration {
    let t0 = Instant::now();
    loop {
        if cond() {
            return t0.elapsed();
        }
        assert!(t0.elapsed() < timeout, "{what}: not within {timeout:?}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// The tree under `root` as path → content (`/` for a directory), ignoring
/// reserved names. Tolerates a tree that changes while it is read (an object
/// that vanishes is left out).
fn snapshot(root: &Path) -> BTreeMap<String, String> {
    fn walk(root: &Path, rel: &Path, out: &mut BTreeMap<String, String>) {
        let Ok(dir) = fs::read_dir(root.join(rel)) else {
            return;
        };
        for e in dir.flatten() {
            if is_reserved(e.file_name().as_bytes()) {
                continue;
            }
            let rel = rel.join(e.file_name());
            let key = rel.to_str().unwrap().to_owned();
            let Ok(md) = e.path().symlink_metadata() else {
                continue;
            };
            if md.is_dir() {
                out.insert(key, "/".into());
                walk(root, &rel, out);
            } else if let Ok(content) = fs::read(e.path()) {
                out.insert(key, String::from_utf8_lossy(&content).into_owned());
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(root, Path::new(""), &mut out);
    out
}

fn read(path: impl AsRef<Path>) -> Option<String> {
    fs::read(path).ok().map(|c| String::from_utf8(c).unwrap())
}

fn applied(cycles: &[CycleReport]) -> usize {
    cycles.iter().map(|c| c.report.applied).sum()
}

const START: Duration = Duration::from_secs(10);

#[test]
fn edit_in_a_appears_in_b_within_3s_without_echo() {
    let mut p = Pair::new(SymlinkPolicy::Links);
    p.a.write("doc.txt", "v1");
    p.a.write("dir/other.txt", "o");
    let ((), stats) = with_daemon(&mut p, Daemon::new(), |c| {
        // The first cycle is a full one and copies the initial tree.
        let first = c.next_cycle(START);
        assert!(first.full);
        assert_eq!(first.report.applied, 3);
        // Our own writes on B cause scoped cycles that do nothing.
        let echo = c.settle(Duration::from_secs(1));
        assert_eq!(applied(&echo), 0, "echo of the first cycle: {echo:#?}");

        fs::write(c.a.join("doc.txt"), "v2").unwrap();
        let took = wait_until("edit reaches B", Duration::from_secs(3), || {
            read(c.b.join("doc.txt")).as_deref() == Some("v2")
        });
        eprintln!("edit reached B after {took:?}");

        // Exactly one action: the write to B. The cycles that follow it
        // (inotify reporting our write on B) apply nothing.
        let after = c.settle(Duration::from_secs(1));
        assert_eq!(applied(&after), 1, "{after:#?}");
        assert!(after.iter().all(|c| !c.full), "{after:#?}");
        assert!(
            after.len() >= 2 && after.last().unwrap().report.applied == 0,
            "expected an echo cycle that applies nothing: {after:#?}"
        );
    });
    assert_eq!(stats.applied, 4);
    assert_eq!(stats.full_cycles, 1);
    p.assert_converged();
}

#[test]
fn burst_of_a_three_level_tree_arrives_in_full() {
    let mut p = Pair::new(SymlinkPolicy::Links);
    with_daemon(&mut p, Daemon::new(), |c| {
        c.next_cycle(START);
        // As fast as possible: directories are created and filled before
        // the watcher can add their watches.
        for i in 0..4 {
            for j in 0..4 {
                for k in 0..4 {
                    let d = c.a.join(format!("d{i}/e{j}/f{k}"));
                    fs::create_dir_all(&d).unwrap();
                    fs::write(d.join("leaf.txt"), format!("{i}{j}{k}")).unwrap();
                }
                fs::write(c.a.join(format!("d{i}/e{j}/mid.txt")), "m").unwrap();
            }
            fs::write(c.a.join(format!("d{i}/top.txt")), "t").unwrap();
        }
        let want = snapshot(&c.a);
        assert_eq!(want.len(), 4 + 16 + 64 + 64 + 16 + 4);
        wait_until("the whole tree reaches B", Duration::from_secs(10), || {
            snapshot(&c.b) == want
        });
        // Later edits deep inside the new tree are watched too (after the
        // echo cycles, whose scans would otherwise pick them up anyway).
        c.settle(Duration::from_secs(1));
        fs::write(c.a.join("d3/e3/f3/late.txt"), "late").unwrap();
        wait_until("an edit in the new tree", Duration::from_secs(3), || {
            read(c.b.join("d3/e3/f3/late.txt")).is_some()
        });
        c.settle(Duration::from_secs(1));
    });
    p.assert_converged();
}

#[test]
fn injected_overflow_triggers_a_full_rescan() {
    let mut p = Pair::new(SymlinkPolicy::Links);
    // A's watcher reads only what the test injects: its edits go unseen.
    let (inject, source) = ChannelSource::new();
    p.a.replica.local_mut().set_event_source(Box::new(source));
    p.a.write("old.txt", "old");
    let ((), stats) = with_daemon(&mut p, Daemon::new(), |c| {
        assert!(c.next_cycle(START).full);
        c.settle(Duration::from_millis(500));

        fs::write(c.a.join("missed.txt"), "missed").unwrap();
        fs::remove_file(c.a.join("old.txt")).unwrap();
        let none = c.cycles_for(Duration::from_secs(1));
        assert!(none.is_empty(), "nothing should notice the edit: {none:#?}");
        assert!(c.b.join("old.txt").exists() && !c.b.join("missed.txt").exists());

        inject.send(Event::Overflow).unwrap();
        let cycle = c.next_cycle(Duration::from_secs(3));
        assert!(cycle.full, "{cycle:#?}");
        assert_eq!(cycle.report.applied, 2, "{cycle:#?}");
        assert_eq!(read(c.b.join("missed.txt")).as_deref(), Some("missed"));
        assert!(!c.b.join("old.txt").exists());
        c.settle(Duration::from_millis(500));
    });
    assert_eq!(stats.full_cycles, 2);
    p.assert_converged();
}

#[test]
fn periodic_rescan_catches_what_the_watchers_miss() {
    let mut p = Pair::new(SymlinkPolicy::Links);
    let (_quiet_a, source) = ChannelSource::new();
    p.a.replica.local_mut().set_event_source(Box::new(source));
    let (_quiet_b, source) = ChannelSource::new();
    p.b.replica.local_mut().set_event_source(Box::new(source));
    let daemon = Daemon::new().rescan_every(Duration::from_secs(1));
    with_daemon(&mut p, daemon, |c| {
        assert!(c.next_cycle(START).full);
        fs::write(c.b.join("from_b.txt"), "b").unwrap();
        wait_until("the timer's rescan", Duration::from_secs(3), || {
            c.a.join("from_b.txt").exists()
        });
        assert!(c.next_cycle(Duration::from_secs(3)).full);
    });
    p.assert_converged();
}

#[test]
fn quiet_tree_causes_no_actions() {
    let mut p = Pair::new(SymlinkPolicy::Links);
    for i in 0..10 {
        p.a.write(&format!("a{i}/f.txt"), format!("a{i}"));
        p.b.write(&format!("b{i}.txt"), format!("b{i}"));
    }
    with_daemon(&mut p, Daemon::new(), |c| {
        let first = c.next_cycle(START);
        assert_eq!(first.report.applied, 30);
        assert!(first.report.is_converged());
        c.settle(Duration::from_secs(1));
        assert_eq!(snapshot(&c.a), snapshot(&c.b));

        let quiet = c.cycles_for(Duration::from_secs(5));
        assert!(quiet.is_empty(), "cycles in a quiet tree: {quiet:#?}");
    });
    p.assert_converged();
}

#[test]
fn edits_on_both_sides_converge_with_a_conflict_copy() {
    let mut p = Pair::new(SymlinkPolicy::Links);
    p.a.write("shared.txt", "base");
    p.sync();
    with_daemon(&mut p, Daemon::new(), |c| {
        c.next_cycle(START);
        c.settle(Duration::from_millis(500));
        fs::write(c.a.join("shared.txt"), "from a").unwrap();
        fs::write(c.b.join("shared.txt"), "from b, newer").unwrap();
        wait_until("both sides agree", Duration::from_secs(5), || {
            let (a, b) = (snapshot(&c.a), snapshot(&c.b));
            a == b && a.len() == 2
        });
        c.settle(Duration::from_secs(1));
    });
    p.assert_converged();
    assert_eq!(p.a.read("shared.txt"), "from b, newer");
    assert_eq!(p.a.conflicts("").len(), 1);
}

/// `-L`: changes to what followed links point to arrive under the links'
/// paths, from the watcher alone (no periodic rescan): in the tree (the
/// referent's own path is watched too) and outside it (extra watches).
#[test]
fn followed_referents_are_watched() {
    let outside = tempfile::tempdir().unwrap();
    fs::create_dir(outside.path().join("o")).unwrap();
    fs::write(outside.path().join("o/f"), "out v1").unwrap();
    let mut p = Pair::new(SymlinkPolicy::CopyLinks);
    p.a.write("d/x", "in v1");
    p.a.symlink("l", "d");
    p.a.symlink("lo", outside.path().to_str().unwrap());
    let ((), _) = with_daemon(&mut p, Daemon::new(), |c| {
        c.next_cycle(START);
        c.settle(Duration::from_secs(1));
        assert_eq!(read(c.b.join("lo/o/f")).as_deref(), Some("out v1"));

        fs::write(outside.path().join("o/f"), "out v2").unwrap();
        wait_until("outside edit reaches B", Duration::from_secs(3), || {
            read(c.b.join("lo/o/f")).as_deref() == Some("out v2")
        });
        fs::write(c.a.join("d/x"), "in v2").unwrap();
        wait_until("edit through l reaches B", Duration::from_secs(3), || {
            read(c.b.join("l/x")).as_deref() == Some("in v2")
                && read(c.b.join("d/x")).as_deref() == Some("in v2")
        });
        // A new directory outside, then a file in it (watched after the
        // scan that found the directory).
        fs::create_dir(outside.path().join("new")).unwrap();
        wait_until("new outside dir reaches B", Duration::from_secs(3), || {
            c.b.join("lo/new").is_dir()
        });
        c.settle(Duration::from_millis(500));
        fs::write(outside.path().join("new/g"), "g").unwrap();
        wait_until("file in it reaches B", Duration::from_secs(3), || {
            read(c.b.join("lo/new/g")).as_deref() == Some("g")
        });
        c.settle(Duration::from_millis(500));
    });
    p.assert_converged();
}

/// Makes the error a scan fails with.
type ScanFault = fn() -> ffsl_sync::Error;

/// How a [`Flaky`] replica fails (issue #12).
#[derive(Clone, Default)]
struct Faults {
    /// Every scan fails with this, until the replicas are opened again.
    scan: Arc<Mutex<Option<ScanFault>>>,
    /// Every read of this path is unstable.
    stuck: Option<&'static str>,
}

/// A local replica with injected [`Faults`].
struct Flaky {
    inner: LocalReplica,
    faults: Faults,
}

impl Replica for Flaky {
    fn id(&self) -> ReplicaId {
        self.inner.id()
    }

    fn scan(&mut self, scope: Scope) -> ffsl_sync::Result<ScanStats> {
        if let Some(fail) = *self.faults.scan.lock().unwrap() {
            return Err(fail());
        }
        self.inner.scan(scope)
    }

    fn changes_since(&self, seq: u64) -> ffsl_sync::Result<Vec<(RelPath, Entry)>> {
        self.inner.changes_since(seq)
    }

    fn snapshot(&self) -> ffsl_sync::Result<Arc<Snapshot>> {
        self.inner.snapshot()
    }

    fn open_read(
        &self,
        path: &RelPath,
        expect: &Entry,
    ) -> ffsl_sync::Result<Box<dyn ContentReader>> {
        if self
            .faults
            .stuck
            .is_some_and(|s| path.as_bytes() == s.as_bytes())
        {
            return Err(ffsl_sync::Error::Unstable {
                path: path.as_bytes().to_vec(),
                reason: "stuck",
            });
        }
        self.inner.open_read(path, expect)
    }

    fn apply(
        &mut self,
        path: &RelPath,
        op: Op,
        pre: Precondition,
        content: Option<&mut dyn Read>,
    ) -> ffsl_sync::Result<Outcome> {
        self.inner.apply(path, op, pre, content)
    }

    fn watch(&mut self) -> Option<Receiver<Hint>> {
        self.inner.watch()
    }

    fn record_sync(
        &mut self,
        peer: ReplicaId,
        tombstones: Vec<(RelPath, VersionVector, PeerState)>,
        retention: Duration,
    ) -> ffsl_sync::Result<Vec<RelPath>> {
        self.inner.record_sync(peer, tombstones, retention)
    }

    fn clock(&self) -> ffsl_sync::Result<u64> {
        self.inner.clock()
    }

    fn witness(&mut self, floor: u64) -> ffsl_sync::Result<()> {
        self.inner.witness(floor)
    }
}

impl Housekeeping for Flaky {
    fn next_sweep(&self) -> Option<Instant> {
        self.inner.next_sweep()
    }

    fn sweep(&mut self) {
        self.inner.sweep();
    }

    fn status(&self, peer: ReplicaId) -> ffsl_sync::Result<ReplicaStatus> {
        self.inner.status(peer)
    }
}

/// Two flaky replicas, opened and reopened in `dirs` (A, B, state).
struct FlakyPair {
    dirs: [tempfile::TempDir; 3],
    faults: Faults,
    /// How often the pair was opened again.
    reopened: Arc<AtomicUsize>,
    /// Without inotify: only the daemon's timers make cycles.
    quiet: bool,
}

impl FlakyPair {
    fn new(faults: Faults, quiet: bool) -> FlakyPair {
        FlakyPair {
            dirs: [(); 3].map(|()| tempfile::tempdir().unwrap()),
            faults,
            reopened: Arc::default(),
            quiet,
        }
    }

    fn path(&self, side: usize, rel: &str) -> PathBuf {
        self.dirs[side].path().join(rel)
    }

    fn open(&self) -> [Flaky; 2] {
        [ID_A, ID_B].map(|id| {
            let side = usize::from(id == ID_B);
            let mut inner = open_replica(
                self.dirs[side].path(),
                self.dirs[2].path(),
                id,
                SymlinkPolicy::Links,
            );
            if self.quiet {
                let (tx, source) = ChannelSource::new();
                // The source ends when its sender goes: keep it.
                std::mem::forget(tx);
                inner.set_event_source(Box::new(source));
            }
            Flaky {
                inner,
                faults: self.faults.clone(),
            }
        })
    }

    /// Runs `daemon.run_reopening` while `f` runs. Opening the pair again
    /// clears a scan fault.
    fn run<R>(
        &self,
        daemon: Daemon,
        f: impl FnOnce(&Receiver<CycleReport>) -> R,
    ) -> (R, ffsl_sync::Result<DaemonStats>) {
        let (tx, reports) = crossbeam_channel::unbounded();
        let daemon = daemon.reports(tx);
        let open = || {
            self.reopened.fetch_add(1, Ordering::Relaxed);
            *self.faults.scan.lock().unwrap() = None;
            Ok(self.open())
        };
        std::thread::scope(|s| {
            let (stop, stopped) = crossbeam_channel::bounded::<()>(1);
            let replicas = self.open();
            let running = s.spawn(move || daemon.run_reopening(replicas, open, drop, &stopped));
            let out = f(&reports);
            drop(stop);
            (out, running.join().unwrap())
        })
    }
}

/// What every index operation fails with after a write failed (`ENOSPC`,
/// `EIO`), until the database is opened again.
fn index_io_failed() -> ffsl_sync::Error {
    ffsl_sync::Error::Db(redb::Error::PreviousIo)
}

/// Issue #12: a cycle that fails with an error that may pass (here the
/// index's, after a failed write) no longer ends the daemon: it opens the
/// replicas again after a wait, and goes on syncing.
#[test]
fn a_transient_failure_reopens_the_replicas() {
    let pair = FlakyPair::new(Faults::default(), false);
    let (waited, stats) = pair.run(
        Daemon::new().restart_delay(Duration::from_millis(200)),
        |r| {
            r.recv_timeout(START).expect("first cycle");
            *pair.faults.scan.lock().unwrap() = Some(index_io_failed);
            fs::write(pair.path(0, "after.txt"), "after").unwrap();
            let t0 = Instant::now();
            wait_until("the file reaches B", Duration::from_secs(5), || {
                read(pair.path(1, "after.txt")).as_deref() == Some("after")
            });
            t0.elapsed()
        },
    );
    let stats = stats.expect("the daemon stopped cleanly");
    assert_eq!(pair.reopened.load(Ordering::Relaxed), 1);
    assert!(stats.cycles >= 2, "{stats:?}");
    assert!(waited >= Duration::from_millis(200), "{waited:?}");
}

/// Issue #12: an error that cannot pass (a bad index) still ends the
/// daemon, without opening the replicas again.
#[test]
fn a_bad_index_ends_the_daemon() {
    let pair = FlakyPair::new(Faults::default(), false);
    let ((), res) = pair.run(
        Daemon::new().restart_delay(Duration::from_millis(10)),
        |r| {
            r.recv_timeout(START).expect("first cycle");
            *pair.faults.scan.lock().unwrap() = Some(|| ffsl_sync::Error::BadIndex {
                reason: "test".into(),
            });
            fs::write(pair.path(0, "after.txt"), "after").unwrap();
            std::thread::sleep(Duration::from_millis(500));
        },
    );
    assert!(
        matches!(res, Err(ffsl_sync::Error::BadIndex { .. })),
        "{res:?}"
    );
    assert_eq!(pair.reopened.load(Ordering::Relaxed), 0);
}

/// Issue #12: a path that every cycle leaves unresolved is tried again
/// less and less often, not every `retry_delay`.
#[test]
fn a_path_that_stays_unresolved_is_retried_less_often() {
    let faults = Faults {
        stuck: Some("stuck.txt"),
        ..Faults::default()
    };
    let pair = FlakyPair::new(faults, true);
    fs::write(pair.path(0, "stuck.txt"), "x").unwrap();
    let retry = Duration::from_millis(100);
    let (cycles, _) = pair.run(Daemon::new().retry_delay(retry), |r| {
        let first = r.recv_timeout(START).expect("first cycle");
        assert_eq!(
            first.report.unresolved,
            [RelPath::new("stuck.txt").unwrap()]
        );
        let mut at = vec![Instant::now()];
        let end = at[0] + Duration::from_millis(2500);
        while let Ok(c) = r.recv_timeout(end.saturating_duration_since(Instant::now())) {
            assert!(!c.full, "{c:#?}");
            assert_eq!(c.paths, [RelPath::new("stuck.txt").unwrap()]);
            at.push(Instant::now());
        }
        at
    });
    // Retries after 0.1, 0.2, 0.4, 0.8 s… (not 25 in 2.5 s).
    assert!((4..=6).contains(&cycles.len()), "{}", cycles.len());
    let gaps: Vec<Duration> = cycles.windows(2).map(|w| w[1] - w[0]).collect();
    assert!(gaps.windows(2).all(|g| g[1] > g[0]), "{gaps:?}");
}
