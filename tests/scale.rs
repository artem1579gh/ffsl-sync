//! Scale benchmark (design §9, T48, issue #15). Ignored by default:
//! `cargo test --release --test scale -- --ignored --nocapture`.
//!
//! Builds a tree of many small files (plus one big file) on A, syncs it to
//! an empty B, then times the cycles that matter on a big tree, and prints
//! the numbers; it asserts only that every cycle converges:
//!
//! - **initial:** the first full sync;
//! - **no-op full:** a full cycle (`sync --once`) with nothing to do;
//! - **no-op scoped:** a cycle scoped to one unchanged path, as the daemon's
//!   echo cycle after its own writes;
//! - **one file, scoped / full:** one small file changed on A, then a
//!   scoped (daemon) or a full cycle;
//! - **touch big, scoped:** the big file's mtime changed on A (T46: B's file
//!   is changed in place);
//! - **scan A, full:** a full scan of A alone, nothing changed (T49: no set
//!   of every path).
//!
//! Each line also gives the RSS before the phase and its peak RSS (`VmHWM`,
//! reset before it through `/proc/self/clear_refs`; the allocator keeps
//! freed memory, so the peak is never below the RSS before), and the most
//! heap the phase held above what was allocated when it began (counted by
//! this test's global allocator, so memory freed earlier does not hide it).
//! Both replicas, and in remote mode both servers, run in this process, so
//! it covers all of them. The two modes take turns (a lock), so each has the
//! machine to itself.
//!
//! Settings (environment variables):
//! - `FFSL_SCALE_FILES` (200000): small files in the tree;
//! - `FFSL_SCALE_BIG_MIB` (256): the big file's size;
//! - `TMPDIR`: where the trees and the state go (e.g. `/dev/shm` for tmpfs).

mod harness;

use std::alloc::{GlobalAlloc, Layout, System};
use std::fs;
use std::path::Path;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant, SystemTime};

use ffsl_sync::config::SymlinkPolicy;
use ffsl_sync::engine::SyncReport;
use ffsl_sync::replica::Replica;
use ffsl_sync::scan::Scope;
use harness::{Mode, Pair, rp};

/// Files per directory.
const PER_DIR: usize = 1000;

/// The system allocator, counting the bytes allocated now and the most at
/// any time since [`Heap::reset`].
struct Heap {
    now: AtomicUsize,
    peak: AtomicUsize,
}

#[global_allocator]
static HEAP: Heap = Heap {
    now: AtomicUsize::new(0),
    peak: AtomicUsize::new(0),
};

unsafe impl GlobalAlloc for Heap {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let p = unsafe { System.alloc(layout) };
        if !p.is_null() {
            self.grew(layout.size());
        }
        p
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) };
        self.now.fetch_sub(layout.size(), Ordering::Relaxed);
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let p = unsafe { System.realloc(ptr, layout, new_size) };
        if !p.is_null() {
            self.now.fetch_sub(layout.size(), Ordering::Relaxed);
            self.grew(new_size);
        }
        p
    }
}

impl Heap {
    fn grew(&self, size: usize) {
        let now = self.now.fetch_add(size, Ordering::Relaxed) + size;
        self.peak.fetch_max(now, Ordering::Relaxed);
    }

    /// Starts a new peak; returns the bytes allocated now.
    fn reset(&self) -> usize {
        let now = self.now.load(Ordering::Relaxed);
        self.peak.store(now, Ordering::Relaxed);
        now
    }
}

