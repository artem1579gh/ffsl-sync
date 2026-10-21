//! Continuous sync loop driven by watcher hints and a periodic timer (design §5.9, §6.4).
//!
//! [`Daemon::run`] starts both replicas' watchers, then runs a full sync
//! cycle (watch first, then scan, so nothing changed in between is missed).
//! After that it waits for:
//! - **hints** from either watcher: a cycle scoped to the dirty paths (both
//!   replicas scan them), or a full one after an overflow;
//! - the **rescan timer** ([`Daemon::DEFAULT_RESCAN`]): a full cycle, the
//!   backstop for anything the watchers missed;
//! - a **retry**: paths a cycle left unresolved are synced again after
//!   [`Daemon::RETRY_DELAY`], even if no new event names them. The delay
//!   doubles for each cycle in a row that leaves a path unresolved, up to
//!   [`Daemon::MAX_RETRY_DELAY`] (issue #12);
//! - a **quarantine deadline**: replaced old inodes are swept when their
//!   grace period ends (§5.3 step 4(f)); they are also swept right after
//!   each cycle, which unlinks those a write lease can be taken on;
//! - the **stop** channel (a message, or its sender dropped).
//!
//! A remote replica (design §7.1) whose connection is lost fails the cycle
//! with [`Error::Connection`]; the daemon then waits
//! [`Daemon::RECONNECT_DELAY`] and runs a full cycle, which connects again.
//! Its hints come from the server; after the hint connection was lost and
//! made again, a full rescan is asked for.
//!
//! Any other failed cycle ends [`Daemon::run`]. [`Daemon::run_reopening`]
//! then opens the replicas again and goes on, after a wait, if the error may
//! pass ([`Error::may_pass`], issue #12).
//!
//! Our own writes produce inotify events too. They cause one more cycle
//! scoped to the written paths, whose scan finds the index already up to date
//! (§5.3 step 5), so it applies nothing: no echo.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender, select};

use crate::engine::{Engine, SyncReport};
use crate::error::{Error, Result};
use crate::fs::RelPath;
use crate::replica::Housekeeping;
use crate::scan::Scope;
use crate::status::PairStatus;
use crate::watch::Hint;

/// Runs a pair of replicas continuously.
#[derive(Clone, Debug)]
pub struct Daemon {
    engine: Engine,
    rescan_every: Duration,
    retry_delay: Duration,
    restart_delay: Duration,
    reports: Option<Sender<CycleReport>>,
    status_dir: Option<PathBuf>,
}

impl Default for Daemon {
    fn default() -> Daemon {
        Daemon {
            engine: Engine::new(),
            rescan_every: Daemon::DEFAULT_RESCAN,
            retry_delay: Daemon::RETRY_DELAY,
            restart_delay: Daemon::RESTART_DELAY,
            reports: None,
            status_dir: None,
        }
    }
}

/// One sync cycle the daemon ran.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CycleReport {
    /// A full scan (the first cycle, the timer, or a watcher overflow), as
    /// opposed to one scoped to dirty paths.
    pub full: bool,
    /// The dirty paths a scoped cycle scanned (empty for a full one).
    pub paths: Vec<RelPath>,
    pub report: SyncReport,
}

/// Totals over a daemon run.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DaemonStats {
    pub cycles: u64,
    pub full_cycles: u64,
    /// Steps applied (the action counter).
    pub applied: u64,
    /// Files written as a block-level delta (design §7.1), among `applied`.
    pub deltas: u64,
    pub conflicts: u64,
    /// Cycles that left paths unresolved or failed.
    pub unconverged: u64,
}

/// What the next cycle has to scan.
#[derive(Default)]
struct Todo {
    full: bool,
    paths: BTreeSet<RelPath>,
}

/// A path left unresolved: when to try it again, after how many cycles in a
/// row left it so.
struct Retry {
    at: Instant,
    tries: u32,
}

impl Todo {
    fn add(&mut self, hint: Hint) {
        match hint {
            Hint::FullRescan => self.full = true,
            Hint::Paths(paths) => self.paths.extend(paths),
        }
    }

    fn is_empty(&self) -> bool {
        !self.full && self.paths.is_empty()
    }
}