fn env(name: &str, default: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

/// A field of `/proc/self/status`, in KiB.
fn status_kib(field: &str) -> u64 {
    let status = fs::read_to_string("/proc/self/status").unwrap();
    status
        .lines()
        .find_map(|l| l.strip_prefix(field)?.strip_prefix(':'))
        .and_then(|v| v.trim().trim_end_matches("kB").trim().parse().ok())
        .unwrap_or(0)
}

/// Resets the peak RSS to the current RSS.
fn reset_peak() {
    // Linux ≥ 4.0; without it, the peak is the process's so far.
    let _ = fs::write("/proc/self/clear_refs", "5");
}

/// Writes `files` small files under `root`, `PER_DIR` per directory, on a
/// few threads.
fn populate(root: &Path, files: usize) {
    let dirs = files.div_ceil(PER_DIR);
    let threads = 8;
    std::thread::scope(|s| {
        for t in 0..threads {
            s.spawn(move || {
                for d in (t..dirs).step_by(threads) {
                    let dir = root.join(format!("d{d:05}"));
                    fs::create_dir(&dir).unwrap();
                    for f in 0..PER_DIR.min(files - d * PER_DIR) {
                        fs::write(dir.join(format!("f{f:04}")), format!("{d}/{f}\n")).unwrap();
                    }
                }
            });
        }
    });
}

/// Runs one cycle, prints how long it took and its peak RSS.
fn phase(p: &mut Pair, name: &str, scope: Scope) -> SyncReport {
    measure(p, name, |p| {
        let r = p
            .engine
            .sync(&mut p.a.replica, &mut p.b.replica, scope)
            .unwrap();
        assert!(r.is_converged(), "{name}: {r:?}");
        p.a.replica.sweep_quarantine();
        p.b.replica.sweep_quarantine();
        r
    })
}

/// Runs `f`, prints how long it took, its peak RSS and heap, and what it
/// applied.
fn measure(p: &mut Pair, name: &str, f: impl FnOnce(&mut Pair) -> SyncReport) -> SyncReport {
    reset_peak();
    let before = status_kib("VmRSS");
    let heap = HEAP.reset();
    let traffic = p.traffic();
    let start = Instant::now();
    let r = f(p);
    let took = start.elapsed();
    let peak = status_kib("VmHWM");
    let heap_peak = HEAP.peak.load(Ordering::Relaxed) - heap;
    let moved = p.traffic() - traffic;
    eprintln!(
        "  {name:<18} {:>9.3} s   RSS {:>6} -> peak {:>6} MiB   heap +{:>5} MiB   applied {:>7}{}",
        took.as_secs_f64(),
        before / 1024,
        peak / 1024,
        heap_peak >> 20,
        r.applied,
        if p.mode() == Mode::Remote {
            format!("   {:>9} KiB on the wire", moved / 1024)
        } else {
            String::new()
        },
    );
    r
}

fn scale(mode: Mode) {
    static ONE_AT_A_TIME: Mutex<()> = Mutex::new(());
    let _turn = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let files = env("FFSL_SCALE_FILES", 200_000);
    let big_mib = env("FFSL_SCALE_BIG_MIB", 256);

    let mut p = Pair::new(SymlinkPolicy::Links).over(mode);
    let start = Instant::now();
    populate(p.a.root(), files);
    p.a.write("big", vec![7u8; big_mib << 20]);
    eprintln!(
        "{mode:?}: {files} files + {big_mib} MiB written in {:.1} s",
        start.elapsed().as_secs_f64()
    );

    let r = phase(&mut p, "initial", Scope::Full);
    assert_eq!(r.applied, files + files.div_ceil(PER_DIR) + 1);
    let r = phase(&mut p, "no-op full", Scope::Full);
    assert_eq!(r.applied, 0);
    let r = phase(
        &mut p,
        "no-op scoped",
        Scope::Paths(vec![rp("d00000/f0000")]),
    );
    assert_eq!(r.applied, 0);

    p.a.write("d00000/f0001", "changed, scoped\n");
    let r = phase(
        &mut p,
        "one file, scoped",
        Scope::Paths(vec![rp("d00000/f0001")]),
    );
    assert_eq!(r.applied, 1);
    p.a.write("d00000/f0002", "changed, full\n");
    let r = phase(&mut p, "one file, full", Scope::Full);
    assert_eq!(r.applied, 1);

    let f = fs::File::options()
        .write(true)
        .open(p.a.path("big"))
        .unwrap();
    f.set_modified(SystemTime::now() - Duration::from_secs(3600))
        .unwrap();
    let r = phase(&mut p, "touch big, scoped", Scope::Paths(vec![rp("big")]));
    assert_eq!(r.applied, 1);

    measure(&mut p, "scan A, full", |p| {
        let stats = p.a.replica.scan(Scope::Full).unwrap();
        assert_eq!(stats.changed, 0, "{stats:?}");
        SyncReport::default()
    });
}

both_modes! {
    #[ignore]
    scale,
}