impl Daemon {
    /// Period of the backstop full rescan (design §5.9).
    pub const DEFAULT_RESCAN: Duration = Duration::from_secs(10 * 60);
    /// Delay before paths a cycle left unresolved are tried again.
    pub const RETRY_DELAY: Duration = Duration::from_secs(1);
    /// The longest delay before a path left unresolved by many cycles in a
    /// row is tried again (unless an event names it sooner).
    pub const MAX_RETRY_DELAY: Duration = Duration::from_secs(5 * 60);
    /// The first wait before [`Daemon::run_reopening`] opens the replicas
    /// again; it doubles while no cycle completes in between, up to
    /// [`Daemon::MAX_RESTART_DELAY`].
    pub const RESTART_DELAY: Duration = Duration::from_secs(5);
    /// See [`Daemon::RESTART_DELAY`].
    pub const MAX_RESTART_DELAY: Duration = Duration::from_secs(5 * 60);
    /// Delay before a full cycle after a remote replica's connection was
    /// lost.
    pub const RECONNECT_DELAY: Duration = Duration::from_secs(5);

    pub fn new() -> Daemon {
        Daemon::default()
    }

    /// Overrides the engine (e.g. its round limit).
    pub fn engine(mut self, engine: Engine) -> Daemon {
        self.engine = engine;
        self
    }

    /// Overrides [`Daemon::DEFAULT_RESCAN`].
    pub fn rescan_every(mut self, period: Duration) -> Daemon {
        self.rescan_every = period;
        self
    }

    /// Overrides [`Daemon::RETRY_DELAY`].
    pub fn retry_delay(mut self, delay: Duration) -> Daemon {
        self.retry_delay = delay;
        self
    }

    /// Overrides [`Daemon::RESTART_DELAY`].
    pub fn restart_delay(mut self, delay: Duration) -> Daemon {
        self.restart_delay = delay;
        self
    }

    /// Sends a [`CycleReport`] after every cycle (for tests and monitoring).
    pub fn reports(mut self, tx: Sender<CycleReport>) -> Daemon {
        self.reports = Some(tx);
        self
    }

    /// Saves the pair's [`PairStatus`] in `pair_dir` after every cycle and
    /// quarantine sweep, for `status` (which cannot open the indexes while
    /// the daemon holds them).
    pub fn status_dir(mut self, pair_dir: PathBuf) -> Daemon {
        self.status_dir = Some(pair_dir);
        self
    }

    fn save_status(&self, a: &dyn Housekeeping, b: &dyn Housekeeping) {
        let Some(dir) = &self.status_dir else { return };
        if let Err(e) = PairStatus::of(a, b).and_then(|st| st.save(dir)) {
            tracing::warn!(error = %e, "cannot save the status report");
        }
    }

    /// Syncs `a` and `b` until `stop` receives a message or is closed.
    ///
    /// Fails only when a cycle fails as a whole (an index or root failure;
    /// a lost connection is retried); per-path problems are logged and
    /// retried. Quarantined files may be left when it returns: sweep them
    /// before exiting.
    pub fn run(
        &self,
        a: &mut dyn Housekeeping,
        b: &mut dyn Housekeeping,
        stop: &Receiver<()>,
    ) -> Result<DaemonStats> {
        let mut stats = DaemonStats::default();
        self.run_into(a, b, stop, &mut stats)?;
        Ok(stats)
    }

    /// Runs [`Daemon::run`] on `replicas`, and hands them to `close` when it
    /// returns. If it failed with an error that may pass
    /// ([`Error::may_pass`]: `EIO`, `ENOSPC` in the state directory, …), it
    /// waits, opens them again with `open` and goes on (issue #12). Opening
    /// them again matters: the index database refuses every operation after
    /// a failed write until it is reopened, and opening a replica replays
    /// its journal as after a crash. The wait starts at [`Daemon::RESTART_DELAY`] and
    /// doubles while no cycle completes in between. If opening them again
    /// fails with an error that may pass, it waits and tries again.
    ///
    /// Fails with an error that cannot pass (a corrupt index, a missing
    /// root marker). Returns the totals of every run when `stop` receives a
    /// message or is closed.
    pub fn run_reopening<R: Housekeeping>(
        &self,
        mut replicas: [R; 2],
        mut open: impl FnMut() -> Result<[R; 2]>,
        mut close: impl FnMut([R; 2]),
        stop: &Receiver<()>,
    ) -> Result<DaemonStats> {
        let mut stats = DaemonStats::default();
        let max_delay = Self::MAX_RESTART_DELAY.max(self.restart_delay);
        let mut delay = self.restart_delay;
        loop {
            let cycles = stats.cycles;
            let [a, b] = &mut replicas;
            let res = self.run_into(a, b, stop, &mut stats);
            close(replicas);
            let e = match res {
                Ok(()) => return Ok(stats),
                Err(e) if !e.may_pass() => return Err(e),
                Err(e) => e,
            };
            if stats.cycles > cycles {
                delay = self.restart_delay;
            }
            tracing::warn!(error = %e, ?delay, "sync cycle failed; opening the replicas again");
            replicas = loop {
                select! {
                    recv(stop) -> _ => return Ok(stats),
                    default(delay) => {}
                }
                delay = (delay * 2).min(max_delay);
                match open() {
                    Ok(replicas) => break replicas,
                    Err(e) if e.may_pass() => {
                        tracing::warn!(error = %e, ?delay, "cannot open the replicas again; trying again");
                    }
                    Err(e) => return Err(e),
                }
            };
            tracing::info!("replicas open again");
        }
    }

    /// When a path left unresolved by `tries` cycles in a row is tried again.
    fn retry_after(&self, tries: u32) -> Duration {
        let max = Self::MAX_RETRY_DELAY.max(self.retry_delay);
        self.retry_delay
            .saturating_mul(1 << tries.saturating_sub(1).min(16))
            .min(max)
    }

    /// [`Daemon::run`], adding to `stats`.
    fn run_into(
        &self,
        a: &mut dyn Housekeeping,
        b: &mut dyn Housekeeping,
        stop: &Receiver<()>,
        stats: &mut DaemonStats,
    ) -> Result<()> {
        let mut hints = [a.watch(), b.watch()].map(|h| h.unwrap_or_else(crossbeam_channel::never));
        let mut todo = Todo {
            full: true,
            paths: BTreeSet::new(),
        };
        let mut next_full = Instant::now() + self.rescan_every;
        let mut retry: BTreeMap<RelPath, Retry> = BTreeMap::new();
        loop {
            let mut changed = false;
            let now = Instant::now();
            if now >= next_full {
                todo.full = true;
            }
            for (path, r) in retry.iter_mut().filter(|(_, r)| r.at <= now) {
                todo.paths.insert(path.clone());
                // Not again before the cycle's outcome, should it fail.
                r.at = now + self.retry_after(r.tries + 1);
            }
            if !todo.is_empty() {
                let cycle = std::mem::take(&mut todo);
                let report = match self.cycle(a, b, cycle, stats) {
                    Ok(report) => report,
                    Err(e) if e.is_disconnected() => {
                        tracing::warn!(error = %e, delay = ?Self::RECONNECT_DELAY, "sync cycle failed; trying again");
                        stats.unconverged += 1;
                        next_full = Instant::now() + Self::RECONNECT_DELAY;
                        continue;
                    }
                    Err(e) => return Err(e),
                };
                if report.full {
                    next_full = Instant::now() + self.rescan_every;
                }
                // Every cycle reconciles the whole indexes: a path it did
                // not leave unresolved is done.
                let unresolved: BTreeSet<&RelPath> = report.report.unresolved.iter().collect();
                retry.retain(|path, _| unresolved.contains(path));
                let now = Instant::now();
                for path in unresolved {
                    let r = retry
                        .entry(path.clone())
                        .or_insert(Retry { at: now, tries: 0 });
                    r.tries += 1;
                    r.at = now + self.retry_after(r.tries);
                }
                if let Some(tx) = &self.reports {
                    let _ = tx.send(report);
                }
                changed = true;
            }
            // Right after a cycle too: what can be leased goes at once.
            let after_cycle = changed;
            let mut sweep = |r: &mut dyn Housekeeping| {
                if r.next_sweep()
                    .is_some_and(|d| after_cycle || d <= Instant::now())
                {
                    r.sweep();
                    changed = true;
                }
            };
            sweep(a);
            sweep(b);
            if changed {
                self.save_status(a, b);
            }

            let wake = [
                Some(next_full),
                retry.values().map(|r| r.at).min(),
                a.next_sweep(),
                b.next_sweep(),
            ]
            .into_iter()
            .flatten()
            .min()
            .expect("the rescan timer is always set");
            let timeout = wake.saturating_duration_since(Instant::now());
            let (mut closed, mut stopping) = (None, false);
            select! {
                recv(stop) -> _ => stopping = true,
                recv(hints[0]) -> h => match h {
                    Ok(h) => todo.add(h),
                    Err(_) => closed = Some(0),
                },
                recv(hints[1]) -> h => match h {
                    Ok(h) => todo.add(h),
                    Err(_) => closed = Some(1),
                },
                default(timeout) => {}
            }
            if stopping {
                break;
            }
            if let Some(i) = closed {
                tracing::warn!(
                    side = ["A", "B"][i],
                    "watcher stopped; relying on periodic rescans"
                );
                hints[i] = crossbeam_channel::never();
            }
            // Hints that arrived meanwhile join the same cycle.
            for h in &hints {
                for hint in h.try_iter() {
                    todo.add(hint);
                }
            }
        }
        tracing::info!(?stats, "daemon stopped");
        Ok(())
    }

    /// Runs one cycle for `todo`.
    fn cycle(
        &self,
        a: &mut dyn Housekeeping,
        b: &mut dyn Housekeeping,
        todo: Todo,
        stats: &mut DaemonStats,
    ) -> Result<CycleReport> {
        let (full, paths) = if todo.full {
            (true, Vec::new())
        } else {
            (false, todo.paths.into_iter().collect::<Vec<_>>())
        };
        let scope = if full {
            Scope::Full
        } else {
            Scope::Paths(paths.clone())
        };
        tracing::debug!(full, paths = paths.len(), "sync cycle");
        let report = self.engine.sync(a, b, scope)?;
        stats.cycles += 1;
        stats.full_cycles += u64::from(full);
        stats.applied += report.applied as u64;
        stats.deltas += report.deltas as u64;
        stats.conflicts += report.conflicts.len() as u64;
        if !report.is_converged() {
            stats.unconverged += 1;
            for (p, e) in &report.errors {
                tracing::warn!(path = %p, error = %e, "not synced");
            }
        }
        Ok(CycleReport {
            full,
            paths,
            report,
        })
    }
}

/// Blocks `SIGINT` and `SIGTERM` in this thread and every thread it starts
/// from now on, and turns them into a message on the returned channel, sent
/// by a dedicated `sigwait` thread. A second signal exits the process at
/// once.
///
/// Call it before any other thread is started, so no thread runs the
/// default handler (which would kill the process mid-commit).
pub fn shutdown_signals() -> Result<Receiver<()>> {
    // SAFETY: plain libc calls on a local, initialized signal set.
    let set = unsafe {
        let mut set: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut set);
        libc::sigaddset(&mut set, libc::SIGINT);
        libc::sigaddset(&mut set, libc::SIGTERM);
        let rc = libc::pthread_sigmask(libc::SIG_BLOCK, &set, std::ptr::null_mut());
        if rc != 0 {
            return Err(Error::io(
                "block SIGINT and SIGTERM",
                std::io::Error::from_raw_os_error(rc),
            ));
        }
        set
    };
    let (tx, rx) = crossbeam_channel::bounded(1);
    std::thread::Builder::new()
        .name("signals".into())
        .spawn(move || {
            let mut first = true;
            loop {
                let mut sig = 0;
                // SAFETY: `set` is a valid signal set; `sig` is a valid out pointer.
                if unsafe { libc::sigwait(&set, &mut sig) } != 0 {
                    continue;
                }
                if first {
                    tracing::info!(signal = sig, "shutting down (signal again to exit at once)");
                    let _ = tx.try_send(());
                    first = false;
                } else {
                    tracing::warn!(signal = sig, "exiting at once");
                    std::process::exit(128 + sig);
                }
            }
        })
        .map_err(|e| Error::io("spawn signal thread", e))?;
    Ok(rx)
}
