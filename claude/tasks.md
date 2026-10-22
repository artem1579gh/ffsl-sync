# ffsl-sync: tasks

The design lives in [`design.md`](design.md); §N.M below refers to its sections.

## How to work on a task (read this first)

1. Pick the **first unchecked task** whose dependencies are all checked.
2. Read only the design sections listed in that task's **Read** line. Then read the existing code it touches.
3. Keep the scope tight: do this task only. If you need a small stub from a later task, add a stub with a `todo!()` and say so in Notes.
4. Make the **Done when** criteria pass, plus `cargo build`, `cargo test` and `cargo clippy --all-targets -- -D warnings`.
5. Tick the box. Fill in **Notes** with deviations from the design, open issues and follow-ups. If you deviated from the design, update `design.md` too.
6. Commit with the message `T<NN>: <title>`.

---

## M0: foundations

### [x] T01: Scaffolding
- **Depends on:** —
- **Read:** §1, §2
- **Files:** `Cargo.toml`, `src/lib.rs`, `src/main.rs`, `src/cli.rs`, `src/config.rs`, `src/error.rs`, empty `mod.rs` stubs for every module in §2
- **Do:**
  - Add the §2 dependencies that are needed now: rustix, libc, blake3, redb, serde, postcard, toml, clap, crossbeam-channel, tracing, tracing-subscriber, thiserror, anyhow, jiff. Dev dependencies: tempfile, proptest. Leave out landlock and rustls for now.
  - Add a lib + bin layout, with `main.rs` calling `ffsl_sync::cli::run()`.
  - Add a clap CLI with these subcommands: `init <pair> --a <dir> --b <dir>`, `sync --once <pair>`, `daemon <pair>`, `status <pair>`. They are stubs that print "not implemented".
  - `config.rs`:
    - `PairConfig { name, replicas: [ReplicaConfig; 2] }`;
    - `ReplicaConfig { id: ReplicaId(u64), root: PathBuf, symlinks: SymlinkPolicy, munge_links: bool, keep_dirlinks: bool }`;
    - a placeholder `SymlinkPolicy` enum: `Skip | Links | CopyLinks | CopyUnsafeLinks | SafeLinks | CopyDirlinks`;
    - TOML load and save under `$XDG_STATE_HOME/ffsl-sync/<pair>/config.toml`;
    - `init` generates random replica IDs.
  - Add an `error.rs` `Error` enum with `thiserror`.
- **Done when:** `cargo run -- --help` lists the subcommands, `init` writes a config that loads back, and clippy is clean.
- **Notes:**
  - Deps as listed; `rustix` has features `fs` + `rand` (`rand` is used for `getrandom`, so no `rand` crate is needed). `postcard` uses `use-std`, `tracing-subscriber` uses `env-filter`. Edition 2024.
  - Module stubs: `fs`, `index`, `symlink`, `scan`, `watch`, `replica`, `engine` (`mod.rs` holding only a doc comment) plus `src/daemon.rs`. Submodule files (`root.rs` etc.) are left to their own tasks.
  - `cli::run()` returns `anyhow::Result` because `cli.rs` is the binary's front end; the rest of the library uses `crate::Error`. Stub subcommands exit 1 with "Error: `<cmd>`: not implemented". `sync` without `--once` is rejected and points to `daemon`. Logging goes to stderr via `RUST_LOG` (default `info`).
  - Config: `ReplicaId` is serialized as a 16-digit hex **string** in TOML, because TOML integers are i64 and random u64 IDs would not round-trip. `ReplicaId` itself is `serde(transparent)` u64, so postcard/index stay compact. `Display` is also 16-digit hex, so ID7 = the first 7 chars.
  - `SymlinkPolicy` is serialized in kebab-case (`links`, `copy-unsafe-links`, …) and defaults to `Links` (rsync `-a`). `symlinks`, `munge_links` and `keep_dirlinks` are optional in the file. `deny_unknown_fields` is on.
  - The config API takes `state_home` explicitly (`PairConfig::load/create/save`, `config::init`); `config::state_home()` reads `$XDG_STATE_HOME`, else `$HOME/.local/state` (relative values are ignored, per the XDG spec). This keeps tests independent of the environment.
  - `config::init` canonicalises both roots and rejects: a missing root or a non-directory, overlapping or nested roots, a state dir inside a root, an existing pair (installed via `link(2)`, so no overwrite even under a race), and non-UTF-8 root paths (TOML can't store them; follow-up if needed). Pair names are restricted to `[A-Za-z0-9._-]`, ≤64 bytes, no leading `.`.
  - Config writes are atomic (temp file + fsync + rename/link + dir fsync), with dir mode 0700 and file mode 0600. These use `std::fs`, which is fine because the state dir is never inside a replica.
  - Tests: 8 unit tests in `config.rs`, plus `tests/cli.rs` (help lists the subcommands, `init` round-trips through the real binary, stubs report "not implemented").

### [x] T02: Filesystem feature checks (`fs/caps.rs`)
- **Depends on:** T01
- **Read:** §1 (environment caveat), §5.3 step 2, §5.3 step 4(b)
- **Files:** `src/fs/caps.rs`
- **Do:** implement `Caps::probe(root_dirfd) -> Caps { openat2, rename_exchange, rename_noreplace, o_tmpfile, linkat_empty_path, statx_btime, leases, fs_type }`. Each check is a real, harmless operation inside the root using `.~ffsl.probe.*` names, cleaned up afterwards. Add a `require_minimum()` check that fails clearly when openat2 or the renameat2 flags are missing.
- **Done when:** a test on a tempdir in `/tmp` reports everything supported (except leases, which depend on the environment). The result is logged at startup.
- **Notes:**
  - Extra field `linkat_proc_fd` (the `/proc/self/fd/N` + `AT_SYMLINK_FOLLOW` fallback from §5.3 step 2), plus `Caps::tmpfile_usable()` = `o_tmpfile && (linkat_empty_path || linkat_proc_fd)`. T04 should use these to choose between O_TMPFILE and a named temp file. Unprivileged `AT_EMPTY_PATH` only works on kernels ≥ 6.10; on older kernels it reports false (ENOENT).
  - `fs_type` is an `FsType` enum built from the `statfs` magic (ext2/3/4, xfs, btrfs, tmpfs, 9p, fuse, nfs, overlay, `Other(magic)`). `Caps::log` warns when the type is not one of the four supported filesystems; `require_minimum` checks only features, not the type.
  - `openat2` counts as supported only if `..` really fails with EXDEV under `RESOLVE_BENEATH`. `rename_noreplace` must give EEXIST on an existing name and succeed on a free one. `rename_exchange` must actually swap the inodes. `leases` takes, checks (`F_GETLEASE`) and releases an `F_WRLCK` lease; any failure counts as false.
  - Errnos that mean "unsupported" give `false`. Anything else (e.g. EACCES/EROFS on the root) is an `Err`, since such a root couldn't be synced anyway.
  - Cleanup safety: probe names are `.~ffsl.probe.<random tag>.<suffix>`. Every probe inode stays pinned by an fd until cleanup, so its inode number can't be reused; `O_PATH` pins don't block write leases. Cleanup unlinks a name only if it still holds one of our (dev, ino); anything else is left and logged. There is a residual statx→unlink window on a random reserved name, which is accepted. Test: `cleanup_leaves_foreign_files_alone`.
  - Mutating outside `commit.rs`: `caps.rs` creates and removes its own probe files. CLAUDE.md anticipates this (the "probe files" in the reserved prefix), and design §2 now says so.
  - `Error::MissingCapabilities { fs_type, missing }` was added. Until `Root` (T03) exists, `Caps::probe_path` opens the root directly with `O_PATH|O_DIRECTORY`; T03/T11 can switch to `Root`.
  - CLI: `init` probes both roots and refuses one missing the required features. `sync --once` and `daemon` load the config, probe and log both roots ("filesystem capabilities" at INFO), then say "not implemented". `status` is still a plain stub. In T11, `LocalReplica::open` should own the probe.
  - Measured on this WSL2 machine (kernel 6.18): ext4 `/tmp` and tmpfs `/dev/shm` report every capability as true, leases included. The drvfs path is unit-tested only through forced flags, because tests must not touch `/mnt/c`.

## M1: race-safe filesystem primitives

### [x] T03: `Root`, `RelPath`, fingerprints
- **Depends on:** T01
- **Read:** §2, §3 (paths), §5.1, §5.2
- **Files:** `src/fs/root.rs`, `src/fs/stat.rs`, `src/fs/mod.rs`
- **Do:**
  - `RelPath`: raw bytes, validated; components are non-empty and contain no `/`, `.` or `..`. Provide `parent()`, `name()`, `depth()` and `join()`.
  - `Root::open(path)`: an `O_PATH|O_DIRECTORY` fd.
  - `Root::resolve_parent(&RelPath) -> Result<(OwnedFd, &[u8] name)>` using openat2 with `RESOLVE_BENEATH|NO_SYMLINKS|NO_MAGICLINKS|NO_XDEV`. ELOOP and EXDEV map to `Error::Unstable`.
  - `Root::read_dir(&RelPath)`: fd-based, returns names and d_type.
  - `Fingerprint { dev, ino, size, mtime_ns, ctime_ns, mode, kind }` from `statx` (fd or `AT_SYMLINK_NOFOLLOW`).
  - `stable_read(parentfd, name, sink) -> Result<(Fingerprint, [u8;32])>` implementing §5.2: hash while reading, compare the before and after fingerprints, retry three times, then return `Unstable`.
- **Done when:** unit tests show:
  - a symlinked intermediate directory gives `Unstable`;
  - `..` and absolute paths are rejected by `RelPath`;
  - a file appended to during `stable_read` (from a hook or a thread) gives `Unstable`;
  - the normal case returns the correct blake3 hash.
- **Notes:**
  - `RelPath` (in `fs/root.rs`, re-exported as `fs::RelPath`) wraps `Vec<u8>`; the empty path is the root (`RelPath::root()`, depth 0, `parent()`/`name()` are `None`). Rejected: empty components (so absolute paths, `//` and a trailing `/`), `.`, `..` and NUL bytes. `join()` takes one or more components, validated the same way. Serde goes through `try_from = "Vec<u8>"`, so deserialised paths are validated too. `Display` uses `escape_ascii` (root shows as `.`). Reserved `.~ffsl.` names are *not* rejected; that is the scanner's job (T04 adds `is_reserved`).
  - `Root { fd, path }`: `open`, `fd`, `resolve_parent`, plus `open_dir` (an `O_RDONLY` fd for the directory itself, root included), `read_dir` (sorted, without `.`/`..`, reserved names included; `kind: Option<FileKind>`, `None` for `DT_UNKNOWN`) and `stat` (a `Fingerprint` of the path itself). For a depth-1 path, `resolve_parent` returns a dup of the root fd. `Caps::probe_path` now uses `Root::open`.
  - **Deviation (design §5.1 updated):** directory opens use `O_PATH|O_DIRECTORY` without `O_NOFOLLOW`. With it, a trailing symlink in the parent path is opened as the link itself and fails with ENOTDIR instead of ELOOP. `RESOLVE_NO_SYMLINKS` alone rejects every symlink with ELOOP. The file open in `stable_read` keeps `O_NOFOLLOW` (no `O_DIRECTORY`, so it gives ELOOP).
  - Errors: new `Error::InvalidPath { path, reason }` and `Error::Unstable { path: Vec<u8>, reason: &'static str }`. ELOOP, EXDEV and EAGAIN (openat2's "possible concurrent rename") map to `Unstable`. ENOENT and ENOTDIR stay `Error::Io`, so callers can tell "gone" from "changed". In `stable_read`, `Unstable.path` is the single `name`; callers holding the `RelPath` should rewrap it.
  - `Fingerprint { dev (makedev), ino, size, mtime_ns, ctime_ns, mode (& 0o7777), kind: FileKind { File, Dir, Symlink, Special } }`, with `of_fd`, `at` (`AT_SYMLINK_NOFOLLOW`), `same_file` (dev+ino) and `unchanged` (same_file + kind, size, mtime, ctime: the §5.2 check). A statx result missing any needed field is an `Io` error (Unsupported). `mnt_id` from §3 `LocalMeta` is not in the fingerprint; T10 can add it if needed.
  - `stable_read(parent, name, &mut impl Sink)`. `Sink` has `restart()` (called before each retry, so a sink can drop a torn attempt) and `write()`; `Vec<u8>` and `Discard` implement it. There are 1 + 3 attempts with 5/10/20 ms backoff. Unstable when: the name is a symlink or not a regular file; F1≠F2 (`unchanged`); more bytes are read than F1.size (stops early on a growing file); the byte count ≠ size; the name is removed or replaced (F3); or the file is leased (EAGAIN). The open uses `O_NONBLOCK`, so a FIFO or a foreign lease can't block us; FIFOs then fail the regular-file check. `O_NOATIME` falls back on EPERM. The returned fingerprint is F2.
  - Tests (16 new): RelPath validation, parts, serde; resolve_parent (normal, symlinked intermediate inside and outside the tree → `Unstable`, missing/ENOTDIR → `Io`); read_dir types incl. non-UTF-8 names; stable_read normal multi-chunk + empty (blake3 checked), append on every attempt → `Unstable` after exactly 4 attempts, append on the first attempt only → success with the new content, a concurrent appending thread → `Unstable`, name replaced/removed mid-read, symlink/dir/FIFO → `Unstable`. Hooks don't exist yet (T04), so the "hook" is a `Sink` that mutates the file on its first write of each attempt. T04 could add a `hooks::point("stable_read.after_read")`.
  - For T04: `resolve_parent` returns an `O_PATH` fd, which works as a dirfd for every `*at` call and for `O_TMPFILE`, but `fsync(parentfd)` (§5.3 step 5) needs a separate `O_RDONLY` open of the directory (`Root::open_dir`, or `openat2(parentfd, ".", O_RDONLY|O_DIRECTORY)`).

### [x] T04: Hooks, temp files, create-type commits
- **Depends on:** T03, T02
- **Read:** §5.3 (steps 1–3 and 5), §5.4, §5.5, §9 (race injection)
- **Files:** `src/fs/hooks.rs`, `src/fs/tmpname.rs`, `src/fs/commit.rs`
- **Do:**
  - `hooks::point(&'static str)` does nothing unless `cfg(any(test, feature = "hooks"))`. Tests use it to register closures per point. Add a `hooks` feature to Cargo.toml.
  - `tmpname`: `.~ffsl.<id>`, `.~ffsl.old.<id>`, `.~ffsl.del.<id>`, and `is_reserved(name)`.
  - `TempFile::create(parentfd, caps)`: O_TMPFILE, falling back to a named temp file. Then `write_all` with hashing, `finish(mode, mtime)` (fchmod, futimens, fsync), then link in. The fd is kept open.
  - `commit::create_file(root, relpath, content, meta, expected_hash) -> Outcome` using `RENAME_NOREPLACE`. Reject on hash mismatch.
  - `commit::create_symlink(root, relpath, target)` via a temp name and NOREPLACE.
  - `commit::mkdir(root, relpath, mode)`.
  - Post-commit verification (§5.3 step 5, without the index or journal for now). `Outcome` here is a local enum: `Applied(Fingerprint) | PreconditionFailed`.
  - Hook points at every step boundary.
- **Done when:**
  - Unit tests: create on an empty name succeeds; create on an existing name gives `PreconditionFailed` and leaves no temp file behind; symlink and mkdir behave the same way.
  - A hook-injected concurrent create (just before the rename) results in `PreconditionFailed` with the user's file untouched.
- **Notes:**
  - **Hooks** (`fs/hooks.rs`, feature `hooks`): `point(name)` is an `#[inline(always)]` no-op unless `cfg(any(test, feature = "hooks"))`. The registry is **thread-local**, so parallel tests don't see each other's hooks; commit code runs on the caller's thread. API: `on(name, FnMut) -> Guard` (removed on drop), `once(name, FnOnce)`, `start_trace()`/`take_trace()` (the ordered list of points reached, for T07/T15 to enumerate every point per op). Closures are taken out while running, so they may re-enter `point`.
  - **Hook points** (shared names across ops; use the trace to see which an op hits): `commit.resolved`; `stage.created`, `stage.written`, `stage.synced`, `stage.linked`; `create.before_rename`, `create.after_rename`; `commit.synced`, `commit.verified`. Files hit all `stage.*`; symlinks and dirs only `stage.linked`.
  - **tmpname**: `.~ffsl.<16 hex>`, `.~ffsl.old.<id>`, `.~ffsl.del.<id>`, plus `is_reserved`, `parse` (for journal replay) and `TmpId::random`. `conflict_name` lives here too (see T05).
  - **API** (`fs/commit.rs`): functions take `&Ctx { root, caps, replica }` instead of a bare root, since they need the caps (temp-file strategy) and, from T05, the replica ID for conflict names. `create_file(ctx, path, &mut dyn Read, FileMeta { mode, mtime_ns }, &expected_hash)`, `create_symlink(ctx, path, target)`, `mkdir(ctx, path, mode)`. `Outcome::PreconditionFailed(&'static str)` carries a reason for logs/tests.
  - **TempFile**: O_TMPFILE when `Caps::tmpfile_usable()`, linked with `AT_EMPTY_PATH` or `/proc/self/fd` per caps; else `O_CREAT|O_EXCL` under a temp name. `finish(meta)` = fchmod (setuid/setgid stripped: `& 0o1777`), futimens (atime `UTIME_OMIT`), fsync, link in, and returns a `Staged` that keeps the fd (pins N). Dropping a `TempFile` or a live `Staged` removes the temp name only if it still holds our inode (and, for `Staged`, the same size/mtime).
  - Hash mismatch → `PreconditionFailed("content hash mismatch")`, nothing linked. A cheap "name exists" precheck runs before staging so we don't write content for a create that can't succeed; `RENAME_NOREPLACE` is still the binding check.
  - **Deviation (design §5.4 updated):** `mkdir` stages a `.~ffsl.<id>` dir (0700), pins it, fchmods it to the exact mode (no umask), then renames with NOREPLACE; same step 5 checks.
  - **Step 5:** fsync the parent (separate `O_RDONLY` open of `.`), name must hold N (ino+kind, plus size+mtime for non-dirs), parent must re-resolve to the same (dev, ino). A failure here means the commit happened but something changed right after: `Err(Unstable)`, so the caller rescans (design §5.3 step 5 updated). Reserved target names and the root are refused with `InvalidPath`.
  - Tests (11 in commit.rs + 3 hooks + 4 tmpname): create file (O_TMPFILE and named fallback), mode/mtime incl. negative mtime and 300 KB content, existing name, hash mismatch, hook-injected concurrent create before the rename (file, symlink, dir-with-child) → `PreconditionFailed`, user object untouched, no leftovers; exact hook trace; symlinked ancestor → `Unstable` with nothing written outside; ancestor swapped for an outside symlink after resolution → write lands in the pinned (moved) dir, outside dir empty, `Unstable`; name replaced right after the rename → `Unstable`.

### [x] T05: CAS replace via RENAME_EXCHANGE
- **Depends on:** T04
- **Read:** §5.3 step 4, §5.10
- **Files:** `src/fs/commit.rs`
- **Do:**
  - `commit::replace_file(root, relpath, expected: &Fingerprint, content, meta)` and `replace_symlink(...)` implementing (a) pin and check, (c) exchange, (d) verify, (e) undo or conflict copy, and (f) quarantine with a grace timer. Leases come in T18; leave a hook for them.
  - `Outcome` gains `Preserved { conflict: RelPath }`.
  - Add a `Quarantine` struct listing pending `.old.` files with deadlines, and `Quarantine::sweep()` to process them.
  - Conflict-copy naming helper: `conflict_name(name, now, replica_id)` (§6.2). Put it in `engine/conflict.rs`, or in `fs/` if that is simpler, and note where.
- **Done when:** these tests pass:
  - normal replace;
  - expected fingerprint mismatch → `PreconditionFailed`, file unchanged;
  - hook writes to the file between (a) and (c) → user content survives at `name` (after the undo);
  - hook replaces the file with a new inode between (a) and (c) → user content survives;
  - hook holds an fd, and the test writes through it after (c) → the quarantine sweep produces a conflict copy containing that write.
- **Notes:**
  - `replace_file(ctx, &mut Quarantine, path, &Expected, content, meta, &expected_hash)` and `replace_symlink(ctx, &mut Quarantine, path, &Expected, target)`. `Expected { fp, hash: Option<[u8;32]>, racy }` (`From<Fingerprint>`): `hash` enables the (d) rehash, done when `racy` or size < 1 MiB (`REHASH_BELOW`). T11 should fill it from the index entry.
  - The old object may be a file or a symlink for either function (file↔symlink type change in one exchange; design §5.5 updated); a directory gives `PreconditionFailed("not a file or symlink")`.
  - Flow: precheck (cheap), stage, (a) pin with `O_PATH|O_NOFOLLOW` + `Fingerprint::unchanged` against `expected.fp`, (b) placeholder comment for the T18 lease, (c) `RENAME_EXCHANGE`, (d) `same_object` (ino, kind, size, mtime; no ctime) + optional rehash via `stable_read`, (f) rename to `.~ffsl.old.<id>` and add to the quarantine, then step 5. ENOENT on the exchange (name deleted after the pin) → `PreconditionFailed("name removed")`. Errors during (d) count as "changed", so they trigger the undo rather than leaving the user's file under a temp name.
  - (e) undo: exchange back; if the object coming out is N unchanged, unlink it → `PreconditionFailed("changed during commit")`; if not, it is user data too → renamed to a conflict name → `Preserved { conflict }`. If the name was deleted after (c) (ENOENT on the second exchange), the user's object is renamed back with NOREPLACE (conflict name on EEXIST).
  - **Quarantine**: `Quarantine::new(replica, grace)` (`DEFAULT_GRACE` = 400 ms = 2 × debounce), `len`, `is_empty`, `next_deadline`, `sweep() -> SweepReport { removed, conflicts, dropped }`. Each entry holds an `O_PATH` fd for the parent dir (found even if the dir is renamed) and one pinning the old inode (so its ino can't be reused). Sweep: changed size/mtime → conflict copy right away; unchanged and past the deadline → unlink; name gone or holding a foreign inode → forgotten and left alone. Per-entry errors are logged and retried next sweep. Not persisted: T15's journal must cover `.old.` files across restarts.
  - Residual: the stat → unlink window in the sweep (a write through a still-open fd in that instant is lost). T18's lease check closes it; already listed in design §5.10.
  - **Conflict names**: `fs::tmpname::conflict_name(name, split_ext, now: civil::DateTime, replica)` (in `fs/` because `fs::commit` needs it and `fs` must not depend on `engine`; T13's `engine/conflict.rs` should reuse it). Extension = after the last `.` that is neither first nor last; names are shortened to fit 255 bytes. On EEXIST the timestamp is bumped by 1 s (up to 16 tries). Local time from `jiff::Zoned::now()`.
  - New hook points: `replace.before_pin`, `replace.pinned`, `replace.before_exchange`, `replace.after_exchange`, `replace.before_rehash`, `replace.verified`, `replace.quarantined`, `replace.before_undo`, `replace.after_undo`, `quarantine.before_unlink`, `quarantine.before_conflict`.
  - Tests (14): normal replace (both temp strategies) + sweep; exact hook trace; stale expectation and missing name; change between precheck and (a); append between (a) and (c) → undone, same inode back at `name`; same-size write with mtime restored → caught only by the rehash; new inode (and a directory) swapped in between (a) and (c) → user object back at `name`; name deleted before (c); fd held from before (c) and written after → sweep makes `f.sync-conflict-…-abcdef0.txt` with the late write, and the fd keeps writing into it; grace period respected and a foreign object at the `.old.` name left alone; both objects modified around (c) → `Preserved`; name deleted after (c) → user object restored; symlink replace normal/stale/raced, file↔symlink; directory refused.

### [x] T06: CAS delete and rmdir
- **Depends on:** T05
- **Read:** §5.6, §5.7
- **Files:** `src/fs/commit.rs`
- **Do:** `commit::delete(root, relpath, expected)` for files and symlinks: pin, rename to `.del.`, verify, then unlink or quarantine, with restore and conflict fallbacks. `commit::rmdir(root, relpath)`: ENOTEMPTY → `PreconditionFailed`.
- **Done when:** tests cover:
  - a normal delete;
  - a modification just before the rename (hook) → the file is restored at `name`;
  - the name re-created by the user after our rename → the restored file goes to a conflict name and both survive;
  - rmdir with a child created by a hook just before → `PreconditionFailed`, child intact.
- **Notes:**
  - **API:** `delete(ctx, &mut Quarantine, path, &Expected)` (file or symlink; a directory gives `PreconditionFailed("not a file or symlink")`) and `rmdir(ctx, &mut Quarantine, path)`. New `Outcome::Removed` for a successful delete/rmdir (`Applied(Fingerprint)` has nothing to carry); T11 maps it to `Applied(tombstone)`.
  - **Delete flow:** same precheck as replace, then (1) pin + check (`pin_expected`, now shared with replace), (2) `renameat2(name → .~ffsl.del.<id>, NOREPLACE)` (ENOENT → `PreconditionFailed("name removed")`), (3) `verify_old` (shared with replace; it takes the rehash hook point as an argument), (4) quarantine (renamed on to `.~ffsl.old.<id>`) instead of unlinking, so a write through a held fd becomes a conflict copy. (5) On a failed verify, `restore` (shared with replace's undo) moves it back with NOREPLACE; on EEXIST it keeps it under a conflict name → `Preserved`. Step 5: fsync the parent, the name must be **free**, and the parent must re-resolve to the same (dev, ino); otherwise `Err(Unstable)`.
  - **rmdir:** checks the name is a directory, **settles** the quarantine entries of that directory (`Quarantine::settle_dir`: unchanged → unlink now, written since → conflict copy in the dir), then `unlinkat(AT_REMOVEDIR)`. ENOTEMPTY/EEXIST → `PreconditionFailed("directory not empty")`; ENOTDIR (replaced by a file/symlink) → `"not a directory"`; ENOENT → `"name removed"`. Without settling, deleting children and then their directory (§5.7) would always fail on the quarantined `.old.` files. Design §5.6, §5.7 and §5.10 updated.
  - `Pending` now records its directory's identity (`dir_fp`); `Quarantine::sweep` and `settle_dir` share `process()`.
  - Residuals (design §5.7, §5.10): rmdir has no fingerprint precondition, so an empty directory swapped for another empty one is removed (no data lost); settling cuts the grace period short for children of a removed directory.
  - New hook points: `delete.before_pin`, `delete.pinned`, `delete.before_rename`, `delete.after_rename`, `delete.before_rehash`, `delete.verified`, `delete.quarantined`, `delete.before_restore`; `rmdir.checked`, `rmdir.before_rmdir`, `rmdir.after_rmdir`. T18 lease placeholders are marked at `delete.before_rename`.
  - Tests (11): normal delete of a file and a symlink with the exact trace; stale/missing/directory; a modification before the rename → restored at `name` (same inode); a same-size, mtime-preserving write → caught by the rehash and restored; a new inode (a directory with a child) before the rename → restored; a modification before the rename plus the name re-created after it → `Preserved`, conflict copy `f.sync-conflict-…-abcdef0.txt` and the new file both survive; name re-created after a valid delete → `Unstable`; a write through a held fd after the delete → conflict copy on sweep; rmdir normal (trace), file, symlink-to-dir, missing, non-empty; a child created by a hook just before the rmdir → `PreconditionFailed`, child intact; a directory replaced by a file just before → file untouched; rmdir settling quarantined children (grace 1 h): a late-written child becomes a conflict copy and keeps the directory, while unchanged children are unlinked, entries in other directories wait, and the rmdir succeeds.

### [x] T07: Race-injection attack suite
- **Depends on:** T06
- **Read:** §5.1, §9
- **Files:** `tests/attack.rs` (integration; needs the `hooks` feature: `cargo test --features hooks`)
- **Do:**
  - For **every** hook point in commit.rs, and for each of create, replace, delete, mkdir and symlink:
    - replace an ancestor directory with a symlink to an outside tempdir;
    - move an ancestor directory out of the root;
    - swap the target for a symlink.
  - After each case, assert:
    - nothing was created or changed in the outside dir (snapshot it before and after);
    - every piece of user content written by the hook still exists somewhere in the root (at its name or a conflict copy);
    - no `.~ffsl.*` leftovers remain after `Quarantine::sweep` with a zero grace period.
- **Done when:** the whole suite passes. Record any accepted residual (§5.1, a directory moved out) in Notes.
- **Notes:**
  - `tests/attack.rs`, gated with `[[test]] required-features = ["hooks"]` in Cargo.toml, so a plain `cargo test` skips it. Run it with `cargo test --features hooks --test attack` (~3.5 s).
  - **Operations:** create file, create symlink, mkdir, replace file, replace symlink, delete file, delete symlink, rmdir, and delete-then-rmdir (delete `a/b/t/c`, then rmdir `a/b/t`, which covers the quarantine settle). The target is always `a/b/t`. File-staging ops run under both temp strategies (O_TMPFILE and named).
  - **Variants** reach the undo, restore and conflict paths: `EditBefore` (the user rewrites the file through a held fd at `replace.before_exchange` / `delete.before_rename`) and `LateWrite` (through the held fd at `*.quarantined`, so the sweep or the rmdir settle makes a conflict copy).
  - **Enumeration:** each op × variant × caps runs once clean with `hooks::start_trace`. Then, for **every position** in the trace (the nth hit of a point, so the two `commit.*` passes of delete-then-rmdir are both attacked; the trace includes the quarantine sweep), each of the 6 attacks is injected on a fresh tree: ancestor `a` or `a/b` → symlink to the outside dir; `a` or `a/b` moved out of the root (into a separate "away" tempdir); the target moved aside within the root and replaced by a symlink to an outside file or directory. 1242 cases. Each attack first writes a unique marker file in `a/b`. The outside dir holds decoys at every path a followed link would reach (`t`, `b/t`, `victim`, `vdir/`).
  - **Checks per case:** the outside dir snapshot (kind, mode, size, mtime, ctime and content of every entry, the dir itself included) is unchanged; every tracked piece of user content (markers, the attacker's symlink, held-fd writes) exists as a file or symlink somewhere in the root; an `Applied`/`Removed` outcome holds right after the call (skipped only when the attack fired at that call's `commit.verified`); no `.~ffsl.*` names remain in the root or the away dir after a final zero-grace `Quarantine::sweep`; the injection actually fired.
  - **Coverage test** `every_hook_point_is_attacked`: it parses every hook point name out of `src/fs/commit.rs` (outside its tests) and requires the clean traces to reach all of them (and nothing else). It also checks that the clean runs do what they should (success, `PreconditionFailed` for `EditBefore`, a resurrected dir for a late write before rmdir) with nothing lost and no leftovers. New hook points therefore fail this test until a scenario reaches them.
  - **Mutation check** (done by hand, not committed): disabling the parent re-resolution in step 5 fails 5 suites through the outcome check; making `restore` overwrite (no NOREPLACE) fails delete through the lost-content check.
  - **Accepted residual (§5.1, directory moved out):** when `a` or `a/b` is moved out of the root mid-commit, the commit completes inside the moved directory, through the pinned parent fd. The new file/dir/symlink, a restored file, or a conflict copy can then end up in the away dir, and our own temp and `.old.` names there are cleaned up through the same fds. The op reports `Err(Unstable)` (or `PreconditionFailed`), never `Applied`. Nothing is ever created in a directory that was never inside the root, and the "user content survives" check counts the away dir only for this attack. No other residuals were found; the suite passed without changes to `commit.rs`.

## M2: index, symlinks, scanner

### [x] T08: Index entries, version vectors, redb store
- **Depends on:** T01
- **Read:** §3, §6.1
- **Files:** `src/index/{mod,entry,vv,store}.rs`
- **Do:**
  - `Entry`, `Kind`, `UnmanagedReason` and `LocalMeta` exactly as in §3, with serde derives. On the wire, `LocalMeta` is `#[serde(skip)]`, or use a separate wire type.
  - `VersionVector`:
    - `bump(id)` (max + 1);
    - `merge`;
    - `compare -> Ord4 { Equal, Dominates, Dominated, Concurrent }`.
  - `IndexStore` (redb). Tables:
    - `entries: &[u8] -> Entry`;
    - `by_seq: u64 -> &[u8]`;
    - `meta` (replica id, next_seq, max counter).
    - API: `get`, `put` (assigns seq), `changes_since(seq)`, `iter_prefix`, and transactions.
- **Done when:**
  - proptest: version-vector compare is reflexive, antisymmetric and transitive (a partial order); merge is commutative, associative and idempotent, and dominates both inputs.
  - Store round-trip tests pass.
  - `changes_since` returns every entry put after the given seq.
- **Notes:**
  - **Entry** (`index/entry.rs`): `Entry`, `Kind`, `UnmanagedReason`, `LocalMeta` as in §3. `Entry`'s serde form is the **wire** form (`local` is `#[serde(skip)]`, tested: the raw target bytes don't appear in the encoding); the store persists `(Entry, LocalMeta)`. `LinkInfo { ino, ctime_ns, raw_target, out_of_tree }` was left open by the design (T10/T17 may extend it). Also `Entry::new` (seq 0, default local), `is_live`/`is_tombstone`/`is_unmanaged`, and `sync_mode(st_mode) = & 0o1777` (same mask as `commit.rs`).
  - **VersionVector** (`index/vv.rs`): `SmallVec<[(ReplicaId, u64); 2]>` (new dep `smallvec` with `serde`), canonical (sorted, no zeros), so derived `==` agrees with `compare == Equal`; `PartialOrd` follows `compare`. API: `bump(id)` (max + 1), `bump_after(id, floor)` (above a Lamport floor too), `get`, `set`, `max_counter`, `iter`, `merge` (returns a new vector), `compare -> Ord4`, `descends`, `FromIterator`. Deserialisation rejects unsorted, duplicate, zero or > `MAX_COUNTER` (2^62) counters, so `bump` can't overflow and needs no error path.
  - **IndexStore** (`index/store.rs`): `open(path, replica)` creates or checks `meta` (schema 1, replica ID; mismatch → `Error::BadIndex`); `path_for(pair_dir, id)` = `<pair>/<id>.redb`. Values are stored as `&[u8]` and decoded by us, so a corrupt record is `BadIndex`, not a panic inside redb's `Value::from_bytes`. `by_seq` keeps exactly one row per entry (the old one is removed on overwrite). Transactions: `read() -> ReadTxn` (snapshot: `get`, `changes_since`, `iter_prefix`, `len`, `next_seq`, `max_counter`) and `write() -> WriteTxn` (`get` sees own puts, `put(&path, &mut Entry) -> seq` sets `entry.seq`, `remove` for T18's GC, `commit`; drop = abort). The single-op helpers on `IndexStore` each use one transaction.
  - `iter_prefix` is component-wise (`a` gives `a`, `a/…`, not `ab` or `a.txt`; root gives everything), via the key range `["a/", "a0")`. `changes_since` and `iter_prefix` return `Vec`s; fine for now, a streaming/callback form can come with T21 if large indexes need it.
  - `meta.max_counter` is the largest vv counter ever put (a Lamport clock), for `bump_after`. T10 should bump with `vv.bump_after(local_id, store.max_counter())` so a re-created path outranks anything seen before.
  - New errors: `Error::Db(redb::Error)` (every redb sub-error goes through `redb::Error`) and `Error::BadIndex { reason }`.
  - Tests (16): vv basics, compare cases, non-canonical serde rejected, proptests (partial order: reflexive, swap-consistent, antisymmetric, transitive; merge commutative, associative, idempotent, upper and least upper bound; bump strictly dominates; serde round trip); entry wire form; store round-trip of every kind with full `LocalMeta`, non-UTF-8 key and the root key, across reopen; overwrite reassigns seq; `changes_since` against a model over 50 puts with overwrites, at every cut; `iter_prefix`; transaction abort/snapshot isolation/remove; another replica's index rejected; corrupt record → `BadIndex`.

### [x] T09: Symlink safety, munging, policy
- **Depends on:** T01
- **Read:** §4 (all)
- **Files:** `src/symlink/{mod,safety,munge,policy}.rs`, `src/config.rs` (finalize `SymlinkPolicy`)
- **Do:**
  - `safety::is_unsafe(link_relpath, target: &[u8]) -> bool`, a faithful port of rsync `util1.c:unsafe_symlink()`. Copy its semantics, including how `.` and repeated `/` are handled.
  - `munge`: `munge(&[u8]) -> Vec<u8>` and `unmunge(&[u8]) -> Vec<u8>` with the prefix `/rsyncd-munged/` (§4.4).
  - `policy::classify(policy, link_relpath, target, referent_kind: Option<Kind>) -> Treatment { AsSymlink, Follow, Unmanaged(reason) }` covering every row of §4.3 except the `-K` adoption (T17).
- **Done when:**
  - a table test with at least 20 cases (absolute, `..` escapes at various depths, `a/../..`, `./x`, trailing slash, empty target);
  - proptest: `unmunge(munge(x)) == x` for all x, and `munge` is injective;
  - a classification table test for each policy × {safe, unsafe, dangling, dir referent}.
- **Notes:**
  - **Deviation (design §4.2 updated):** `safety::unsafe_symlink(dest, src)` ports rsync **3.4.1**, not the classic version. The classic algorithm (identical in upstream 3.2.7) disagreed with the installed `rsync 3.2.7` on 8 of 41 cases: Ubuntu backports the CVE-2024-12088 fix, which also rejects a `/../` after the leading `../` run (`a/../x`, `./../x`) and a trailing `/..` (`x/..`, even `../..` at depth 2). T17's differential test compares against the installed rsync, so we follow the current upstream. Quirks kept: `..` in `src` resets the margin, a leading `/` in `src` adds depth, repeated `/` are skipped, only exact `.`/`..` are special, input ends at the first NUL. `is_unsafe(&RelPath, target)` wraps it.
  - The 40-row table (plus raw-`src` quirks) was checked one by one against `rsync -a --safe-links` on this machine (scratch script, not committed; 45/45 agree including the policy-test targets). T17 can turn this into a permanent differential test.
  - **munge** (`symlink/munge.rs`): `munge`, `unmunge` (strips exactly one prefix; a target without it is returned unchanged), `is_munged`, `PREFIX`. Unlike rsync's sender (which unmunges only when `len > prefix len`), the exact-prefix target `/rsyncd-munged/` unmunges to the empty target, so `unmunge(munge(x)) == x` holds for all `x`; empty targets can't exist on Linux anyway. T17 should use `is_munged` to spot user-made unprefixed links in a munged replica.
  - **policy** (`symlink/policy.rs`): `classify(policy, link, canonical_target, referent: Option<FileKind>) -> Treatment`. **Deviation from the task text:** the referent is an `fs::FileKind` (what `stat` says), not an index `Kind`, because it describes the on-disk object, not an entry. `None` = dangling; `Some(Special)` under following → `Unmanaged(Special)`; `Some(Symlink)` (impossible after `stat`) is treated as dangling. Loops (ancestor dev/ino, `ELOOP`) are for the scanner (T10). `needs_referent(policy, link, target)` tells the scanner when it can skip the `stat`. Rows: Skip → `IgnoredLink`; Links → `AsSymlink`; CopyLinks → `Follow`/`Dangling`; CopyUnsafeLinks → unsafe as `-L`, safe as `-l`; SafeLinks → unsafe `IgnoredLink`, safe `AsSymlink`; CopyDirlinks → `Follow` only for a dir referent, else `AsSymlink` (dangling stays a link, as in rsync's `link_stat`). `-K` and munging are per-replica and not part of `classify`.
  - `config::SymlinkPolicy` is final: docs describe each policy's §4.3 behaviour, `Hash` was added, and it is re-exported as `symlink::SymlinkPolicy`.
  - Tests (7): safety table (40 rows) + raw-`src` quirks; munge examples + proptests (round trip, injective; inputs biased towards repeated and partial prefixes); policy matrix of 6 policies × {safe, unsafe} × {file, dir, dangling, special}, each with 3 target spellings, plus a check that the referent is ignored when `needs_referent` is false; depth-dependent safety.

### [x] T10: Scanner
- **Depends on:** T03, T08, T09
- **Read:** §3, §4.5, §5.2
- **Files:** `src/scan/{mod,scanner,hasher}.rs`
- **Do:**
  - `Scanner::scan(root, index, policy, scope: Full | Paths(Vec<RelPath>))`:
    - recursive, fd-based (open each child directory via openat2 from the parent fd);
    - never follow symlinks except as `classify` says, with following done as in §4.5;
    - skip reserved names;
    - track the (dev, ino) stack for loops;
    - rehash shortcut plus the racy flag;
    - on change, write the new Entry with `vv.bump(local_id)`;
    - paths missing from disk become a Tombstone (with a vv bump) unless already a tombstone;
    - an unstable read leaves the entry unchanged and reports the path as dirty.
  - Return `ScanStats { changed, dirty: Vec<RelPath>, ... }`.
- **Done when:** integration tests on a tempdir show:
  - a first scan indexes files, dirs and symlinks correctly under `Links` and `Skip`;
  - a modification bumps the vv once;
  - an untouched rescan rehashes nothing (count via a hasher counter) except racy entries;
  - a deletion produces a tombstone;
  - a loop link becomes `Unmanaged(Loop)` under `CopyLinks`;
  - a dangling link becomes `Unmanaged(Dangling)` under `CopyLinks`.
- **Notes:**
  - **API** (`scan/scanner.rs`): `Scanner::new(&Root, &IndexStore, SymlinkPolicy)` with builder options `.munge_links(bool)` and `.racy_window(Duration)`, then `.scan(&Scope) -> ScanStats`. This is a builder rather than the `scan(root, index, policy, scope)` form in the task text. The replica ID that gets bumped is `index.replica()`. `Scope::{Full, Paths(Vec<RelPath>)}`. `ScanStats { scanned, hashed, changed, tombstoned, dirty, errors }`: `changed` counts version-vector bumps (tombstones included), and `errors` lists paths skipped for non-race reasons (EACCES, a mount point). Only root and index failures abort the scan.
  - **Traversal:** depth-first. Each child directory is opened with `openat2(parentfd, name, O_RDONLY|O_DIRECTORY, RESOLVE)` and must be the same inode as its `statx`. Children are listed before recursing, so one fd stays open per level. Reserved names are skipped at every level. Every directory (real or followed) is checked against the (dev, ino) stack, so a loop reached through a followed parent's real subdirectory also becomes `Unmanaged(Loop)`.
  - **Following (design §4.5 updated):** the referent is opened beneath the root via the link's own root-relative path (`BENEATH|NO_MAGICLINKS|NO_XDEV`). On `EXDEV` it is opened from the link's dirfd with only `NO_MAGICLINKS` and marked `out_of_tree`. If the two routes disagree, the link is unstable. The first open is `O_PATH`, to get the referent's kind. Files are read through the new `fs::stable_read_with` (`stable_read` was refactored onto it; same checks, with the open and the F3 check made pluggable), with link-unchanged and same-referent rechecks. Directories are reopened as `"."` beneath the `O_PATH` fd. ENOENT/ENOTDIR mean dangling, and ELOOP means `Unmanaged(Loop)`. Children of a followed directory are indexed under the link's path (`l/x`), and only the link's own entry carries `via_link`. T11/T17 must check ancestors for `via_link` before writing beneath a followed link.
  - **Change rules (design §3 updated):** a logical change is kind, hash/target, mode, and mtime for files only. Directory and symlink mtimes alone are not changes, and the stored value is the one from the last logical change. Logical changes use `vv.bump_after(id, txn.max_counter())`. A change to `LocalMeta` alone goes through the new `WriteTxn::put_local` (same seq, so `changes_since` doesn't re-report it). Unmanaged entries compare by reason only. Tombstones get mode 0, mtime 0 and default `LocalMeta`. An `Unmanaged` entry whose object disappears becomes a tombstone too, as the task text asks. T12 should check that this is what it wants (see the file→ignored-link→deleted case).
  - **Racy:** a file is `racy` when its ctime ≥ scan start − window. `DEFAULT_RACY_WINDOW` = 1 s; tests use 200 ms and sleep past it. Only files (incl. followed files) are ever racy.
  - **Tombstones and protection:** the scan records every path it found. Afterwards, indexed non-tombstone entries in the scanned regions that weren't found become tombstones, except dirty or failed paths and their subtrees. Updates are batched (1024 per write txn), and every write checks that the entry still has the seq read at the scan start; otherwise the path is reported dirty.
  - **Scoped scans:** paths are deduplicated to the outermost ones. Each one is reached by walking down from the root, recording every ancestor (so a new `n1/n2/leaf` also indexes `n1` and `n1/n2`). If an ancestor is gone, or is not a real directory (a file, or a symlink, followed or not), that ancestor is scanned recursively instead and becomes the tombstone region.
  - **Hasher** (`scan/hasher.rs`): `Hasher { hash_at, hash_with, hashed() }`. The sink reaches the hook point `scan.read_chunk` on every chunk. The scanner also has `scan.stat` after each `statx`. Neither is in `commit.rs`, so `tests/attack.rs` is unaffected.
  - **Not done / follow-ups:** `LocalMeta.mnt_id` is still 0, because `Fingerprint` has no mount ID. A mount point inside a root is reported in `errors` on every scan and never indexed. A directory that can't be opened (EACCES) keeps its old entry, even its mode. Out-of-tree watches and `-K` are T16/T17. A munged replica unmunges every target; spotting user-made unprefixed links is T17. Recursion depth is unbounded (one fd and one stack frame per level).
  - Tests (19 new): `tests/scan.rs` (12): first scan under `Links` (files, modes with setuid stripped, sticky dir, links incl. absolute, dangling and non-UTF-8, FIFO → `Special`, reserved names skipped, distinct counters) and `Skip`; modify → one bump above the Lamport floor, quiet rescans, mtime-only change, chmod, dir mtime not a change, symlink re-created identical vs retargeted; untouched rescan hashes 0 and racy entries are rehashed exactly until settled (cleared in place, same seq); deletion of a tree → tombstones, no re-bump, re-creation dominates the tombstone, file→dir; `CopyLinks` loops (`..`, `.`, an ELOOP chain); dangling (missing, absolute, through a file) and then resolved; followed files/dirs in and out of tree, a diamond, the shortcut for followed files, local-only retarget, referent content change; `CopyDirlinks`, `SafeLinks`, `CopyUnsafeLinks`; munged targets; scoped scans (dedupe, ancestors indexed, missing ancestor, ancestor replaced by a symlink, root in scope); an unreadable dir keeps its subtree. Unit tests (4): `prefixes`/`outermost`; a read that changes on every attempt → dirty, entry unchanged, exactly 4 attempts; a dir swapped for an outside symlink between `statx` and open → dirty, nothing outside indexed; an index entry rewritten mid-scan → dirty, not overwritten. Plus `put_local_keeps_seq` in `store.rs`.

## M3: engine → simplified variant complete

### [x] T11: Replica trait and LocalReplica
- **Depends on:** T06, T10
- **Read:** §7, §5.3 step 5, §5.8 (only the "no echo" idea; the journal comes in T15)
- **Files:** `src/replica/{mod,local}.rs`
- **Do:**
  - Implement the trait, `Op`, `Precondition` and `Outcome` exactly as in §7.
  - `LocalReplica` owns a `Root`, an `IndexStore`, the `Caps`, a `Quarantine` and its config.
  - `apply` maps a logical `Precondition::Matches{..}` to the indexed `LocalMeta` fingerprint, calls the right `fs::commit` function, and on success writes the resulting Entry into the index (the vv comes from the op).
  - `open_read` wraps `stable_read`, streaming, and verifies the expected hash at EOF.
  - `watch()` returns `None` for now.
- **Done when:**
  - unit tests apply each `Op` on a tempdir replica and check both the disk and the index;
  - a stale precondition gives `PreconditionFailed`.
- **Notes:**
  - **API deviations (design §7 updated):**
    - `apply` takes the `path` (§7's signature had none).
    - Every `Op` that leaves an entry carries its `vv` (and `Mkdir`/`Symlink` their `mtime_ns`), since "the vv comes from the op": `WriteFile{meta: FileMeta, hash, vv}`, `Mkdir{mode, mtime_ns, vv}`, `Symlink{target, mtime_ns, vv}`, `Delete{vv}`, `Rmdir{vv}`, `RenameToConflict{to: RelPath}`, `SetMeta{mode, mtime_ns, vv}`.
    - `Precondition::Matches{kind: Kind, vv}`: the `Kind` carries the hash or target, and `Precondition::matching(&entry)` builds one.
    - `Outcome::Applied(Entry)` is the path's new entry and `PreconditionFailed` carries the current index entry, both without `LocalMeta`, like `changes_since`.
    - `ContentReader` = `Read + Send` (blanket impl). `Op`, `Precondition` and `FileMeta` derive serde for T20.
  - **`LocalReplica`** (`replica/local.rs`): `open(&ReplicaConfig, pair_dir)` owns the capability probe (log + `require_minimum`, as T02 asked). Builder options `racy_window` and `quarantine_grace`. Accessors `root`, `caps`, `index`, `config`, `quarantine`, plus `sweep_quarantine()` for T16's timer (not in the trait). `scan` builds the T10 `Scanner` with the replica's policy and `munge_links`. `watch()` returns `None`. A minimal `watch::Hint { Paths, FullRescan }` exists for the trait; T16 owns it.
  - **apply** is one redb write txn: logical check against the index, then `Expected` from the entry (files from the index; symlinks by checking the indexed (dev, ino, ctime) against a fresh `statx`, because the index keeps a symlink's mtime from its last *logical* change), then the `fs::commit` call, then the put(s) and the commit. A failed commit or an `Err` aborts the txn, so the index is untouched.
    - Refused with `PreconditionFailed`: an `Unmanaged` entry; a path that is, or lies beneath, a followed link (`via_link`; T17's write-back); a parent that is not an indexed real directory; `Rmdir` while an indexed descendant is still live.
    - `Error::InvalidOp` (new) for an op that can't fit the indexed kind (caller bug).
    - Files we write are stored `racy`: the next scan rehashes them once (and, being within that scan's racy window, once more). It finds nothing to bump: no echo, tested after every op.
  - **New commit functions** (each gets hook points and attack-suite coverage):
    - `commit::rename_to(ctx, path, &Expected, to)` for `RenameToConflict`: pin and check, move aside to a reserved name, verify (rehash), restore on change, then `NOREPLACE` onto `to` (restore → `PreconditionFailed("new name exists")` if it was taken), then step 5 at `to`.
    - `commit::set_dir_mode(ctx, path, &Fingerprint, mode)`: pin the dir, require the same inode **and** the indexed mode, `fchmod` through the pin, then step 5. This is the one in-place change (design §5.4).
    - Hook points: `rename.{before_pin,pinned,before_rename,after_rename,before_rehash,verified,before_restore,moved}` and `chmod.{before_pin,pinned,after_chmod}`.
    - `tests/attack.rs` gained `RenameFile` (Plain + EditBefore at `rename.before_rename`), `RenameSymlink` and `ChmodDir`: 1464 cases (was 1242), all passing with no change to existing commit code.
  - **RenameToConflict index update:** the path becomes a tombstone with the old vv plus a **local bump**. With the old vv unchanged, it would equal the peer's view of the loser while the content differs, and §6.1 would loop on "rescan". The copy at `to` gets a fresh vv `{local: tombstone counter + 1}`. Files and symlinks only; `to` must be a sibling (`InvalidPath` otherwise). T12 must resolve dir-vs-dir (mode) conflicts without renaming.
  - **SetMeta:**
    - A file whose mode or mtime differs is rewritten as a **copy** (a `StableReader` on itself → `replace_file`; the hash check guards against a change mid-copy), never `fchmod`ed in place. In-place metadata changes would hide a concurrent write from the rehash shortcut.
    - A directory whose mode differs → `set_dir_mode`.
    - Otherwise the index alone is updated after a `statx` check: same file mode/mtime, a directory's mtime, any symlink. So T12/T13 can use `SetMeta` with unchanged metadata to record a **merged vv** (`MergeVv`).
  - **open_read** uses the new `fs::StableReader` (in `stat.rs`): one attempt, §5.2 checks, size checked at open, hash at EOF. Failures are `io::Error`s wrapping `Error::Unstable`; new `Error::from_stream` unwraps them, and `TempFile::copy_from` now uses it, so a torn source makes `apply` return `Err(Unstable)` with nothing committed. Also new: `Error::is_unstable`, `Error::is_not_found`.
    - Our index entry must have the expected kind (else `Unstable`), and `expect` must be a file (else `InvalidOp`).
    - Followed links (`-L`) are read via `Root::open_referent`, moved there from the scanner. At EOF the link must still be the indexed inode with the same ctime; the referent path is not re-resolved.
  - **CLI:** `sync --once` and `daemon` now open both `LocalReplica`s (probe, log, open the index), then still say "not implemented" (T13/T16).
  - **For T13/T17/T18:**
    - `apply` doesn't check whether this replica's policy would manage an incoming symlink (e.g. an unsafe target under `SafeLinks` becomes `IgnoredLink` on the next scan, which bumps it). T17 should refuse or adapt that.
    - The `SetMeta` file copy keeps its read fd open across the exchange, which would block T18's lease. T18 should close it first (e.g. stage, then drop the reader).
    - `Preserved` leaves the index untouched (rescan).
  - Tests: 12 in `replica/local.rs`:
    - create/replace/file↔symlink, with no echo;
    - stale vv, kind or `Absent`, a disk change since the scan for every CAS op, a hook-injected edit mid-commit, unindexed name, unmanaged FIFO;
    - mkdir, nested write, parent-not-indexed, rmdir with live child, delete, rmdir, recreate over a tombstone;
    - munged symlinks;
    - rename to conflict (non-sibling, taken name, vv shapes, `changes_since`, winner create);
    - `SetMeta` (file copy, index-only merge, dir chmod, dir mtime, symlink, user chmod → `PreconditionFailed`);
    - `open_read` (normal 200 KB, foreign entry, non-file, rewrite mid-read → stays failed, size changed, torn source → apply `Err` with nothing committed);
    - `-L` read and retarget mid-read, write refused;
    - invalid ops; `changes_since` without `LocalMeta`.
  - Plus 6 in `commit.rs`: rename trace and refusals, an edit before the move → restored, the name taken after verify → restored; chmod trace and refusals, a dir swapped after the pin → only the pinned one changed.

### [x] T12: Reconciler and planner (pure)
- **Depends on:** T08
- **Read:** §6 (all)
- **Files:** `src/engine/{mod,reconcile,plan}.rs`
- **Do:**
  - `reconcile(a: &dyn IndexView, b: &dyn IndexView) -> Vec<Action>` with no I/O, implementing the §6.1 table and §6.2 winner choice.
  - `Action { path, kind: Push{from, to, entry} | MergeVv | Conflict{winner, loser_side, conflict_name} | Skip(reason) | Rescan }`.
  - `plan(actions) -> Vec<Phase>`, ordered as in §6.3, including type-change splitting and directory-delete resurrection.
- **Done when:**
  - an exhaustive table test over {absent, file, dir, symlink, tombstone, unmanaged}² × {vv equal, a>b, b>a, concurrent} × {same content, different content} produces the expected action;
  - an ordering test passes;
  - a resurrection test passes.
- **Notes:**
  - **API deviations (design §6.1–§6.4 updated):**
    - `reconcile(a, b, now: jiff::civil::DateTime)`: `now` (local wall-clock time) is only for conflict names, so the function stays pure. T13 passes `jiff::Zoned::now().datetime()`.
    - `IndexView` (new trait in `engine/mod.rs`): `replica`, `get`, `entries` (path order), `descendants`. `engine::Snapshot::new(id, entries)` is the in-memory one; T13 builds it from `changes_since(0)`. `Side { A, B }` names the replicas.
    - `ActionKind`: `Push{from, entry, target}` (`to` is `from.other()`; `target` is the precondition entry), `MergeVv{vv, mtime_ns, a, b}`, `Conflict(Resolution)`, `Resurrect(Resolution)`, `Skip(SkipReason)`, `Rescan`. `Resolution{winner, entry, vv, loser, conflict_name: Option<RelPath>}` replaces the task's `Conflict{winner, loser_side, conflict_name}`. `SkipReason::{Unmanaged(Side), BeneathUnmanaged(RelPath)}`.
    - **Resurrection is decided in `reconcile`, not `plan`:** a resurrected type change needs a conflict name, and the check needs the views. `plan(&[Action]) -> Vec<Phase>` only orders and splits. `Phase{kind: Conflicts|Deletes|Creates, steps}` (non-empty phases only); `Step{side, path, op: Op, pre: Precondition, source: Option<Entry>}`, where `source` is the other side's entry to `open_read` for a `WriteFile`.
  - **Semantics** (all in design §6):
    - **Conflicts write only to the loser side:** rename it away, then create the winner's version there with merged + bump (`Absent`). The winner records that vector in the **next** round (a dominating push of identical content → index-only `SetMeta`). So nothing ever dominates the loser unless the loser side really holds the winner's version, whatever the executor does after a failure. Dir-vs-dir (mode) conflicts: the loser gets the winner's mode via `SetMeta`.
    - Equal vectors with a different file mtime → `Rescan`. Concurrent with the same content → `MergeVv` with the newer mtime. Tombstone vs tombstone → no action, even with different vectors (**T18:** GC must cope with such pairs). A push of the same content → `SetMeta`, no transfer.
    - `Unmanaged` vs live → `Skip`; vs absent or tombstone → nothing. Every action beneath an unmanaged path → `Skip(BeneathUnmanaged)`. The root path is ignored.
    - The delete half of a type change keeps the **target's** vector, so a failed create is retried next round.
    - Resurrection: a dir removal on side S is blocked if S would still hold anything beneath it after the round (present and not deleted, `Unmanaged` included, or created). Deepest first, so ancestors follow. Over a type change, the other side's file or symlink is renamed to a conflict copy first.
    - Conflict names: `<ID7>` of the loser's replica; a name live in either index, or already chosen in this call, moves on by one second.
  - **For T13:**
    - Run phases in order. After a step fails, skip that path's later steps and mark it dirty.
    - `Rescan` actions are dirty paths too.
    - Keep looping while `reconcile` yields non-`Skip` actions, not only while paths are dirty: conflicts and resurrections need a second round.
    - `Skip(Unmanaged(_))` deserves a warning.
  - **Tests** (13 in `engine/`):
    - `decision_table_is_exhaustive`: {absent, file, dir, symlink, tombstone, unmanaged}² × {equal, a>b, b>a, concurrent} × {same, different content} = 288 cases, checked against a separate written-out oracle. Each case also checks the entries the action carries (merged + bumped vv dominates both, conflict-name format) and that it can be planned.
    - Unit tests: mtime rules; winner rules (mtime, ID tie-break, type rank, bumped vv, dir-vs-dir); conflict names that dodge live names (but not tombstones) and each other (long names that truncate to the same stem); an unmanaged subtree; resurrection (nested, untouched siblings still deleted); resurrection over a type change and over an unmanaged child; root ignored; `descendants` with siblings that sort inside a prefix (`d-x`, `d.x`).
    - `plan`: a push matrix (every target kind under every pushed kind → ops, phases, preconditions, vectors, source); an ordering test (rename → deletes depth-descending → creates depth-ascending, including a dir→file type change and `MergeVv`); a resurrection plan.
    - `engine/sim.rs` (proptest, 512 cases): two in-memory replicas that mirror `LocalReplica`'s index semantics (incl. `InvalidOp` as a failure). It runs random edits on a synced base, then concurrent random edits on both sides, then syncs. It asserts convergence in ≤ 3 rounds (limit 5), equal live entries, no failed steps, and that every file version survives unless the peer's vector dominates it. Mutation-checked: disabling resurrection or the conflict rename makes it fail.

### [x] T13: Executor, conflicts, `sync --once`, test harness
- **Depends on:** T11, T12
- **Read:** §6.2, §6.4, §7, §9 (harness)
- **Files:** `src/engine/{executor,conflict}.rs`, `src/cli.rs`, `tests/harness/mod.rs`, `tests/sync_once.rs`
- **Do:**
  - `Engine::sync_once(a: &mut dyn Replica, b: &mut dyn Replica)`:
    - scan both replicas;
    - reconcile;
    - execute the phases, streaming `open_read` → `apply`;
    - collect dirty paths from `PreconditionFailed` and unstable reads;
    - rescan those paths and repeat, up to 5 rounds.
  - Conflict handling: `RenameToConflict` on the loser side, then normal propagation.
  - Wire up the `sync --once <pair>` CLI.
  - Harness:
    - `Pair::new(policy)` creates two tempdir replicas plus state directories;
    - a scenario DSL (`a.write("x", "1")`, `b.symlink(...)`, `sync()`, …);
    - `assert_converged()` checks the trees are equal after policy normalization, the version vectors are equal, and there are no reserved leftovers.
- **Done when:** scenario tests pass for:
  - create, modify and delete on each side;
  - concurrent modification → conflict copy on both sides with both contents;
  - concurrent delete vs modification → modification survives;
  - type change;
  - nested directory create and delete;
  - symlinks under `Links`.
  - `cargo run -- sync --once` works on two real directories.
- **Notes:**
  - **API (design §6.4 updated):**
    - `engine::Engine` (builder `max_rounds`): `sync_once(a, b)` = `sync(a, b, Scope::Full)`. `sync` takes the initial scan scope, for T16's watcher-driven cycles. Each round reconciles the **whole** indexes (`changes_since(0)`); incremental exchange by `seq` is left for T16/T20.
    - It returns `SyncReport { rounds, applied, retried, conflicts: Vec<ConflictCopy>, resurrected, unmanaged, errors: Vec<(RelPath, String)>, unresolved }` and `is_converged()`. `engine::MAX_ROUNDS = 5`; `sim.rs` now uses it.
    - `engine/conflict.rs` is bookkeeping only (`ConflictCopy {side, path, copy}`, logging of conflicts and resurrections). The resolution itself is already in reconcile/plan (T12): `RenameToConflict` on the loser side, then normal propagation.
  - **Executor behaviour:**
    - These mark a path dirty (skip its later steps, rescan it on both sides before the next round): `PreconditionFailed`, `Preserved`, `Err` that `is_unstable()`/`is_not_found()` from either `open_read` or `apply`, `Rescan` actions, and `ScanStats::dirty`.
    - Other per-path errors (also `InvalidOp`, an engine bug) are logged and reported, and that path and its subtree are left alone for the rest of the cycle. Only `Db`/`BadIndex` errors and scan errors (root or index) abort.
    - `ScanStats::errors` paths are reported, not blocked: the scanner keeps their entries unchanged, which is safe to reconcile.
    - The loop stops when a reconcile yields no steps and nothing is dirty. A round that only rescans counts toward the 5.
  - **CLI:** `sync --once` runs one cycle, prints a summary (applied, conflict copies, resurrections, unmanaged, errors, unresolved) and exits non-zero if not converged.
    - Before exiting it drains both quarantines (sweeps until empty, at most 10 × `DEFAULT_GRACE`). Otherwise every replaced file would leave a `.~ffsl.old.*` behind when the process exits. **T15** should still sweep stale quarantine names at startup (a crash or a give-up leaves them).
    - The `stubs_…` CLI test now covers only `daemon`/`status`. New test `sync_once_syncs_two_directories` (also checks that no reserved names are left after a replace).
    - Checked by hand with `cargo run -- init` + `sync --once` on two scratch dirs: initial sync, then a concurrent edit → conflict copy on both sides.
  - **Harness** (`tests/harness/mod.rs`, design §9 updated):
    - Holds two `LocalReplica`s, not generic `Replica`s: **T22** has to abstract over replica construction.
    - The `Racing` wrapper passes every `Replica` call through to its `LocalReplica` and injects a user edit before the first `apply`/`open_read` at a path. It covers the retry loop without the `hooks` feature.
    - `assert_converged`'s normalization handles only `Links` and `Skip` (plus unmunging). Other policies hit `unimplemented!` (**T17**).
  - **Tests** (`tests/sync_once.rs`, 15):
    - create, modify (content, file mode), delete on each side, plus a no-op second cycle;
    - concurrent modification with each side winning by mtime → one conflict copy with the loser's ID7, both contents on both sides;
    - concurrent identical change → `MergeVv`, no copy;
    - delete vs modification on each side → the modification survives;
    - dir delete vs new child → resurrection;
    - type changes file→dir→symlink→file→dir→file;
    - nested dirs (create, dir mode, recursive delete);
    - symlinks under `Links` (relative, dir link not descended, absolute, dangling, escaping, retarget, delete, concurrent retarget → conflict copy of the link);
    - `Skip` leaves links alone and reports a link-vs-file `Unmanaged`;
    - vv records both replicas;
    - races: destination edited before `apply` → retry → conflict; source rewritten before `open_read` → `Unstable` → resent; directory filled just before `Rmdir` → kept.
  - Mutation-checked: skipping conflict renames, or not rescanning dirty paths, makes tests fail.

### [x] T14: Model-based property test
- **Depends on:** T13
- **Read:** §9
- **Files:** `tests/model.rs`
- **Do:**
  - proptest generates sequences of ops (write, append, delete, mkdir, rmdir, symlink, rename) on A or B, interleaved with `sync()`.
  - A simple in-memory reference model predicts the outcome: last writer wins per causal history, and concurrent edits produce a conflict copy.
  - After a final sync, compare the model with the real trees: equal paths, and the content sets including conflict copies.
- **Done when:** 256 cases pass. Every shrunk failure found along the way gets fixed and added as a regression scenario in `tests/sync_once.rs`.
- **Notes:**
  - **Model** (`tests/model.rs`, design §9 updated): independent of the engine. Causal history per path is a set of event IDs, not a vv. Events are recorded at sync time, like the scanner: only when a path differs from its record (kind, content, target; mtime for files only). Per-path resolution, then a deepest-first resurrection pass. Conflict copies are predicted as (dir, origin name, loser's ID7, content) and their real names adopted from disk; everything else must match exactly after **every** sync, on top of `Pair::assert_converged`.
  - **Ops** (on A or B, paths of 1–3 components over `{a, b}`): write (unique content and mtime; replaces a dir or link, creates parents), append, recursive delete, mkdir (replaces a file or link), rmdir (empty only), symlink (5 targets: relative, nested, `../`, absolute dangling, `.`), rename (to a free path in an existing dir), sync. An op whose precondition does not hold (e.g. a file ancestor) is a no-op in both model and tree. Mode changes are not generated (dir-vs-dir mode conflicts stay covered by `engine/sim.rs`).
  - **Symlink mtimes:** a symlink's recorded mtime decides symlink-vs-symlink conflicts. A synced link's on-disk mtime is the commit time, so the user op gives every created or moved (renamed, or inside a renamed dir) link a fresh mtime via `utimensat(AT_SYMLINK_NOFOLLOW)`.
  - **Results:** 256 cases (~35 s debug) pass; 5000 cases in release passed too. Per 256 cases about 200 conflict copies and 100 resurrections (a third over a type change) are predicted and checked.
  - **Shrunk failures:** one, a model bug (a symlink re-created with the same target is no change; the model compared its mtime). Added as `recreated_symlink_with_same_target_is_not_a_change` in `tests/sync_once.rs`. No engine bug found.
  - Mutation-checked: inverting the §6.2 winner or disabling resurrection in `reconcile` fails within a few cases.
  - Not covered: user edits *during* a sync (the `Racing` scenarios in `sync_once.rs`, and T19's stress test), policies other than `Links` (T17).

## M4: durability

### [x] T15: Journal and crash recovery
- **Depends on:** T13
- **Read:** §5.3 step 1, §5.3 step 5, §5.8
- **Files:** `src/index/journal.rs`, `src/fs/commit.rs`, `src/replica/local.rs`, `tests/crash.rs`
- **Do:**
  - Add an `Intent` table to redb.
  - `commit` functions take a `&Journal`: write the intent before the temp file and update its state at each step (`TempWritten`, `Exchanged`, `Done`). Mark it Done in the same transaction as the index update.
  - `LocalReplica::open` replays unfinished intents as described in §5.8.
- **Done when:** `tests/crash.rs` passes. It forks a child that runs one apply with a hook calling `std::process::abort()` at point N. The parent reopens the replica, checks that no user data was lost and no unexplained `.~ffsl.*` files remain, and checks that a following `sync_once` converges. The test sweeps N over every hook point and op type.
- **Notes:**
  - **API (design §3, §5.3 step 1, §5.8 updated):**
    - `index::journal`: `Journal` (table `intents` in the index's redb file; `IndexStore::journal()`, sharing the `Database` through an `Arc`; `Journal::in_memory()` for tests of `fs::commit` alone), `Intent { op, path, parent: Fingerprint, tmp, old, expected: Option<Expected>, staged: Option<Fingerprint>, state }`, `IntentOp { Create, Replace, Delete, Rename }`, `IntentState { Started, TempWritten, Exchanged, Quarantined }`. Methods `begin` (durable), `update(durable)`, `take_open`, `pending`, `finish(done, quarantined)`, `forget`; `WriteTxn::finish_intents` does the same inside the index transaction. `Fingerprint`, `FileKind` and `Expected` derive serde.
    - The journal reaches the commit functions through **`Ctx { journal: &Journal }`** rather than an extra parameter. The ids a commit begins collect in the journal until `take_open`, so the caller can finish them.
    - `commit::recover(ctx, &mut Quarantine, id, &Intent) -> Recovered { removed, quarantined, restored, conflicts, foreign, skipped }`.
    - `Quarantine`: entries carry their intent id; `holds(id)`, `grace`, `set_grace` (deadlines are `since + grace`, so it applies to waiting entries too); `SweepReport::finished` lists settled intents. `rmdir` forgets those it settles; `LocalReplica::sweep_quarantine` forgets the rest.
  - **Commit changes:** every reserved name is chosen at random in `Record::new` and recorded before it exists (`with_fresh_name` is gone; `EEXIST` on a reserved name is an error). `TempFile::create` takes the name; `TempFile::finish(meta, record)` calls `record(&fp)` after the fsync and before linking. Durable redb commits per op (about 2.4 ms each on this WSL2 ext4): create 1, replace 1 with `O_TMPFILE` or 2 (named temp file, symlink), delete 1, rename 1, mkdir/symlink 1; `Exchanged` and all Done records for failed commits and quarantine settles are non-durable. The model test went from ~35 s to ~37 s.
  - **LocalReplica:** `apply` no longer holds a write transaction across the commit (redb has one writer and the commit writes the journal): preconditions are checked on a read snapshot, then one write transaction stores the entries and finishes the intents. `open` replays every pending intent; after a commit that returned `Err`, its intents are replayed at once (an error path may leave reserved names). An intent whose replay fails with an I/O error is kept for the next start. `quarantine_grace` no longer requires an empty quarantine (replay may fill it); new `caps_mut()` for tests (e.g. forcing named temp files).
  - **Replay** (design §5.8): inspection-based, idempotent. Started → anything at `tmp` is ours. N → removed. O at `tmp`/`.del.` → verify: unchanged → quarantine (roll forward), changed → exchange back if the name holds N, else NOREPLACE, else conflict copy. Other objects → put back the same way. A rename's moved-aside object always goes back (roll back). O at `old` → quarantined again with a fresh grace. A moved/replaced parent → skipped, names left alone (logged).
  - **Hook points:** `journal.started`, `journal.temp_written` (in `commit.rs`; the attack suite reaches them; `commit_points` now includes the `journal` prefix; 1578 cases (was 1464), all passing) and `recover.resolved`, `recover.undone`, `recover.done`, `recover.before_rehash` (replay only; excluded from the attack suite's coverage check).
  - **Tests:**
    - `commit.rs` (+6): every reserved name seen at any step is already journaled (both temp strategies), with the right op/state per commit; replay of hand-built crash states: staged objects removed (Started: file, symlink, dir; TempWritten: N removed, foreign left), exchange with O unchanged → quarantined then re-quarantined after a "restart" (late write → conflict copy), O modified → exchanged back / conflict copy when N was replaced / swapped-in object restored, delete and rename move-asides, moved or replaced directory skipped, foreign object at `old` left alone. `journal.rs` (1), `local.rs` (1: intents finish with the index, kept while quarantined, replayed on reopen).
    - `tests/crash.rs` (`required-features = ["hooks"]`, ~22 s): 14 scenarios (create/replace file, create/replace symlink, file↔symlink, mkdir with child, delete file/symlink/tree, conflict, chmod file/dir, dir→file), file-staging ones under both temp strategies. **Deviation:** the child is the test binary re-run as `crash_child` (env `FSYNC_CRASH_CHILD`), not a `fork()`, because the parent runs cases on all cores; core dumps are disabled with `setrlimit`. Each case crashes one sync cycle (plus sweeps) at the nth hit of a traced point, then checks: every reserved name left is in its replica's journal; reopen (replay) + zero-grace sweep leaves none; the next sync converges (`assert_converged`) to exactly the crash-free outcome. Late-write variant (held fd on B, written at the crash point, points before the first quarantine sweep): the write must survive. 684 cases. `crashes_during_recovery` crashes the replay again at `recover.done` (311 cases, more than 10 replays crashed).
    - Harness: `Tree` keeps a root path (owning the tempdir optionally), `open_replica`, `Pair::open_at` (existing dirs, not removed on drop), `Pair::dirs`, `Pair::reopen_after(f)` (closes both replicas while `f` runs, then reopens them, which replays).
  - **Mutation checks** (by hand, not committed): recovery disabled → 210/467 cases fail (reserved names left); a delete journaled after its move-aside → `crash.rs` reports unexplained names; replay discarding a modified old file instead of putting it back → late-write cases fail. A non-durable `TempWritten` is **not** caught: a process crash keeps redb's non-durable commits, so durability against power loss is argued (design §5.8), not tested.
  - **Residuals / follow-ups:** a reserved name created by someone else at one of our random names would be removed in state `Started` (only possible deliberately). `.~ffsl.old.*` names from before T15 (no journal) are left alone. **T16:** the daemon's quarantine timer must use `LocalReplica::sweep_quarantine` (it forgets settled intents). **T18:** the lease checks hook into the same commit steps; tombstone GC is unaffected.

## M5: daemon

### [x] T16: Watcher and daemon
- **Depends on:** T13 (T15 recommended)
- **Read:** §5.9, §6.4
- **Files:** `src/watch/{mod,inotify,debounce}.rs`, `src/daemon.rs`, `src/replica/local.rs` (`watch()`), `src/cli.rs`, `tests/daemon.rs`
- **Do:**
  - Watcher behavior:
    - inotify via `/proc/self/fd/<dirfd>`;
    - a map wd → (dev, ino) → path;
    - watch-then-scan for new directories;
    - `MOVE_SELF`/`DELETE_SELF` handling;
    - overflow → `Hint::FullRescan`.
  - Debouncer: 200 ms quiet, 2 s max.
  - `EventSource` trait for test injection.
  - Daemon loop: wait for hints from both replicas or the periodic timer (10 min), then `sync_once` scoped to dirty paths; full scan on overflow or timer. Also run `Quarantine::sweep` on a timer.
  - Our own writes must not trigger extra sync actions (verify with an action counter).
  - Clean shutdown on SIGINT/SIGTERM.
- **Done when:** tests pass for:
  - an edit in A appears in B within 3 s;
  - a burst creation of a 3-level directory tree with files appears in full;
  - an injected overflow → full rescan and convergence;
  - a quiet tree → no actions for 5 s after convergence.
- **Notes:**
  - **API (design §5.9, §6.4 updated):**
    - `watch`: `Event { Dirty(RelPath), Overflow }`; `EventSource: Send` with `wait(timeout) -> Result<Vec<Event>>`; `ChannelSource::new() -> (Sender<Event>, ChannelSource)` for injection; `Debouncer` (pure; `QUIET` 200 ms, `MAX_DELAY` 2 s, `MAX_PATHS` 10 000 → `FullRescan`; `push(event, now)`, `deadline`, `take_due(now)`, `take`); `Watcher::spawn(name, source, debouncer)` (thread, 100 ms tick, stopped and joined on drop; a source error counts as an overflow); `InotifySource::new(Root)` (`watches()`, `LIMIT_POLL` 60 s).
    - `LocalReplica`: `watch()` starts the watcher once (later calls return the same hints; `None` with a warning if inotify fails); `set_event_source(Box<dyn EventSource>)` replaces inotify (a `&mut self` setter rather than a builder, so tests can set it on a harness replica). The watcher is owned by the replica and stops with it.
    - `fs::Root::try_clone` (a `dup`; the watcher gets its own root fd without resolving the path again).
    - `daemon::Daemon` (builders `engine`, `rescan_every`, `retry_delay`, `reports(Sender<CycleReport>)`): `run(&mut LocalReplica, &mut LocalReplica, &Receiver<()>) -> Result<DaemonStats { cycles, full_cycles, applied, conflicts, unconverged }>`. `CycleReport { full, paths, report }`. `daemon::shutdown_signals()` blocks SIGINT/SIGTERM and turns them into the stop message from a `sigwait` thread (a second signal exits at once with 128+sig).
  - **Inotify details:** watches go on `/proc/self/fd/<fd>` of a directory opened with `Root::open_dir` (beneath, no symlinks). New directories are watched breadth first, each watch added before its directory is listed; the event itself makes the whole subtree dirty, scanned by the next cycle. A directory's `MOVED_FROM`/`DELETE` removes the watches of its subtree and `MOVED_TO`/`CREATE` re-adds them, so no watch reports under a stale path; `MOVE_SELF` on a still-mapped directory dirties its parent (design), on the root it is an overflow. `IN_IGNORED` drops the mapping. Reserved names are ignored.
  - **Daemon:** watch first, then the initial full cycle. Scoped cycles scan the hinted paths of **both** sides on both replicas (cheap; keeps `Engine::sync` unchanged). Unresolved paths are retried after 1 s even without a new event. The quarantine is swept at its next deadline. Our own writes cause one empty scoped cycle each (no action; tested). A cycle that fails as a whole (`Db`, `BadIndex`, root) ends `run` with the error, no backoff. The engine now logs a cycle with nothing to do at `debug` instead of `info`.
  - **CLI:** `daemon <pair>` blocks the signals before opening the replicas (no thread exists yet), runs until SIGINT/SIGTERM, drains the quarantines, prints totals and exits 0.
  - **Tests:** unit: debouncer (3), watcher thread with injected events (1), inotify (5: existing tree, reserved names and symlinks skipped, burst `mkdir -p` fully watched, moves within/out of the tree, delete, synthetic `MOVE_SELF`/overflow), `LocalReplica::watch` (1). `tests/daemon.rs` (6, ~7 s): edit A→B (≈220 ms; exactly one action, followed by an empty echo cycle), burst of a 3-level tree (4×4×4 dirs, 84 files) plus a later edit deep inside it, injected overflow (A's watcher is a `ChannelSource`; an unseen edit and delete arrive only after the injected `Overflow`, via a full cycle), the periodic timer (1 s) with no watcher events, a quiet tree (no cycle at all for 5 s after convergence), concurrent edits → conflict copy. Each ends with `assert_converged`. `tests/cli.rs`: `daemon` syncs, then stops cleanly on SIGTERM and on SIGINT with no reserved names left (the old "daemon is a stub" test now only covers `status`). Stable over 8 repeated runs and under concurrent CPU load.
  - **Mutation check** (by hand): not watching new directories → the burst test fails (the later deep edit never arrives). This needed the test to wait out the echo cycles first, whose scans would otherwise pick up the edit anyway.
  - **Follow-ups:**
    - **T17:** no watches for followed links' referents. Changes beneath a followed directory link are indexed under the link's path (`l/x`), but in-tree events name the real path. So they are only picked up by a full rescan. Out-of-tree referents are not watched at all.
    - Every cycle reconciles the whole indexes (`changes_since(0)`), so each empty echo cycle still costs O(index). Incremental exchange by `seq` is T20.
    - The root's fd follows the directory: a root that is moved away keeps syncing at its new place (the `MOVE_SELF` only triggers a full rescan). A deleted root scans as empty after its contents were deleted (as with `sync --once`).
    - **T18** `status` could report `DaemonStats` and the last cycle time; the daemon has no state file yet.

## M6: symlinks complete

### [x] T17: Full symlink policy matrix and -K adoption
- **Depends on:** T13, T09
- **Read:** §4 (all)
- **Files:** `src/symlink/policy.rs`, `src/scan/scanner.rs`, `src/replica/local.rs`, `tests/symlink_matrix.rs`
- **Do:**
  - Implement the remaining policy behaviors:
    - `-L` write-back (incoming change replaces the link with a real object), plus the `followed_write=conflict` option;
    - `-K` adoption with a pinned dirfd, plus `--keep-dirlinks-unsafe`;
    - munge per replica;
    - extra watches for out-of-tree referents.
  - Matrix test: policy × {safe relative, unsafe relative, absolute, dangling, link to a file, link to a directory, loop, munged-looking target} × direction (A→B, B→A, both changed). Each case has the expected tree on both sides.
  - Differential test: when `rsync` is in PATH, for each one-directional case run `rsync -a <opts> A/ C/` and assert that our B equals C, ignoring our reserved names and conflict copies.
- **Done when:** the matrix passes; the rsync differential passes or is skipped with a message when rsync is absent.
- **Notes:**
  - **Design:** new §4.3.1 describes what was built. §4.2, §4.4, §4.5, §5.8, §5.9, §7 and §9 are updated too.
  - **API:**
    - `config`: `FollowedWrite { Replace (default), Conflict }` as `followed_write`, and `keep_dirlinks_unsafe` (both optional in TOML).
    - `LinkInfo::adopted`. **Index schema 2**, so a T16 index is refused with `BadIndex`.
    - `Replica::adopt(path) -> Result<bool>`, with a default of `Ok(false)` (T21 forwards it).
    - `Scanner::keep_dirlinks(on, unsafe_ok)` and `Scanner::adopt(paths)`.
    - `Root::at_fd` and `fs::root::follow_at`.
    - `commit::materialize` + `CopyNode`, `commit::set_referent_mode`, `IntentOp::Materialize`.
    - `watch::Followed`, `EventSource::follow` (default no-op), `Watcher::follow`.
    - Harness: `Opts`, `Pair::with`, `Pair::open_at_with`, `Tree::{synced, raw, opts, adopted}`, `raw_tree`.
  - **Deviations:**
    - **`-K` adoption is requested by the engine.** The scanner cannot see the peer's entry. So before each reconcile, the engine calls `Replica::adopt` once per path and cycle where one side has a directory and the other a symlink or `IgnoredLink`. The replica rescans that path with the scanner's `adopt` set. The adoption is a local change (bumped vv).
    - **`followed_write = conflict` declines changes:** the local version re-asserts itself with `merge + bump`, and incoming files and symlinks become conflict copies beside the followed link. Beneath a followed directory a copy is flat, named after the path's last component. A `RenameToConflict` there is a no-op `Applied`.
    - **Loops and rsync:** loops under `-L`/`-k` are `Unmanaged(Loop)`, while rsync recurses until the path is too long. They are excluded from the differential test.
    - **Munging sender and rsync:** with a munging sender, rsync's `--copy-unsafe-links` judges the munged target; we classify the canonical one (§4.2). That combination is excluded from the differential test.
  - **Route** (`replica/local.rs`, `Disk`): replaces T11's "refuse anything at or beneath `via_link`".
    - Reads (`open_read`, materialize) go through every followed directory. Each is opened through its link and must be the indexed link and the indexed directory. Before T17, files beneath a followed directory could not be read (sync looped until `unresolved`).
    - At a followed link itself, the commit acts on the link: `Expected` is the link's fingerprint, and the referent must be unchanged.
    - An index-only `SetMeta` never materializes or declines.
  - **Materialize:**
    - Built from the index: unmanaged entries are not copied (they become tombstones on the rescan, which is harmless), symlinks get their raw targets, and file hashes are checked.
    - After it, a scoped rescan of the link (no bump) and a retry, at most once per directory level.
    - A peer that deletes a whole followed directory makes us materialize it first, then delete it. Correct, but wasteful.
  - **Watcher:**
    - After every scan that changed something, `LocalReplica` hands the watcher all followed and adopted links (an O(index) walk).
    - Aliases are wd → link paths. In-tree referents share the tree's watch; out-of-tree ones get their own. Refreshes add the new watches first, then drop the stale ones.
  - **Tests:**
    - `tests/symlink_matrix.rs` (11):
      - The matrix: 6 policies × 8 cases × {A→B, B→A, both changed} = 144 scenarios, each against a written-out table and the expected tree on both sides.
      - rsync differential on all 96 one-directional cases except the 4 loops under `-L`/`-k`; with rsync 3.2.7 installed, all agree. Without rsync the checks are skipped with a message (checked with `PATH=/nonexistent`).
      - `munged_sender` (3 policies, plus the rsync `--munge-links` differential).
      - `copy_links_write_back`, `followed_write_conflict_keeps_links`.
      - `keep_dirlinks_adopts_and_writes_through` (plus the rsync `-K` differential), `keep_dirlinks_outside_the_root`.
    - `local.rs` (4, replacing `followed_links_are_read_but_not_written`):
      - followed file write-back (refusals included);
      - materialize, including a referent changed mid-copy → nothing committed, no leftovers;
      - conflict mode;
      - `-K` adopt, write-through, chmod, retarget, out of tree.
    - Other unit tests: inotify aliases. `tests/daemon.rs` (+1): in-tree and out-of-tree referents reach B through the watcher alone.
    - Attack suite: `Materialize` (150 cases) and `ChmodReferent` (30), with the `materialize` hook prefix in the coverage check. 1758 cases in all, all passing.
    - Crash suite: per-scenario policy, plus a `materialize` scenario under `-L`. 853 cases (was 684), all passing.
  - **Follow-ups / residuals:**
    - **Crash replay beneath a `-K` link:** an intent of a commit written through a `-K` link records its path relative to the pinned directory. Replay resolves it from the replica root, finds a different parent, and skips the intent: its reserved names are left in the referent, and logged. The crash tests don't cover this. A fix would record the link in the intent.
    - **Mixed policies:** `apply` still writes whatever symlink comes in. With mixed policies (e.g. A `Links`, B `SafeLinks`), an unsafe link written to B becomes `IgnoredLink` on B's next scan, and then stays `Skip(Unmanaged)`. That is stable, but not rsync. With the same policy on both sides, classification is symmetric.
    - **Out-of-tree directories:** a directory created inside an out-of-tree referent is watched only after the scan that its own event triggers.
    - **CLI:** no flags for the new settings yet (config file only). T18's `status` could show adopted links.

## M7: hardening

### [x] T18: Leases, tombstone GC, landlock, status
- **Depends on:** T15, T16
- **Read:** §5.3 step 4(b), (d) and (f), §3 (tombstones), §5.10
- **Files:** `src/fs/lease.rs`, `src/fs/commit.rs`, `src/index/store.rs`, `src/daemon.rs`, `src/cli.rs`
- **Do:**
  - Use `F_SETLEASE F_WRLCK` in replace and delete when `Caps.leases` is true; check `F_GETLEASE` during verify; quarantine unlinks once a lease can be taken.
  - Tombstone GC:
    - track the peer's last-seen vv per replica;
    - GC when equal and older than the retention period (configurable).
  - Optional `landlock` self-sandbox (`--sandbox`): write access to the roots and the state directory only.
  - `status <pair>` prints the index size, pending conflicts, quarantine count and last sync time.
- **Done when:**
  - lease tests pass: an open fd prevents the immediate quarantine unlink, and after close the next sweep unlinks;
  - a GC test passes;
  - a landlock test passes (a write outside the root fails with EACCES), or is skipped when unsupported.
- **Notes:**
  - **Leases** (`fs/lease.rs`): `Lease::take(parent, name, &expect)` opens the file by name (`O_RDONLY|O_NONBLOCK|O_NOFOLLOW`, checked to be the expected inode) and takes `F_WRLCK`; any failure is `None`. `held()` = `F_GETLEASE == F_WRLCK`; `hash()` rehashes through a dup of the lease fd (opening the file again would break our own lease); dropping releases it. The first `take` sets `SIGIO` to ignored unless it has a handler (a lease break sends `SIGIO`, which kills by default). `Caps::probe` now uses `Lease` too.
  - **Commit:** replace takes the lease between `replace.pinned` and `replace.before_exchange` (step (b)), delete before `delete.before_rename`, only when `ctx.caps.leases` and the old object is a regular file. A refused lease is not a failure (falls back to the grace timer). `verify_old` takes `Option<&Lease>`: rehash through it, and a broken lease = changed → undo/restore. The lease is dropped right after (d). `rename_to` takes none (its object stays user data). `StableReader` now closes its fd once the read is verified (or failed), so `SetMeta`'s copy of a file no longer holds it open across the replace (the T11 follow-up; the lease used to be refused there, falling back to the grace timer); test `stable_reader_closes_the_file_at_eof`.
  - **Quarantine:** `Pending.leases` (from the commit's caps). `verdict` stats; if unchanged and leasable, takes the lease, hits the new hook point `quarantine.leased`, re-checks size/mtime through the lease fd (changed → conflict) and returns `Unlink` with the lease, held across the unlink, before the deadline too. Otherwise the old deadline rule. `rmdir`'s settle goes through the same code. The daemon now also sweeps right after each cycle.
  - **Tests (leases):** `fs::lease` (taken only without other fds; an open breaks it and waits for the release); `fs::commit` (5): leased replace + delete unlinked at the first sweep (grace 1 h, trace has `quarantine.leased`); **an open fd keeps the inode quarantined and the next sweep after close unlinks it** (Done-when), a writer's fd still yields a conflict copy; an open during the leased replace / delete (a thread blocked in `open`, detected through `/proc/locks` "BREAKING") → undone/restored with the late write at `name`; symlinks wait for the grace period. The existing commit tests use `Fx::new()` with `leases = false` (their hooks open the old file, which would now wait 45 s for the lease break), `Fx::leased()` for the new ones; likewise one hook in `local.rs`'s `stale_precondition_fails`. `tests/attack.rs`: the `Plain` variant drops the held fd so leases are taken and `quarantine.leased` is attacked (coverage test passes, still ~9 s).
  - **Tombstone GC** (`index/store.rs`): new tables `peers` (peer ID → last sync, ns) and `acks` (peer ID BE + path → postcard `Ack { ours, theirs, since_ns }`), created on open (no schema bump: existing tables unchanged). `IndexStore::record_sync(peer, &[(path, vv, PeerState)], now_ns, retention)` (one `Durability::None` transaction, `WriteTxn::record_sync`). New `Replica::record_sync` (default no-op; `LocalReplica` uses the wall clock), called by `Engine::sync` after every cycle with the last reconcile's snapshots. `PeerState::{Absent, Tombstone(vv), Live}`; an absent peer entry counts as acknowledged (never had it, or already collected it), and any peer tombstone does, whatever its vector, because §6.1 never equalises concurrent tombstones. Retention: `PairConfig::tombstone_retention_days` (serde default 30), `Engine::tombstone_retention`, `DEFAULT_TOMBSTONE_RETENTION`. `SyncReport::collected: [usize; 2]`. Design §3 updated.
  - **Tests (GC):** store (3): collected exactly at `since + retention` (a peer that dropped its tombstone keeps the record's time), a live peer never acks; a changed peer vector restarts the period, a changed own entry is ignored, a recreated path drops its record; every known peer must ack. `tests/sync_once.rs` (2): with the default retention tombstones stay; with zero both sides collect file + dir + child (`collected == [3, 3]`), nothing comes back, a re-created file syncs as new; concurrent tombstones are collected.
  - **Landlock** (`src/sandbox.rs`, raw syscalls via `libc`, no new crate): `sandbox::abi()`, `sandbox::restrict(&[dirs])` handles every write right (+`REFER` ABI≥2, `TRUNCATE` ABI≥3) and allows them beneath the given dirs; `PR_SET_NO_NEW_PRIVS` then `landlock_restrict_self`. Fails if landlock is unavailable. CLI: global `--sandbox`, applied after loading the config and **before** the signal thread starts (roots + pair dir). Test (runs in its own thread, since landlock restricts the calling thread): writes inside work, create/overwrite/unlink/mkdir/symlink/rename-out outside fail with `EACCES`; skipped if unsupported (this WSL2 kernel has ABI 7, so it ran). `tests/cli.rs`: sandboxed `sync --once` (create, replace, delete, rmdir) and `status` work.
  - **Status** (`src/status.rs`): `PairStatus::load(cfg, pair_dir)` reads the indexes (`IndexStore::open`, not `LocalReplica::open`, which would probe and replay); a missing index counts as empty; on `DatabaseAlreadyOpen` (a daemon holds it) it reads `status.toml`, which `Daemon::status_dir(pair_dir)` saves after each cycle and sweep (`PairStatus::of`, live quarantine count). Per replica: entries, tombstones, conflict copies (`fs::is_conflict_name`, new in `tmpname.rs`), quarantined and other unfinished intents, last sync. `config::write_atomic` is now `pub(crate)`. The old `status_is_a_stub` CLI test was replaced by a real one; the daemon CLI test checks `status` while it runs ("daemon running").

### [x] T19: Concurrent stress test
- **Depends on:** T16, T18
- **Read:** §9 (stress test)
- **Files:** `tests/stress.rs` (`#[ignore]`)
- **Do:** follow §9 exactly:
  - writer threads on both trees performing every op type, including directory↔symlink swaps and outside symlinks;
  - a ledger of `(replica, path, pred_hash, new_hash)`;
  - the daemon running in-process;
  - after the writers stop, wait for convergence and check the three invariants.
  - Make the duration and thread count configurable through environment variables.
- **Done when:** `cargo test --release --test stress -- --ignored` passes at the default 30 s / 4 threads per side, and passes 10 consecutive times. Record any flake and its fix in Notes.
- **Notes:**
  - **Design:** §9 "As implemented (T19)" describes the user model, the ledger, the invariants and the reach.
  - **Test:** `tests/stress.rs`, one test `writers_race_the_daemon` (`#[ignore]`, ~37 s). Environment: `FSYNC_STRESS_SECS` (30), `FSYNC_STRESS_THREADS` (4 per tree), `FSYNC_STRESS_SLOTS` (8), `FSYNC_STRESS_PAUSE_MS` (10), `FSYNC_STRESS_SEED` (printed), `FSYNC_STRESS_SANDBOX` (1). Run with `--nocapture` to see the summary (operations done/skipped by errno, daemon totals, retried steps, per-path daemon errors, final cycles, live writes, conflict copies).
  - **Harness:** `Pair::quarantine_grace(d)` (the stress test uses the default 400 ms grace instead of the harness's zero).
  - **Deviations from §9:**
    - The ledger is `Write { path, pred, new }` plus `Removed { hashes }` and `Moved { found }`, instead of only `(replica, path, pred_hash, new_hash)`. A blind overwrite, delete or swap has no single predecessor: it removes whatever was there (a whole directory, maybe just changed by the daemon), so the writer moves it out of the tree atomically and logs what came out. A rename logs what arrived at the new name, which is where the content may then be found.
    - `/tmp/outside` is a per-run sentinel directory `outside/` beside the roots.
    - "At most 3 rounds produce zero actions" is read as: after the writers stop and the daemon goes quiet, at most 3 full `sync_once` cycles until one applies nothing. In every run so far the first one did.
    - In-place writes into an inode that has no name left right afterwards (the rmdir-settle window, §5.10) are exempt and counted (0 in every unmutated run).
  - **Results:** 10 consecutive runs at the defaults passed, no flake (35–36 s each; per run ~18 700 writes, 40 daemon cycles, ~3 150 applied steps, ~1 000 conflict copies made, ~4 200 retried steps, 1–5 blocked `EACCES` commits; the daemon went quiet 0.5–1.4 s after the writers stopped; the first final full cycle always applied nothing; 24–45 live writes checked at the end, 0 exempt). After a last fix to the test (`slot()` could pick a slot past the end with an odd slot count), passes again at the defaults, with `FSYNC_STRESS_SANDBOX=0`, and with 7 slots / 8 threads per tree (~37 000 writes).
  - **Mutation checks** (by hand, not committed): create without `RENAME_NOREPLACE` → 5–7 lost writes, 3 of 3 runs; parent resolution following symlinks → the sentinel changes without the sandbox, `EACCES` (blocked) with it. Replace without its pin check, or without its verification, is not caught: the other one (the rehash against the expected hash), or the quarantine's fingerprint check, still saves the data. Both together are reached about once per 15 s run (the engine's scan → commit gap for a path is a few ms) and were caught in 1 of 4 runs. Narrow windows like that one are the attack suite's job.
  - **Workload tuning** (no engine change was needed): the first version mostly hit absent or wrong-type parents (`ENOTDIR`, `ELOOP`) and left few live writes; in-place writes now create a missing file, a writer recreates a missing slot or `sub` 60% of the time, and slot-level files/links are rarer. A home bias per side (60%) makes replaces under a rare edit of the other side more frequent: the create mutant went from about 2 to 10 hits per run.
  - **Follow-ups:** none required. Possible: a `-L`/`-K` variant (followed links are not exercised here, policy `Links` only); timing-free coverage of the replace window would need hooks, which the attack suite has.

## M8: network → production variant

### [x] T20: Wire protocol
- **Depends on:** T13
- **Read:** §7, §7.1
- **Files:** `src/replica/proto/{mod,messages,framing}.rs`
- **Do:**
  - Request and response enums mirror each `Replica` method.
  - Length-prefixed postcard framing over any `Read + Write`.
  - Content streams as a sequence of `Chunk(Vec<u8>)` messages followed by `End{hash}`.
  - Version handshake.
- **Done when:** a round-trip test of every message over an in-memory pipe passes, and malformed frames produce errors, not panics.
- **Notes:**
  - **Design:** §7.1 "Wire protocol as implemented (T20)" describes framing, handshake, messages, batches, content streams and errors.
  - **Module** `replica::proto`: `mod.rs` (constants `MAGIC`, `PROTOCOL_VERSION`/`MIN_PROTOCOL_VERSION` = 1, `MAX_FRAME` 32 MiB, `CHUNK_SIZE` 64 KiB, `BATCH_BYTES` 1 MiB; `client_handshake`/`server_handshake` → `Session { version, peer }`), `messages.rs` (`Hello`, `HelloReply`, `Request`, `Response`, `Content`, `WireError`, `batches`), `framing.rs` (`write_frame`, `read_frame`, `send_content`, `ContentStream`).
  - **Deviations:** no `Id` request (the handshake's `Welcome` carries the server's replica ID; both sides check the other's ID). Content is a nested `Content { Chunk, End { hash }, Abort(WireError) }` inside `Request::Content`/`Response::Content`; `Abort` was added so a source failing mid-stream (the `StableReader` on the server, or the client's reader from the other replica) ends the stream in step and reaches the receiver as the same error class. `Changes`, `RecordSync` and `Collected` are batched (`more` flag).
  - **Changes outside `proto/`:** `Error::Remote { kind: RemoteKind, message }` and `Error::Protocol { reason }`, `RemoteKind`, `Error::remote_kind()`; `is_unstable`/`is_not_found` recognise remote errors; the executor's `is_fatal` is now `remote_kind() ∈ {Index, Protocol}` (same set as before for local errors, plus protocol and remote index errors). Serde derives on `Scope`, `ScanStats`, `Hint`, `Outcome`. `VersionVector` decodes through its own visitor, which caps the up-front reservation (smallvec's impl reserves the claimed length).
  - **Tests** (15, in `proto/mod.rs`): every `Request`/`Response`/`Op`/`Outcome`/`Kind`/`Content`/`Hello`/`HelloReply` variant round-trips over `std::io::pipe` (exhaustive matches on `Request`, `Response`, `Op` and `Content`, so a new variant won't compile until the test covers it) with a clean EOF after; `LocalMeta` never crosses; the frame layout; handshake (success, version choice, no common version, wrong replica either way, server picking an unknown version, bad magic either way, hang-ups); content (0 / 1 / one chunk / 3 chunks + 17 bytes, in step for the next frame), a torn source → `Abort` → `is_unstable()` on the receiver and stays failed, hash mismatch, an unexpected message, hints set aside by the filter, connection closed at and inside a frame, `drain`; 17 hand-made malformed frames (incl. `..` and NUL in paths, unsorted/zero vv, 2^60-element and 4 GiB length claims, bad bool, bad UTF-8, overflowing `Duration`) are `Protocol` errors and I/O errors stay `Io`; oversized messages are not sent; proptest: random bytes and corrupted/truncated valid messages never panic (2000 cases each); `batches`; error classes survive the round trip and are not prefixed twice when passed on.
  - Mutation-checked by hand: skipping the hash check, the trailing-bytes check, or `drain`'s loop each fails tests.
  - **Pre-existing flake (not fixed, not caused by T20):** `cargo test --features hooks --test crash` fails about 1 run in 6–10, on this branch and on a clean T19 checkout alike (1 of 10 there). Always scenario 14 ("materialize"), at varying hook points (`stage.synced`, `commit.synced`, `scan.stat`): the harness's final rescan of A reports `d/e/l` dirty (`dirty: [RelPath("d/e/l")]`, no changes). Looks timing-dependent; worth a look by whoever touches T15/T17 code next. → **T23**.
  - **For T21:** the server must answer requests in order and drain an `Apply`'s content before answering when `apply` did not consume it. A client that drops an `open_read` reader before EOF must `drain` it (or close the connection); there is no cancel message. The client side of hints needs a reader that demultiplexes `Response::Hint` from answers (`ContentStream`'s filter does this inside a stream). Map `Request::RecordSync` batches back to one `record_sync` call (same `peer` and `retention`).

### [x] T21: RemoteReplica, server, TLS
- **Depends on:** T20
- **Read:** §7.1
- **Files:** `src/replica/remote.rs`, `src/server.rs`, `src/cli.rs`, `src/config.rs`
- **Do:**
  - `serve --listen addr <pair-side>` wraps a `LocalReplica`.
  - `RemoteReplica` implements `Replica` over the protocol.
  - Security:
    - rustls with mutual TLS using self-signed certificates generated at `init`;
    - device ID = blake3 of the certificate;
    - the peer's ID is pinned in the config.
  - Watch hints are forwarded as server-push messages.
  - Incremental `changes_since(seq)` exchange.
- **Done when:** `sync --once` and `daemon` work between two processes over 127.0.0.1, and a wrong peer certificate is rejected.
- **Notes:**
  - **Design:** §7.1 "Network as implemented (T21)": identities and pinning, deployment, server, hint connections, client connections, errors, incremental index exchange, daemon/status. §2 layout and crates updated.
  - **New crates:** `rustls` 0.23.45 (`ring` + `std` only; TLS 1.3 only) and `rcgen` 0.14.10 (`crypto` + `ring`); `rustix` gained the `net` feature (keepalive, `TCP_NODELAY`).
  - **Modules:**
    - `src/tls.rs`: `DeviceId` (blake3 of the cert DER, 64 hex digits, serde as a string), `Identity` (`generate`, `save`/`load` as `<pair>/<replica>.crt`/`.key`, `load` checks the pin, `server_config`/`client_config`), the `Pinned` verifier for both directions.
    - `src/server.rs`: `Server::{new, open(cfg, side, pair_dir), spawn(listener)}` → `ServerHandle::{local_addr, replica, sweep, close_connections, shutdown, into_replica}`.
    - `src/replica/remote.rs`: `RemoteReplica::{connect(remote_cfg, local_cfg, pair_dir), with_tls(addr, local, remote, tls), addr, entries_received, connections}`.
    - `src/replica/mod.rs`: `Housekeeping` trait (`next_sweep`, `sweep`, `status`) for `LocalReplica`, `RemoteReplica`, `PairReplica`; `PairReplica::open(cfg, side, pair_dir)` (local or remote by config).
  - **Config:** `ReplicaConfig.device: Option<DeviceId>` and `.remote: Option<String>` (both omitted from the TOML when unset, so older configs load). `init` now writes both identities (config first, so an existing pair is never touched). `config::init_with(state_home, pair, [NewReplica; 2])` for remote replicas; a remote root must be absolute and is not checked locally; the overlap check only applies when both replicas are local; `ReplicaConfig::device()` errors with `Error::Tls` for a pre-T21 pair.
  - **CLI:** `init --a-remote/--b-remote HOST:PORT` (prints device IDs and what to copy to the server host); `serve <pair> <a|b> [--listen ADDR]` (default address: the replica's `remote`; prints `serving replica … on ADDR`; sweeps the quarantine; SIGINT/SIGTERM stop it cleanly); `sync --once`/`daemon` open `PairReplica`s; `status` shows a remote replica as "served at ADDR"; `--sandbox` covers `serve` (the served root is allowed even though the shared config marks it remote — a bug found by hand and covered by `sandboxed_serve_and_sync`).
  - **Errors:** new `Error::Connection { peer, reason }` (any transport failure; `RemoteKind::Protocol`, so fatal for the cycle; `is_disconnected()`) and `Error::Tls { reason }`. `Daemon` takes `&mut dyn Housekeeping` and retries a disconnected cycle after `RECONNECT_DELAY` (5 s) with a full cycle; existing callers with `LocalReplica`s compile unchanged.
  - **Deviations:**
    - Hints use a separate connection: `Watch` turns its connection into a hint stream (blocking rustls cannot read and write one TLS connection from two threads, and a client idle in the daemon would not read pushed hints on its request connection). Request connections are strictly request/response. Compatible with the T20 messages.
    - The task text's `serve --listen addr <pair-side>` is `serve <pair> <a|b> [--listen addr]`.
    - Each replica (not each host) has a certificate: a client presents the identity of the replica it acts for, so the server pins exactly one device (the peer's). Moving a replica to another host means copying its `.crt`/`.key` there.
  - **Tests:**
    - `tests/remote.rs` (6, in-process over 127.0.0.1): sync both ways (dirs, symlink, non-UTF-8 name, empty and 300 KB files, edits, delete, conflict) with a no-op cycle transferring no entries and the delta bounded, all over one connection; remote index order and `changes_since(seq)`, reads (small reader dropped early → drained, connection kept; large → closed; a request during a read uses a second connection; error classes `Unstable`/`Remote` survive); wrong client certificate, wrong pinned server device, wrong replica ID rejected; hints pushed, then `FullRescan` after the connections were cut; server restart at the same address (cycle fails with `Connection`, the next reconnects, mirror starts over); the daemon with a remote B (hints alone carry B's change; a cut connection is retried).
    - `tests/net.rs` (3, two processes, separate state homes as two hosts): `serve` + `sync --once` + `status` + `daemon` (both directions, SIGTERM) + server certificate not the pinned one (`is not the pinned`) + client certificate not pinned (fails; the server logs `rejected: TLS handshake failed`) + `serve` stops cleanly on SIGTERM; `serve` without an address or identity, sync with nothing listening; `--sandbox` on both sides (skipped without landlock).
    - Unit: `tls` (device ID text, identity save/load/pin/permissions), `config::init_with_a_remote_replica`; `init_round_trips` now expects the identity files.
    - Mutation-checked by hand: a verifier that accepts any certificate fails both `wrong_certificates_are_rejected` and the two-process test. 15 consecutive runs of `remote` + `net`: no flake.
  - **Follow-ups (T22 / later):** the harness can serve a replica in-process with `Server::new(..).spawn(TcpListener::bind("127.0.0.1:0"))` and `ServerHandle::into_replica`; the crash/attack hooks are not reachable through a remote replica's process boundary unless the server runs in-process. No retry of a request on a connection that dies mid-request (the cycle fails and is retried as a whole). Block-level delta transfer (§7.1) → **T24**. The pre-existing `crash` flake noted under T20 was not looked at → **T23**.

### [x] T22: Network test parity
- **Depends on:** T21, T14, T17, T19
- **Read:** §9
- **Files:** `tests/harness/mod.rs`, the existing test files
- **Do:** make the harness generic over a `ReplicaFactory` (local or loopback-remote), then run the sync_once, model, symlink_matrix and stress suites in both modes.
- **Done when:** every suite passes in both modes.
- **Notes:**
  - **Design:** §9 "Harness" describes the modes; §9 stress test, "Both modes".
  - **Harness API:**
    - **`ReplicaFactory` is the enum `Mode { Local, Remote }`** with `Mode::wrap(LocalReplica, peer) -> TestReplica`. Two variants and no state, so a trait with two impls would add nothing.
    - Every pair is opened locally, as before. `Pair::over(mode)` then serves it. `Pair::mode()`, and `reopen_after` and `quarantine_grace` keep the mode (a served replica is taken back with `ServerHandle::into_replica`, changed, and served anew).
    - `Tree::replica` is now a `TestReplica`: `Local(LocalReplica)` or `Remote { client: RemoteReplica, server: ServerHandle }`. It implements `Replica` and `Housekeeping`.
      - `local()`/`local_mut()` give the `LocalReplica` (`Held`: a borrow, or the server's lock) for observations and local-only settings: index entries, config, quarantine, `caps_mut`, `set_event_source`.
      - `into_local()`, `map(peer, f)`, `sweep_quarantine()`.
    - Remote mode serves **both** replicas, so every engine call on either side crosses the protocol. One cached identity pair is shared by all loopback servers and clients (each pins the other), so a pair costs no key generation.
    - `Housekeeping` for a served replica emulates `serve`: `next_sweep` is the server's next deadline, and `sweep` is `ServerHandle::sweep`.
    - `Racing` wraps a `&mut dyn Replica`. In remote mode the edit runs before the client sends the request.
    - `both_modes! { #[attr]* name, … }` (`#[macro_export]`) turns each `fn name(Mode)` into the tests `local::name` and `remote::name`. A function left out of the list is dead code, so clippy fails.
  - **Suites:**
    - `sync_once`: 18 × 2.
    - `symlink_matrix`: 11 × 2. The rsync differential runs in both modes.
    - `model`: `local::` and `remote::sync_matches_reference_model`, 256 cases each. `proptest!` sits inside the two modules, not the macro.
    - `stress`: `local::` and `remote::writers_race_the_daemon`, both `#[ignore]`. A static lock makes them take turns.
      - In remote mode the servers are spawned from a landlocked thread (`Pair::over` on a sandboxed helper thread), because a served replica commits on the server's connection threads and those threads inherit the accept thread's domain. The daemon thread is restricted as before.
      - The initial sync now also runs in the chosen mode.
    - `crash`, `attack`, `daemon` stay local-only, and only their two local-only calls changed (`local_mut()`). Their hooks and event sources live in the replica's process, and the remote paths are covered by the suites above and `tests/remote.rs`.
  - **Results:**
    - `sync_once` (36) and `symlink_matrix` (22) pass in both modes. The model test passes in both modes (~47 s for both together, debug).
    - Stress: 3 consecutive runs of both tests at the defaults passed (~71 s per pair of runs), plus one earlier remote run. Remote runs look like local ones: ~18 700 writes, 37–40 cycles, ~2 900 applied steps, ~950 conflict copies made, ~4 000 retried steps, 4–8 blocked `EACCES` commits (local: 0–6), and the first final full cycle always applied nothing. The `EACCES` in remote mode come from the server threads, which confirms they are sandboxed.
    - No engine, server or protocol change was needed: every scenario behaved the same over loopback.
  - **Mutation check (by hand, not committed):** a server that answers every `Delete` with `PreconditionFailed` fails remote-mode tests (`remote::copy_links_write_back`, `remote::keep_dirlinks_adopts_and_writes_through`, …) while the local ones pass. So remote mode really goes through the server.
  - **Not covered remotely:** crash and attack hooks across the connection (they would need the in-process server's threads to run the hooked commits; possible now, not done). There is also no retry of a request on a connection that dies mid-request (T21).

## M9: follow-ups

### [x] T23: Fix the `crash` suite flake
- **Depends on:** T15, T17
- **Read:** §4.3.1, §4.5, §5.8, §9 (crash suite)
- **Files:** `tests/crash.rs`, plus whatever the root cause turns out to be (likely `src/replica/local.rs`, `src/scan/scanner.rs` or `src/fs/commit.rs`)
- **Do:**
  - Reproduce the flake recorded in T20's Notes: `cargo test --features hooks --test crash` fails about 1 run in 6–10, always in scenario 14 ("materialize", under `-L`), at varying hook points (`stage.synced`, `commit.synced`, `scan.stat`). The harness's final rescan of A reports `d/e/l` dirty (`dirty: [RelPath("d/e/l")]`) with no changes. Run the suite in a loop, with `--release` if that makes it more frequent, until it fails, and capture the logs (`RUST_LOG=ffsl_sync=debug`).
  - Find the root cause. Is it a real defect (a recovery or rescan that leaves the index unstable after a crash during materialize), or a test that is wrong (e.g. a timestamp/racy-window assumption in the harness)?
  - Fix the cause. Do not paper over it with retries, sleeps or a looser assertion, unless the analysis shows the "dirty" report is correct behaviour; then explain why in Notes and assert the correct behaviour instead.
- **Done when:**
  - 50 consecutive runs of `cargo test --features hooks --test crash` pass;
  - the root cause is written up in Notes;
  - if it was a product bug, a deterministic regression test (a hook point, not timing) fails without the fix and passes with it;
  - `cargo test --features hooks` and clippy pass.
- **Notes:**
  - **Reproduced:** 2 of 15 runs of `crashes_at_every_hook_point`, then 3 of 30 runs of the `materialize` scenario alone (cases `materialize.copied`, `scan.read_chunk`, `replace.quarantined`, `stage.linked`, `replace.verified`; both temp strategies). Debug prints at the scanner's dirty branch and the stable-read retry gave the reason every time: `d/e/l concurrent rename during resolution`, sometimes with earlier `unstable read l/e/l … file is leased` lines (retried successfully by the stable read).
  - **Root cause (a product defect, not the harness):** both are `EAGAIN` from the in-tree `openat2` in `Root::open_referent` (`RESOLVE_BENEATH`, symlinks followed). `d/e/l -> ../x` is the only link in the crash suite whose target has a `..`. With a scoped resolve, the kernel refuses each `..` step with `EAGAIN` if the global `rename_lock` (or `mount_lock`) seqcount moved during the lookup, i.e. if **any** rename on the system happened meanwhile, because it cannot prove the `..` did not escape. The suite runs a crash case per core, and every case renames all the time, so the final rescan of A sometimes classified `d/e/l` as unstable. The scanner then reports it dirty (no retry at classification). The stable read did retry (with backoff) but mislabelled the errno as a lease. A rescan was correct to report a path whose lookup failed, but the failure was spurious: openat2(2) says the caller may simply retry. So any busy system made scans of such links flaky (extra dirty rounds, and a "not converged" final rescan in tests). Crash recovery, the journal and materialize were all fine; the scenario index and hook points only varied because the failure depended on the other threads' timing.
  - **Fix:** `open_referent` retries the in-tree lookup on `EAGAIN` up to `SCOPED_RETRIES` (16) times, then behaves as before (the out-of-tree probe; the link is unstable). No other `openat2` in the crate can see this `EAGAIN`: they all use `RESOLVE_NO_SYMLINKS` on `RelPath`s, which have no `..`, or do not scope (`follow_at`, the direct route). Leases also give `EAGAIN` with `O_NONBLOCK`; the bounded retries change nothing for them. No sleeps, and no change to the test or its assertions.
  - **Regression tests (deterministic):** a rename racing *inside* one syscall cannot be triggered by a hook closure, so `fs::hooks` gained **fault points**: `hooks::fault(name) -> Option<Errno>` (always `None` without hooks; not traced, so the crash suite does not crash there) and `hooks::on_fault(name, f)`. `open_referent` asks `fault("root.in_tree_lookup")` before each try.
    - `fs::root::tests::raced_dotdot_lookup_is_retried` (3 injected `EAGAIN`s → the in-tree referent, after 4 tries) and `raced_dotdot_lookup_gives_up` (endless → `EAGAIN` after 1 + 16 tries).
    - `scan::scanner::tests::followed_dotdot_link_survives_unrelated_renames`: the flake's shape (`d/e/l -> ../x` under `CopyLinks`), every first try raced, in classification and in the read. Two scans report nothing dirty, and the link is indexed as the file.
    - `fs::hooks::tests::faults_inject_errnos_untraced`.
    - Mutation-checked: with `SCOPED_RETRIES = 0`, `raced_dotdot_lookup_is_retried` and the scanner test fail (`dirty: [RelPath("d/e/l")]`, the flake's exact report).
  - **Design:** §4.5 (in-tree referents: the `EAGAIN` retry) and §9 (fault points).
  - **Results:** after the fix, 30 runs of `materialize` alone showed no unstable lookup or read at all (the debug prints stayed silent); then 3 consecutive runs of the whole `--test crash` (~30 s each), and `cargo test --features hooks` and clippy (with and without `hooks`) pass. Before the fix, the same materialize-only loop failed 3 of 30 times. **The "50 consecutive runs" criterion was cut short at the user's request** (too long, ~25 min), so it is not met as written → **T25**. The statistical evidence is the 30 + 3 clean runs, plus a debug trace that showed no `EAGAIN` at all after the fix. The deterministic tests above carry the regression.
  - **T25 (2026-10-03): 50 consecutive runs of `cargo test -q --features hooks --test crash` passed** (21:00–21:26, at commit `1f5c9e9`; 3 tests each, 26–43 s per run, 30 s on average). No failure, so T23 stays closed and the criterion is met.

### [x] T24: Block-level delta transfer
- **Depends on:** T22
- **Read:** §3, §5.2, §5.3, §7, §7.1
- **Files:** `src/replica/proto/messages.rs`, `src/replica/proto/mod.rs`, `src/replica/{mod,local,remote}.rs`, `src/server.rs`, `src/engine/executor.rs`, `src/scan/hasher.rs`, and the index if block lists are stored
- **Do:**
  - Extend design §7.1 first with the concrete scheme, then implement it. The starting point is the sketch: fixed, aligned 128 KiB blake3 blocks, as in syncthing (no rolling hash). The source sends the new file's block list. The destination reuses the blocks it already holds in its current file at that path and receives only the missing ones.
  - **Race-freedom stays intact.**
    - The destination assembles a new temp file from stable reads of its own indexed file plus the received blocks. It never writes in place.
    - The temp file is committed by the usual CAS replace against the expected fingerprint.
    - The whole-file hash is checked against `Op::WriteFile`'s hash before commit.
    - If the old file changes during assembly, the result is `Unstable` with nothing committed, and the path is rescanned.
    - All checks run inside the destination replica. The engine still does no I/O, and only orchestrates through `Replica` methods. Extend the trait as needed (e.g. a block list from the source, reads of selected blocks, and an `apply` content that is either a full stream or a delta), with a default that falls back to the full stream.
  - **Protocol:** bump `PROTOCOL_VERSION` to 2 for the new messages. A v1 peer must still work: both sides negotiate down and use full transfers.
  - Use delta only when it can pay off: the destination holds a live file at the path, and the size is above a threshold. Local pairs keep streaming whole files (same host, no network to save).
  - Block hashes may be computed on demand, or stored with the entry (computed by the hasher in the same pass as the file hash). Decide, and note the trade-off in the design.
- **Done when:**
  - changing 1 byte in a 64 MiB file and syncing over the loopback-remote mode transfers at most a few blocks plus protocol overhead. Assert this with a byte counter on the connection.
  - a destination file modified mid-assembly (hook point) commits nothing and leaves no temp files.
  - a forced v1 session still syncs correctly with full transfers.
  - the T22 suites pass in both modes, and so do the attack suite and clippy.
- **Notes:**
  - **Design:** §7.1 "Block-level delta transfer (T24)" (blocks, trait, who decides, assembly, protocol v2, the storage decision, traffic); §7 trait listing; §2 layout; §9 harness and hook points.
  - **Scheme:** fixed, aligned 128 KiB blake3 blocks (`replica::delta`: `BLOCK_SIZE`, `Blocks`, `Delta::plan`, `BlockReader`, `Assembler`); files over `MAX_BLOCKS` (2^19, i.e. 64 GiB) always go whole, so a block list (16 MiB) and a delta fit one frame (no batching needed).
  - **Trait:** four methods with defaults that mean "whole files only": `is_remote()` (false), `blocks(path, &Kind) -> Option<Blocks>` (None), `read_blocks(path, &Kind, &[u32])`, `apply_delta(path, op, pre, &Delta, data)` (both `InvalidOp` by default; only called after `blocks` gave a list). `PairReplica`, the harness's `TestReplica` and `Racing` forward them.
  - **Engine:** a `WriteFile` goes as a delta iff one side `is_remote()`, `pre` is `Matches { File }`, old and new size are both ≥ `Engine::delta_min_size` (default `DELTA_MIN_SIZE` = 1 MiB), and both `blocks()` give a list. `SyncReport::deltas` counts them. If no block is needed, the source is not read at all.
    - **Deviation from a first draft:** I first fell back to a whole transfer when nothing could be reused. Dropped: it only saved the block lists (0.025 %), and with it the T22 suites (single-block files) never reached the delta path. Now every replaced file above the threshold takes one path.
  - **Race-freedom:** `LocalReplica::apply_delta` checks the logical precondition and the delta's shape (`InvalidOp` otherwise), pins the current file through the read route (`Disk::open_pinned`, refactored out of `open_file`; followed links work) and requires its fingerprint to be the indexed one (else `PreconditionFailed`). It then runs the ordinary `apply` with an `Assembler` as content, so `commit.rs` is unchanged: new temp file, whole-file hash against the op's, CAS replace. The assembler checks every block (reused ones against the **new** hash), and at the end that the old file is unchanged and still at its name, then closes it (so the commit's lease works). Any failure is `Unstable` with nothing committed. The source's `BlockReader` requires the indexed fingerprint up front and an unchanged, still-reachable file at the end. New fs primitive: `fs::PinnedFile` (pread at offsets + the `StableReader` end checks); `fs::open_checked` is shared with `StableReader::open_at`.
  - **Block lists are computed on demand, not stored** (trade-off in §7.1): no index schema change. The cost is an extra read of each side's file per delta transfer, only above the threshold and only over a network.
  - **Protocol:** `PROTOCOL_VERSION` = 2 (`DELTA_VERSION`), `MIN_PROTOCOL_VERSION` = 1. The new variants are appended (`Request::Blocks/ReadBlocks/ApplyDelta`, `Response::Blocks`), so v1 encodings are unchanged (a test freezes the variant indices). Each client connection remembers its version: on v1, `blocks()` is `None`, so files go whole. A server refuses v2 requests on a v1 session (protocol error). If the server is replaced by a v1 one between `blocks` and the transfer, the client fails the cycle with `Connection`, and the next cycle goes whole. Caps for tests: `client_handshake_upto`/`server_handshake_upto`, `Server::max_protocol`, `RemoteReplica::with_protocol`.
  - **Traffic counter:** the client's TCP stream is wrapped in a byte-counting `Counted` (TLS records included); `RemoteReplica::traffic() -> (sent, received)`.
  - **Tests:**
    - `tests/delta.rs`:
      - `one_byte_in_64_mib_moves_a_few_blocks`: both replicas served, default threshold. The create moves > 128 MiB; changing 1 byte then moves **314 519 bytes** in all (limit asserted: 2 blocks + 192 KiB; the block crosses twice in loopback-remote mode). Then a block is dropped at the front and one appended on B, and that also goes back as a delta.
      - `v1_sessions_send_whole_files`: v1 server, v1 client, and one v1 replica of two (each way). Files sync correctly with `deltas == 0` and whole-file traffic; a v2/v2 control sends the same changes as 2 deltas under 1 MiB.
      - `destination_changed_mid_assembly_commits_nothing` (`hooks` feature): A served, B local (the hook runs on the test thread). A user write lands in B's file at `delta.block`. The step is retried, the next round makes one conflict copy, both versions survive, and `assert_converged` finds no leftovers.
    - Unit tests (`replica::local`, `replica::delta`):
      - block lists, `read_blocks` (order, a bad index, a change mid-read, a stale file);
      - `apply_delta` rebuilding a changed, inserted and kept block;
      - a mid-assembly change at `delta.block` (reused block) and at `delta.assembled` (final check): `Err(Unstable)`, file = the user's, index unchanged, no leftovers, no open intents;
      - a stale precondition, misfit deltas, a wrong reused block, a bad received block, short or long data, the file changed since the scan.
    - Mutation-checked by hand (not committed): without the reused-block hash check, or without the final old-file check, a unit test fails.
  - **Harness:** in remote mode `Pair::over` sets `delta_min_size(0)`, so the T22 suites send every replaced file as a delta, races included (`Racing` fires `Read` at `read_blocks`, `Apply` at `apply_delta`). Counted once by hand: 58 sync cycles of the model test used deltas. New: `Pair::over_each(modes, versions)`, `Mode::wrap_with`, `Pair::traffic()`.
  - **Results:** `cargo test`, `cargo test --features hooks` (attack 15, crash 3, delta 3, model 2 × 256 cases, sync_once 36, symlink_matrix 22, …) and clippy (with and without `hooks`) pass; stress (`--release -- --ignored`) passes in both modes (72 s). Its daemon keeps the default 1 MiB threshold and its files are small, so the stress run sends no deltas; the delta races are covered by the suites above.
  - **Not done / follow-ups:**
    - No rolling hash: an insertion that is not a whole block shifts every later block, and they are all sent (as in syncthing).
    - Repeated blocks within the new file are each sent; the source's blocks are not deduplicated.
    - The delta request carries the full new block list (needed to verify reused blocks), so a 1-byte change in a 64 GiB file still costs about 3 × 16 MiB of lists.
    - `serve` and `sync` use v2 automatically; there is no CLI switch to force v1 (only the test API).

### [x] T25: Confirm the T23 fix with 50 crash-suite runs
- **Depends on:** T23
- **Read:** T23's Notes
- **Do:** run `for i in $(seq 50); do cargo test -q --features hooks --test crash || break; done` (~25 min; T23 stopped after 3 runs). On a failure, capture the failing case and `RUST_LOG=ffsl_sync=debug` output, and reopen T23.
- **Done when:** 50 consecutive runs pass, and the result is recorded in T23's Notes.
- **Notes:**
  - Ran on 2026-10-03, at commit `1f5c9e9` (after T26–T28), on WSL2 (kernel 6.18): **50 of 50 runs passed** (`crash`: 3 tests each, 26–43 s per run, 30 s on average, about 25 min in all). The loop would have stopped at the first failure and kept its output; there was none. Recorded in T23's Notes.

## M10: usability fixes (found while writing `docs/usage.md`)

### [x] T26: Exit quietly on a closed stdout (broken pipe)
- **Depends on:** —
- **Read:** —
- **Files:** `src/cli.rs` (maybe `src/main.rs`), `tests/cli.rs`
- **Do:**
  - Reproduce: `ffsl-sync status <pair> | head -3` prints three lines, then panics: `failed printing to stdout: Broken pipe (os error 32)`. Rust ignores `SIGPIPE`, so `println!` panics when the reader has gone. Every command that prints (`init`, `sync --once`, `status`, `serve`'s address line, `daemon`'s summary) is affected.
  - Make a closed stdout end the process quietly, with the conventional status (141, i.e. 128 + SIGPIPE, or 0; pick one and note it), and with no panic message. A suggested approach: print through `writeln!` on a locked stdout and treat `ErrorKind::BrokenPipe` as "stop printing, exit". Do **not** blindly reset `SIGPIPE` to `SIG_DFL` for the whole process: `serve`, `daemon` and a remote `sync` write to TCP sockets, and a peer that hangs up must remain a `Connection` error, not kill the process. If you do reset it, first verify that every socket write uses `MSG_NOSIGNAL` (std's `TcpStream` on Linux, and anything rustls writes through), and say so in Notes.
  - A daemon or `serve` whose stdout goes away must keep running. Only its final summary line is lost.
- **Done when:**
  - a `tests/cli.rs` test runs `status` (and `sync --once`) with stdout connected to a pipe whose reader is closed at once. The test asserts no panic (`panicked` is absent from stderr) and the exit status chosen above;
  - `cargo test` and clippy pass.
- **Notes:**
  - **Exit status: 141** (128 + SIGPIPE), what a shell shows for coreutils killed by the signal, so `set -o pipefail` treats truncated output the same way. Chosen over 0. Written down in design §2 ("Closed stdout") and `docs/usage.md` §9.
  - **How:** every `println!`/`print!` in `src/cli.rs` became `outln!`, which calls `cli::out`: `write_fmt` + `flush` on the locked stdout. The first failed write sets a flag and nothing more is printed; the command runs to its normal end (so `sync --once` still drains the quarantine and fails on an unconverged report, and a daemon or `serve` keeps running). `EPIPE` sets `stdout_closed()`; `main` (now returning `ExitCode`) then exits 141. Any other write error (e.g. `>/dev/full`) is logged once and `run` fails with "cannot write to stdout" (exit 1). `main` prints an error as `Error: {e:?}`, as before, through `writeln!` to stderr, so a closed stderr cannot panic either. `print_status` builds its header line in a string instead of two `print!`s.
  - **`SIGPIPE` is not reset:** it stays ignored (Rust's default), so socket writes get `EPIPE` and a peer that hangs up stays `Error::Connection`. No `MSG_NOSIGNAL` audit was needed.
  - `--help | head` was already fine: clap ignores its own print errors.
  - **Test:** `tests/cli.rs` `closed_stdout_exits_quietly` gives the child a pipe (`std::io::pipe`) whose reader is dropped before the spawn (deterministic, no race). `sync --once` and `status` exit 141 with no `panicked`/`Error` on stderr, and the sync happened; a `daemon` on such a stdout keeps syncing, stops on SIGTERM, and exits 141.
  - Checked by hand: `sync --once p | head -0` → 141, `status p | head -3` → three lines, 141; before the fix `status | head -3` panicked (exit 101).

### [x] T27: `status` on the host that serves a replica
- **Depends on:** —
- **Read:** §2 (Status and sandbox), §7.1 (Network as implemented: Deployment, Daemon and status over the network)
- **Files:** `src/status.rs`, `src/cli.rs`, `src/server.rs`, `docs/usage.md`, `tests/net.rs` or `tests/cli.rs`
- **Do:**
  - Reproduce (as in `docs/usage.md` §7): init a pair with `--b-remote 127.0.0.1:PORT` under one `XDG_STATE_HOME` ("laptop"), copy `config.toml` and B's `.crt`/`.key` to a second state home ("server"), run `serve paper b` there, then `sync --once` from the laptop. Then run `status paper` with the **server's** state home. It prints:
    - replica B as `served at 127.0.0.1:PORT (run \`status\` there)`, though this *is* that host, and its index `<B>.redb` is right there;
    - replica A (the laptop's root path) as a local replica with `0 entries`, `last sync: never`, because A's index is not on this host (`PairStatus::load` treats a missing index as "never synced").
  - The laptop's `status` is correct. The `remote` key describes the pair from the client's side, and the server's copy of the config is identical.
  - Make `status` useful on the serving host:
    - **Which replica is served here.** Decide how `status` knows. Options: (a) a remote replica whose index `<id>.redb` exists in this pair directory is served here (after the first `serve`; this is how the laptop/server split already shows on disk); (b) a `status --side a|b` flag. A key file alone does not work: `init` leaves both keys on the client host. Write the choice into §2.
    - **Its state while `serve` runs.** `serve` holds the redb lock, so reading the index fails with `DatabaseAlreadyOpen`, and only a daemon writes `status.toml` today. Have `serve` save a report as the daemon does: after each `RecordSync` and each quarantine sweep, for its own replica. `status` then reads it, as it does for a daemon.
    - **The other replica.** A replica that is local in the config but has no index on this host, while the peer is served here, is shown as "on the client host, not here" (or left out), not as an empty, never-synced replica.
  - Keep the plain local case unchanged: a fresh local pair still shows `never` before its first sync.
- **Done when:**
  - a test with two state homes (served B and a client, as above) checks the server-side `status`, both while `serve` runs (from its report) and after it stopped (from the index). It must show B's real entry count and last sync, and must not claim A was "never synced";
  - the existing `status` tests pass unchanged;
  - `docs/usage.md` §7 shows the server-side status. Its §12 row about `status` on the server host is removed;
  - `cargo test` and clippy pass.
- **Notes:**
  - **Which replica is served here: option (a).** A replica the config marks `remote` whose index `<id>.redb` is in this pair directory is served here (`serve` creates the index when it opens the replica; a client never writes an index for a remote replica). No `--side` flag. Before the first `serve` on a host, `status` there still shows the client's view. Written into design §2 ("`status` on the serving host").
  - **`serve` saves a report** (`status::ServedReport`: state, listen address, time) to `<pair>/status-<replica>.toml`: at start (so `status` works before the first sync), after every `RecordSync` (after its sweep, **before** answering, so the report is current when the client's `sync --once` returns; a test depends on that), and after every sweep in `ServerHandle::sweep`. Enabled by `Server::status_dir` (the CLI sets it; the test harness doesn't). A file per served replica, not `status.toml`, so a daemon and a `serve` sharing one state directory don't overwrite each other.
  - **`PairStatus::load`** now decides per replica: remote and not served here → "served at ADDR (run `status` there)" (unchanged); served here → from the index, or from the `serve` report if the index is locked; local without an index while the peer is served here → `on_client` ("on the client host, not here"); else as before (a missing index is "never"; a locked one takes that replica from the daemon's `status.toml`). `ReplicaStatus` has two new `#[serde(skip)]` fields, `served: Option<Served { addr, report_ns }>` and `on_client`, so `status.toml` keeps its format.
  - **Output** on the server: `served here on ADDR (`serve` running; its report from …)` or `served here at ADDR (`serve` not running)` (the config's address), then the usual index lines.
  - **Test:** `tests/net.rs` `status_on_the_serving_host` (two state homes): the server's `status` before any sync (from serve's first report), after a client sync while `serve` runs (B: 3 entries, a last-sync time, "served here on ADDR"; A: "on the client host", no entries, no "never"), and after `serve` stopped (same, from the index); the client's view is unchanged. The existing `status` tests pass unchanged.
  - **Docs:** `docs/usage.md` §7 has a new "Status on each host" part with the server-side output (rerun under `/tmp`); §2 lists the new file; §9 describes it; the §12 row is gone. README shows `status` on the server.

### [x] T28: Show delta transfers in the CLI output
- **Depends on:** T24
- **Read:** §7.1 (Block-level delta transfer: Engine, Traffic)
- **Files:** `src/cli.rs` (`print_report`, the daemon summary), `src/daemon.rs` (`DaemonStats`), `tests/net.rs` or `tests/cli.rs`, `docs/usage.md`
- **Do:**
  - Today nothing a user sees shows that a delta was used. `SyncReport::deltas` is counted but never printed, `DaemonStats` has no such field, and no log line mentions it. `docs/usage.md` §7 therefore cannot show it: its 1-byte-change example looks exactly like a whole-file transfer.
  - `sync --once`: when `report.deltas > 0`, print it in the summary line, e.g. `synced "paper": 1 change(s) applied in 1 round(s), 1 sent as delta`. Keep the line unchanged when it is 0, so local pairs print as before.
  - `daemon`: add `deltas` to `DaemonStats` (summed per cycle), print it in the stop summary when non-zero, and include it in the per-cycle `sync cycle done` log line (`executor`).
  - Optional, if cheap: the bytes moved by a remote replica (`RemoteReplica::traffic()`), at `debug` level.
- **Done when:**
  - a CLI-level test over loopback (`serve` + `sync --once`, as in `docs/usage.md` §7) changes 1 byte of a file above `DELTA_MIN_SIZE` and asserts `1 sent as delta` in the output. A local pair's output has no delta text;
  - `docs/usage.md` §7 shows the new line in its expected output. Rerun the guide's `sh` blocks under `/tmp` (see `CLAUDE.md`) and check the line matches;
  - `cargo test` and clippy pass.
- **Notes:**
  - **Output:** `sync --once` prints `synced "p": N change(s) applied in R round(s), D sent as delta` when D > 0, and the line is unchanged otherwise (`cli::deltas_note`). The daemon's stop summary gets the same `, D sent as delta` after the applied count (`DaemonStats::deltas`, summed per cycle). The `sync cycle done` info line logs `deltas`.
  - **Optional part done:** `sync --once` and `daemon` log each remote replica's `traffic()` (bytes sent/received) at `debug` when they end ("network traffic").
  - **Tests:** `tests/net.rs` `sync_once_reports_delta_transfers`: `serve` + `sync --once`, a 2 MiB file (create: no delta text), then 1 byte changed → `1 change(s) applied in 1 round(s), 1 sent as delta`, content equal. `tests/cli.rs` `local_sync_prints_no_delta`: the same change on a local pair prints the line without delta text.
  - **Docs:** `docs/usage.md` §7 shows `1 sent as delta` and explains it; §9 mentions it. The guide's `sh` blocks were rerun under `/tmp/ffsl-sync-demo` with the release binary: the line matches. (The §7 demo block that changed one byte no longer stops `serve`; the new status block after it does.)
  - **Results (T26–T28):** `cargo test`, `cargo test --features hooks` and clippy (with and without `hooks`) pass.

## M11: bug fixes from GitHub issues

### [x] T29: Refuse a replica root without its root marker (issue #1)
- **Depends on:** T28
- **Read:** §5.1 (Root marker), §5.8, §6.4 (Daemon), §9 (Harness)
- **Files:** `src/fs/tmpname.rs`, `src/fs/commit.rs` (`create_root_marker`), `src/index/store.rs` (meta `root_marker`), `src/replica/local.rs` (`ensure_root_marker`, `scan`), `src/error.rs`, tests that list roots, `docs/usage.md`
- **Do:**
  - [Issue #1](https://github.com/artem1579gh/ffsl-sync/issues/1): if a replica root is an empty directory (a disk that is not mounted leaves its empty mount point), every indexed path becomes a tombstone and the deletions propagate to the peer. `sync --once` exits 0, and the quarantine is drained, so the files are gone.
  - Put a marker `.~ffsl.root.<replica-id>` at the top of each root, as syncthing's `.stfolder` does. Refuse to open or scan a replica whose marker is missing once one was made. Give pairs made before markers a marker, unless their root is empty while the index lists live entries.
- **Done when:**
  - the issue's reproduction is a test (`tests/cli.rs`) that fails before the fix: the sync exits non-zero, A keeps its files and nothing is written to B. With the disk back, syncing works again;
  - unit tests cover a marker removed after open (scan and reopen refused, nothing indexed as deleted), a marker that is not a regular file, and a legacy index (refused on an empty root, marked on a non-empty one);
  - `docs/usage.md` explains the marker, the error and how to recover;
  - `cargo test`, `cargo test --features hooks` and clippy pass.
- **Notes:**
  - **Marker:** `fs::tmpname::root_marker(id)` = `.~ffsl.root.<16 hex>`, one per replica ID, so a directory can be a replica of several pairs. It sits in the reserved namespace, so the scanner and watcher ignore it and it is never synced. Only its existence as a regular file counts (`Root::stat`, no symlink followed). Its text says what it is.
  - **Creation** (`commit::create_root_marker`): `O_CREAT|O_EXCL|O_NOFOLLOW` at the root fd, never over anything; written, fsynced, root fsynced. It needs no journal: one cut short by a crash is still a marker. Hook points `marker.created`, `marker.written`.
  - **Index:** meta key `root_marker` = 1 once made or found (`IndexStore::root_marked`/`set_root_marked`). It is an additive key, so the schema version stays 2.
  - **Checks:**
    - `LocalReplica::open` runs `ensure_root_marker` after opening the index and before replaying the journal, so nothing is done in a wrong directory.
    - `Replica::scan` checks the marker before the scanner (nothing indexed) and after it (a root removed mid-scan fails the cycle before reconcile). A daemon whose root loses its marker stops with the error.
  - **Error:** `Error::RootMarkerMissing { path, marker, reason }`. Its message suggests mounting the disk, or `touch`ing the marker if it is the right directory. `docs/usage.md` §2 adds the third way out: delete the replica's index, so it starts over from the peer without deleting anything.
  - **Tests:** reserved-name checks (harness `walk`, `crash.rs`, `attack.rs`, `local.rs` `leftovers`, `cli.rs` `names`) leave out the root marker at the top of a root. `docs/usage.md`'s `find` listings leave out `.~ffsl.*`.
  - **Not done here** (suggested in the issue): a mass-deletion guard and a trash directory. They were done as T30 and T31.
  - **Results:** the issue's shell reproduction, run with release binaries: before the fix (2632a68), the second sync prints `5 change(s) applied`, exits 0 and leaves A empty. After it, the sync exits 1 with the marker error and A keeps its files. Upgrade checked by hand: a pair synced by the old binary is refused by the new one while B is an empty directory, and gets its markers once B is back (0 changes). `cargo test`, `cargo test --features hooks` and clippy (with and without `hooks`) pass. The guide's `sh` blocks were rerun under `/tmp/ffsl-sync-demo`; only the `--sandbox` block failed, because that kernel has no Landlock.

### [x] T30: Mass-deletion guard (issue #1)
- **Depends on:** T29
- **Read:** §6.1, §6.3, §6.4 (Mass-deletion guard)
- **Files:** `src/engine/executor.rs`, `src/config.rs`, `src/cli.rs`, `tests/sync_once.rs`, `tests/cli.rs`, `docs/usage.md`
- **Do:**
  - The issue's second suggestion, as unison's `confirmbigdeletes`: refuse a cycle that would delete more than some share of a replica's live entries, unless forced.
  - Decide what a daemon does when the guard trips.
- **Done when:**
  - scenario tests in both modes:
    - most of a replica deleted → held back, with the rest synced and nothing coming back on the deleting side;
    - held back again by the next cycle;
    - applied with the guard off;
    - small or partial deletions apply;
    - a directory replaced by a file counts with its contents;
  - a CLI test covers the output, the exit status and `--allow-mass-delete`. A config test covers the default and the range;
  - `cargo test`, `cargo test --features hooks`, clippy and the stress test pass.
- **Notes:**
  - **Rule:**
    - What counts as a deletion on side S (`executor::deletes_on`): a `Push` to S whose target is live, and whose entry is a tombstone or replaces a directory with a file or symlink.
    - The guard trips for S in a round with more than `MASS_DELETE_MIN` (10) such actions **and** more than `max_delete_percent` (default 50) of S's live entries (the root not counted).
    - Once tripped, it holds every deletion on S for the rest of the cycle (`Cycle::held`). Otherwise, steps applied in round 1 could bring the share under the limit in round 2.
    - Holding all of them keeps the plan consistent: an `Rmdir` is held with its children.
  - **Report:**
    - `SyncReport::held_back: Vec<HeldBack { side, live, paths }>`. It makes `is_converged` false, but it is not `unresolved`, so a daemon does not retry it after 1 s.
    - The `sync cycle done` log has `held_back`, and a `mass deletion held back` warning is logged per trip.
  - **Daemon decision:** it holds the deletions back, warns and keeps syncing everything else; it never stops for this. To apply them, stop it and run `sync --once --allow-mass-delete`. Each later cycle (a hint, or the 10-minute rescan) holds them back and warns again.
  - **Config:** `max_delete_percent` (`u8`, serde default 50, at most 100; 100 turns the guard off). Old configs load with 50. `Engine::max_delete_percent`.
  - **CLI:** `sync --once --allow-mass-delete` runs with 100. The summary lists the held-back count per replica, its live entries, the first 5 paths, `… and N more`, and the command to apply them. The command exits 1, with `N path(s) not synced`, which counts held-back paths.
  - **Tests:**
    - `tests/sync_once.rs`: `mass_deletion_is_held_back` and `small_or_partial_deletions_apply`, both modes.
    - `tests/cli.rs`: `mass_deletion_needs_allow_mass_delete`.
    - `config::tests::load_rejects_invalid_configs`: `max_delete_percent = 101` is rejected, and a missing key gives 50.
    - The stress test (both modes) passes with the guard on.
  - **Docs:** `docs/usage.md` §2 (with real output), §5, §9, §10, §12; README; design §6.4.

### [x] T31: Trash for replaced and deleted files (issue #1)
- **Depends on:** T29
- **Read:** §5.3 step 4(f) (Trash), §5.7, §5.8, §5.10
- **Files:** `src/fs/commit.rs` (`Quarantine`, `Trash`, `purge_trash`, `Ctx::base`), `src/fs/tmpname.rs`, `src/config.rs`, `src/replica/local.rs`, `tests/cli.rs`, `docs/usage.md`
- **Do:**
  - The issue's optional third suggestion: keep the files that a sync replaces or deletes in a trash directory, instead of unlinking them after the 400 ms quarantine.
- **Done when:**
  - unit tests cover:
    - replace, delete and rmdir go to the trash at their directory path, and a purge empties it by age;
    - a symlink in place of the trash or of one of its directories is never followed;
    - races at the trash's hook points;
  - a CLI test enables `trash_days` on one replica through the config file;
  - `cargo test`, `cargo test --features hooks` and clippy pass.
- **Notes:**
  - **Opt-in, per replica:** `ReplicaConfig::trash_days` (default 0 = no trash; `init` writes `trash_days = 0`). Per replica because the replicas may have different disks and needs (e.g. only the server keeps one). Old configs load with 0.
  - **Mechanism:**
    - The quarantine's **Unlink** verdict becomes a move into `.~ffsl.trash/<replica dir>/<stem~YYYYMMDD-HHMMSS.ext>` (`RENAME_NOREPLACE`, still under the lease).
    - Every earlier step is unchanged: grace, lease, and a changed inode becoming a conflict copy.
    - Trash directories are made with `mkdirat` and opened `O_PATH` beneath their parent with the `RESOLVE_*` flags. A symlink there fails the move, and the entry stays in quarantine.
    - `EXDEV` (beneath a `-K` link to another filesystem) unlinks as before. `NOENT` drops the entry.
  - **`Ctx::base`** (new field): the replica path of the commit's root, so that entries beneath a written-through `-K` link get their replica path in the trash. Set in `LocalReplica` (`Base::link`, or the root for replay) and in the tests.
  - **Purge:** `commit::purge_trash(root, older_than)` works by ctime (set by the move in), walks with fds without following anything, and removes directories left empty. `LocalReplica::sweep_quarantine` purges at most once an hour (`PURGE_EVERY`; the first sweep of each process purges). So does `serve`, through the same sweep.
  - **Hook points:** `trash.before_move`, `trash.before_rename`, `trash.before_purge`. They are outside `tests/attack.rs`'s prefixes, since its commit runs have no trash; `commit::tests::trash_races` attacks them instead. (`quarantine.before_trash` was first in the `quarantine` group and failed `every_hook_point_is_attacked`; renamed.)
  - **Tests:**
    - `commit::tests::trash_keeps_replaced_and_deleted_files`, `trash_symlink_is_not_followed`, `trash_races`; `tmpname::tests::trash_names`;
    - `tests/cli.rs` `trash_keeps_what_a_sync_replaces_or_deletes`. Its `names` helper leaves out the trash;
    - checked by hand over the network: with `trash_days` on a served B, the server put B's old version in B's trash.
  - **Docs:** `docs/usage.md` §2 (Trash), §10, §12; README; design §2, §5.3 step 4(f), §5.10, §9.

### [x] T32: Keep unsynced changes when an index is lost or restored (issue #2)
- **Depends on:** T31
- **Read:** §3 (Local change), §6.1, §6.4, §7, §7.1 (Protocol)
- **Files:** `src/index/store.rs` (`witness`), `src/replica/mod.rs` (`Replica::clock`, `witness`), `src/replica/local.rs`, `src/replica/remote.rs`, `src/replica/proto/`, `src/server.rs`, `src/engine/executor.rs` (`Cycle::sync_clocks`), `tests/harness/mod.rs`, `tests/index_reset.rs`, `docs/usage.md`, `README.md`
- **Do:**
  - [Issue #2](https://github.com/artem1579gh/ffsl-sync/issues/2): deleting `<replica-id>.redb` (or restoring an older copy) makes the next sync overwrite the replica's unsynced edits with the peer's older content, silently. The recreated index starts at `max_counter = 0`, so the rescan hands out counters the peer has already seen from this replica with higher values, and the peer's versions dominate. A file recreated on the reset side can likewise be deleted by an old tombstone.
- **Done when:**
  - the issue's reproduction is a test that fails before the fix, in both modes, plus: edits on both sides (conflict copy), a recreated file vs. the peer's tombstone, an index restored from a backup, and a protocol v2 server;
  - a unit test covers `IndexStore::witness`;
  - `cargo test`, `cargo test --features hooks` and clippy pass.
- **Notes:**
  - **Fix: clock exchange.** `Engine::sync` first runs `Cycle::sync_clocks`: it reads both replicas' Lamport clocks (`Replica::clock`, the store's `max_counter`) and raises the lower one to the higher (`Replica::witness` → `IndexStore::witness`, never lowering it, refusing a floor above `MAX_COUNTER`). It runs before any scan, so the reset side's new counters are above every counter the peer holds. No detection or refusal is needed, and no replica ID rotation (which would need both configs changed, and would leave a stale peer in the tombstone GC's `peers` table).
  - **Rejected:** refusing to sync on "the peer holds a counter for us above our clock". The interrupted conflict-winner write (§6.2: the winner's bump goes to the loser side first) makes the same state, so a refusal would have false positives.
  - **Warning:** `ensure_root_marker` warns when the root has its marker but the index has not recorded it: the index was recreated. A restored backup is not detected (it looks like a replica that is behind).
  - **Limit:** a restored index keeps the backup's vector for a path whose content still matches it; if the unsynced change was a revert to exactly that content, the peer's later version wins (design §6.4).
  - **Protocol v3:** `Request::Clock`, `Request::Witness { floor }` → `Response::Clock(u64)`, appended. On v1/v2 sessions `RemoteReplica::clock` uses the largest counter in its mirror, and `witness` does nothing, so a reset local side is still protected against an old server, but an old server's own index is not.
  - **Tests:** `tests/index_reset.rs` (4 scenarios × both modes, plus `lost_index_against_a_v2_server`); all 9 fail with `sync_clocks` disabled. `store::tests::witness_raises_the_clock_only`. Proto tests list the new variants and their tags (`newer_variants_are_appended`).
  - **Results:** the issue's shell reproduction, run with release binaries: before the fix (90b2ac2), both sides end with `v3`, no conflict copy, exit 0. After it, the sync warns that B's index is new, pushes `unsynced edit on B` to A (1 change applied) and exits 0.
  - **Docs:** `docs/usage.md` §2 (Lost or restored index), §12; README; design §3, §6.4 (Clock exchange), §7, §7.1.

### [x] T33: Keep a conflict winner's new edit made before it learns the merged vector (issue #3)
- **Depends on:** T32
- **Read:** §3 (Local change), §6.2, §6.3, §6.4 (Clock exchange)
- **Files:** `src/engine/reconcile.rs` (`resolve`), `src/engine/executor.rs` (`Engine::sync`, `Cycle::sync_clocks`), `src/engine/sim.rs`, `src/engine/plan.rs` (tests), `tests/harness/mod.rs` (`sync_racing_at`), `tests/conflict_clock.rs`, `tests/model.rs`, `docs/usage.md`
- **Do:**
  - [Issue #3](https://github.com/artem1579gh/ffsl-sync/issues/3): `resolve` bumped the merged vector in the **winner's** name, but only the loser records it; the winner learns it a round later (`SetMeta`). Until then the winner's clock is below that counter, so a local change on the winner in that window reuses it (`bump_after(winner, clock)`), the loser's vector (holding the winner's older content) dominates it, and the new edit is replaced without a conflict.
- **Done when:**
  - tests reproduce it, in both modes: an edit on the winner during the round after the resolution, and a cycle cut off after round 1 followed by an edit on the winner (also against a protocol v2 server);
  - the `sim` model checks that no index holds a counter in a replica's name above that replica's clock;
  - `cargo test`, `cargo test --features hooks` and clippy pass.
- **Notes:**
  - **Fix 1: the loser's bump.** `resolve` (conflicts and resurrections) bumps the merged vector in the loser's name. The loser records it with its own index write, so its clock covers it; a later change on the winner is concurrent with it, a conflict that keeps both versions. Every counter in a replica's name is now issued on that replica's index.
  - **Fix 2: Lamport receive before every rescan.** `Engine::sync` runs `Cycle::sync_clocks` before each rescan of dirty paths too, not only before the first scan. Defence in depth: the last round may have given a side counters (its own, e.g. a conflict copy's) its peer has not seen.
  - **Why the in-cycle case was rare:** the loser's conflict copy has a counter above the merged vector's, and its push to the winner in round 2 usually raised the winner's clock before the rescan. The data loss needed that push to fail too (the test touches B's copy while A reads it). A directory mode conflict has no copy, but an index-only `SetMeta` on a directory only checks the inode, so a racing `chmod` does not fail it (the next scan picks the new mode up; nothing is lost). Across cycles, T32's clock exchange already covered it, except against a v2 server (which cannot `witness`).
  - **Tests:** `tests/conflict_clock.rs` (2 scenarios × both modes, plus the v2 server). Before the fix, the in-cycle test fails in both modes and the v2 test fails; with only fix 1 the in-cycle test's counter check fails, with only fix 2 the v2 test fails. `sim`'s new invariant fails at once with the winner's bump. The harness's `sync_racing_at` races at the first path a predicate accepts (a conflict copy's name holds the time). Unit tests in `reconcile.rs` and `plan.rs` now expect the loser's bump.
  - **Also fixed:** `tests/model.rs` was flaky since T30: its reference model applies every deletion, but the engine's mass-deletion guard can hold them back (proptest found a case deleting all 11 entries, failing at HEAD too). It now runs with `max_delete_percent(100)`.
  - **Results:** `cargo test`, `cargo test --features hooks`, clippy (with and without `hooks`) and the release stress test (both modes) pass.
  - **Docs:** `docs/usage.md` §4; design §3, §6.2, §6.3, §6.4 (Clock exchange).

### [x] T34: Symlinks from an untrusted peer never lead out of the root (issue #4)
- **Depends on:** T33
- **Read:** §4.2, §4.3, §4.3.1, §4.4, §4.6, §7.1 (Server)
- **Files:** `src/config.rs` (`trust_peer_links`), `src/replica/local.rs` (`remote_peer`, `neutralizes_peer_links`, `neutralized`), `src/scan/scanner.rs` (`observe_link`), `src/server.rs` (`Server::new`), `src/replica/mod.rs` (`PairReplica::open`), `tests/harness/mod.rs`, `tests/peer_links.rs`, `tests/symlink_matrix.rs`, `docs/usage.md`, `README.md`
- **Do:**
  - [Issue #4](https://github.com/artem1579gh/ffsl-sync/issues/4): a replica creates every symlink its peer sends verbatim. Under a following policy (`copy-links`, `copy-unsafe-links`, `copy-dirlinks`) its next scan follows a planted `x -> /home/bob/.ssh/id_ed25519` out of the tree, and the peer reads the key with `OpenRead`. With `-K` and `keep_dirlinks_unsafe` the peer plants `d -> /home/bob/.ssh`, sends `Adopt { d }` and writes `d/authorized_keys` through it.
- **Done when:**
  - tests reproduce both attacks over the wire (loopback-remote mode), and they fail without the fix;
  - local pairs keep rsync's semantics (the matrix, local mode, unchanged);
  - `cargo test`, `cargo test --features hooks` and clippy pass.
- **Notes:**
  - **Trust:** a replica trusts its peer's links when the peer is local. It does not when the peer is reached over the network: `Server::new` and `PairReplica::open` (the local side of a pair with a remote replica) call `LocalReplica::remote_peer(true)`. The per-replica `trust_peer_links` (an `Option<bool>`, unset by default, left out of the TOML when unset) overrides that either way.
  - **Fix: munge, as rsync's daemon does.** A replica that does not trust its peer's links, could follow one out of its root (a following policy, or `-K` with unsafe), and does not munge anyway, writes a link whose canonical target is unsafe (§4.2) with the `/rsyncd-munged/` prefix (`raw_target` in the index). The same check covers `Op::Symlink`, the conflict copy of `followed_write = conflict`, and the links a materialize copies, which are judged at their new place, since materialize moves a link nearer the root. Safety is judged relative to the commit's root (a `-K` directory beneath one).
  - **Scanner:** a munged link whose unmunged target is the indexed symlink's target is indexed as that symlink, whatever the policy: never followed, never adopted, no echo. It is recognized by its target, not its inode, so materialized copies are recognized too. Without the index it is indexed literally, and still leads nowhere (fails closed). No index schema change.
  - **Considered and rejected:** marking peer-made links in the index as never-follow. It needs a schema change, and it fails open whenever the mark is lost (lost index, materialize, renames). Refusing the op instead of munging would turn every honest unsafe link into a per-cycle error.
  - **Behaviour change:** over the network, a following (or `-K` unsafe) replica gets an unsafe link as an inert symlink. That is the case for `copy-dirlinks` links to files, and for any `-K` link. The matrix's remote mode expects that (`arrived`), and its rsync comparison skips those cases. The harness's `synced()` view mirrors the scanner rule.
  - **Tests:** `tests/peer_links.rs` (7 scenarios × both modes): the read attack step by step over the protocol (with a forged `open_read`), the same through the engine, the adoption and write attack, a link beneath an adopted directory outside the root, the `trust_peer_links` override, safe targets kept, and a materialized link. Without the fix, 6 fail (every remote one that expects munging, plus the local override test). Unit tests: `local::tests::untrusted_peer_links_are_written_munged` (conflict copy, override, default by locality) and `config::tests::init_round_trips`.
  - **Results:** a manual run with release binaries (`serve` with B on `copy-links`, A with an absolute and a `..` link to outside files) leaves munged links on B, A's links intact, and nothing from outside on A. The second sync applies nothing, and `serve` logs a warning per link.
  - **Docs:** `docs/usage.md` §6 (Links from the other replica, the trust model), §7, §10; README; design §4.2, §4.3, §4.6 (new), §7.1.

### [x] T35: Resource limits for `serve` (issue #5)
- **Depends on:** T34
- **Read:** §7.1 (Framing, Batches, Server, Client connections)
- **Files:** `src/server.rs` (`Limits`, `HandshakeSlot`, `Deadline`, `record_sync_list`), `src/budget.rs` (new), `src/replica/proto/` (`DECODE_BUDGET`, `read_frame`), `src/fs/root.rs` (`RelPath` decoding), `src/index/vv.rs`, `src/index/store.rs` (`retain_current_tombstones`), `src/scan/scanner.rs` (`coalesce`, `Scope::bounded`, `ScanStats::bound`), `src/replica/local.rs` (`read_blocks`), `src/replica/remote.rs` (`scan`), `tests/server_limits.rs`, `tests/net.rs`, `docs/usage.md`, `README.md`
- **Do:**
  - [Issue #5](https://github.com/artem1579gh/ffsl-sync/issues/5): `serve` had no resource limits. (1) Before the handshake: a thread and 2 fds per connection, no cap, and a per-read handshake timeout, so ~500 connections or a slowloris client pushed `accept` into EMFILE. (2) `Apply`/`ApplyDelta` read the upload under the replica's lock with no timeout, so a stopped client froze every request and `serve` ignored the first SIGTERM (its loop waited in `sweep()`). (3) `MAX_FRAME` bounded bytes, not decoded memory (a 32 MiB frame of empty paths: ~768 MiB), and `RecordSync` batches piled up without limit. (4) `ReadBlocks` accepted duplicate indices. (5) `ScanStats` lists were unbounded, so a tree with ~500k unreadable paths broke `write_frame` on every cycle.
- **Done when:**
  - tests cover each item and fail with its mechanism disabled;
  - `cargo test`, `cargo test --features hooks` and clippy pass.
- **Notes:**
  - **(1) Connections:** `server::Limits` (defaults: 64 connections, 8 handshakes, 4 per IP, 10 s handshake deadline, 60 s request timeout; `Server::limits` overrides them, for tests; not in the config). Over a limit, the accept loop drops the connection before TLS (debug log for handshakes; warn for the total, which only the authenticated peer can reach). The deadline is absolute: a `Deadline` wrapper sets the socket timeout to the time left before each read and write of the TLS and protocol handshakes. Failed TLS handshakes log at debug (`tests/net.rs` now runs `serve` with `ffsl_sync::server=debug`).
  - **(2) Stalled client:** after the handshake, no timeout between requests (idle connections stay usable); from the moment a request arrives until it is answered, read/write timeouts of `request_timeout`. A stalled upload then fails the stream (`Broken`, so `drain` does not wait again), `apply` commits nothing and the lock is released. `ServerHandle::sweep` uses `try_lock`, so `serve` handles SIGTERM at once; `shutdown` closes the connections, which ends the upload. Staging uploads before the lock was rejected (a second copy of every file).
  - **(3) Memory:** `crate::budget`: a thread-local budget (`DECODE_BUDGET` = 64 MiB per frame) charged by list decoders (`budget::vec` via `deserialize_with` on every message `Vec`), spilled version-vector counters and `RelPath` allocations; postcard drops custom error messages, so `within` reports whether the budget ran out and `read_frame` says so. `RecordSync`: each batch keeps only the server's current tombstones (`IndexStore::retain_current_tombstones`, deduplicated by path), and a peer vector of more than 64 counters is a protocol error. No wire change, no protocol version bump.
  - **(4)** `LocalReplica::read_blocks` refuses a block twice (`InvalidOp`).
  - **(5) Lists:** `MAX_LISTED` = 10 000 paths, `MAX_LISTED_BYTES` = 4 MiB per list. `coalesce` cuts the paths at the greatest depth at which they fit (binary search over the depth; `covering` is linear after a component-wise sort, since the scanner's `outermost` hashes every prefix and is quadratic in depth), so every dirty path is still rescanned through an ancestor. Errors are cut short; the server warns with the count (its log has every path). The client bounds `Scope::Paths` the same way.
  - **Tests:** `tests/server_limits.rs` (flood of idle connections, slowloris handshake, connection cap, stalled upload vs. other requests, stalled upload vs. shutdown, `RecordSync` junk and a too-long vector). Disabling the deadline, the request timeout, the `try_lock` or the handshake slots fails the matching test. Unit tests: `budget::tests`, `proto::tests::decoded_size_is_bounded`, `vv::tests::spilled_counters_are_charged_to_the_decode_budget`, `store::tests::only_current_tombstones_are_retained`, `scanner::tests::long_lists_are_coalesced_or_cut_short`, and `local::tests::blocks_and_read_blocks_of_the_indexed_file` (a block twice).
  - **Results:** `cargo test`, `cargo test --features hooks` and clippy (with and without `hooks`) pass. One run of the full hooks suite failed `crash::crashes_at_every_hook_point` once ("rescan of … found changes"); it passed 6 runs in a row alone and in the full suite again. The crash suite uses local replicas only and reaches none of the changed code (the scanner's new functions serve the wire only), so it is taken as a timing flake; not investigated further. Manual run with the release binary: 500 idle connections from one host left `serve` at 14 fds (6 before: 4 handshake slots of 2 fds; the 496 others closed at once); a `sync` from the same host during the flood was refused (its IP's slots were taken), and worked once the 10 s deadline passed (6 fds again); SIGTERM stopped `serve` cleanly.
  - **Limit:** an attacker on the peer's own IP address (or one that keeps every handshake slot from many addresses) can still delay the peer's new connections while the flood lasts; its established connections are not affected. Firewalling the port remains the real fix.
  - **Docs:** `docs/usage.md` §7 (Limits of `serve`), §8, §12; README; design §7.1.

### [x] T36: Check the state directory and overlapping roots at every open (issue #6)
- **Depends on:** T35
- **Read:** §5.1, §7.1 (Identities, Deployment)
- **Files:** `src/fs/layout.rs` (new), `src/replica/local.rs` (`open_in_pair`, `open_checked`), `src/replica/mod.rs` (`PairReplica::open`), `src/server.rs` (`Server::open`), `src/config.rs` (`write_atomic`), `tests/layout.rs`, `docs/usage.md`
- **Do:**
  - [Issue #6](https://github.com/artem1579gh/ffsl-sync/issues/6): "state directory inside a root" and "overlapping roots" were checked only by `init`, lexically, and only for local replicas. Serving `$HOME` with the default `XDG_STATE_HOME` synced B's private key and index to the peer, and `serve`'s `status-<id>.toml`, rewritten after every `RecordSync`, started a new cycle forever. Symlinked layouts and loopback setups (one replica served on the same host) slipped through too; the config's temp names (`.config.toml.tmp.*`) were not reserved.
- **Done when:**
  - tests reproduce the served `$HOME`, a state directory behind a symlinked root, and overlapping roots over loopback (both ways), and fail without the fix;
  - `cargo test`, `cargo test --features hooks` and clippy pass.
- **Notes:**
  - **Fix:** `fs::layout::check`, run by `LocalReplica::open` (so also under `serve`) right after the root is opened, before the capability probe, the index or the root marker writes anything. It compares directories by `(st_dev, st_ino)` along the `..` chain of each (not paths, so symlinks hide nothing): the root must neither hold the pair's state directory nor lie inside it. `LocalReplica::open_in_pair` (used by `PairReplica::open` and `Server::open`) also checks the other replica's root: directly if this process opens it too; otherwise (a remote replica, or the client's replica under `serve`, whose configured path is on the client's host) only if its path here holds its root marker, so a namesake on another host is never compared. Its marker in one of the root's ancestors refuses the root too, whatever its configured path.
  - **First open:** a replica with no marker yet (never opened) is not found by a marker; the other side's next open is. In the loopback setup `serve` runs first, so the client finds B's marker in either direction (tested).
  - **Not covered:** a state directory reached through a bind mount whose source is inside the root (`..` at a mount's top leads to the mount point's parent).
  - **Also:** `write_atomic`'s temp files are `.~ffsl.<file>.tmp.<pid>.<n>` (reserved names).
  - **Tests:** `tests/layout.rs` (3 tests); all 3 fail without the fix (`serve` keeps running; the others sync). Unit tests `fs::layout::tests` (state directory either way, symlinked state home and root, local overlaps through a symlink, a remote root compared only with its marker, a marker that is a directory ignored).
  - **Results:** `cargo test`, `cargo test --features hooks` and clippy (with and without `hooks`) pass. Manual run: `serve` of a `$HOME` holding the default state home exits with "contains the state directory"; `sync --once` of A = `x` against B = `x/y` served on 127.0.0.1 exits with "contains …, the root of the other replica".
  - **Docs:** `docs/usage.md` §2 (State directory), §7, §12; design §5.1, §7.1.

### [x] T37: Each identity on its own host, plus small network hardening (issue #7)
- **Depends on:** T36
- **Read:** §7.1 (Identities, Who presents what, Deployment, Messages, Server)
- **Files:** `src/config.rs` (`init_with`, `identity_here`, `create_identity`, `pin_device`, `save_checked`), `src/tls.rs` (`Identity::generate`, `read`, `client_config`), `src/cli.rs` (`identity`, `pin`, `--sandbox`), `src/server.rs` (`tombstone_retention`, `RecordSync`), `src/error.rs` (`printable`), `src/replica/proto/` (`From<WireError>`, `Refused`), `tests/harness/mod.rs` (`Pair::tombstone_retention`), `tests/net.rs`, `tests/server_limits.rs`, `tests/cli.rs`, `tests/layout.rs`, `tests/sync_once.rs`, `docs/usage.md`, `README.md`
- **Do:**
  - [Issue #7](https://github.com/artem1579gh/ffsl-sync/issues/7): `init --b-remote` wrote B's private key on the client, which never uses it, and the guide had the user copy it (and, copying the whole state directory, A's key) to the server. Also: `RecordSync` used the client's `peer` and `retention`; the client sent SNI `ffsl-sync` and certificates carried the CN `ffsl-sync replica <id>`; the server's error messages and `Refused` reasons were printed raw (terminal escapes); `--sandbox` was accepted by `init` and ignored.
- **Done when:**
  - each key exists only on the host that presents it, and only the config and a device ID cross between hosts;
  - tests cover each item and fail without its fix;
  - `cargo test`, `cargo test --features hooks` and clippy pass.
- **Notes:**
  - **Identities:** `init` makes the identities its host needs (`identity_here`): a local replica's, and a remote one's only if both are remote (the client acts for each replica towards the other's server, so it needs both keys; unchanged). A remote replica with a local peer is left unpinned. On its server, `identity <pair> <side>` makes it (generate, save, pin in the config there) and prints its device ID and the `pin` command; `pin <pair> <side> <ID>` pins it on the client. `identity` never replaces an identity: run again it prints the same ID; one saved by a run interrupted before the config was written is pinned; a pinned replica whose identity is not here is refused ("made on another host"). `pin` is refused for a replica whose `.crt`/`.key` is on this host, and checks the config (not the other replica's device ID). Until pinned, `sync` and `serve` fail with `Error::Tls` saying what to run. Considered and rejected: `serve` making its identity on first start (the client would still need a pin step, and a typo'd side would silently make a key).
  - **Old pairs** keep working (no config change); the guide says to delete A's key from the server. Their certificates keep the old CN (pinned); a new pair replaces them. No key rotation command.
  - **`RecordSync`:** a `peer` other than the session's is a protocol error; `retention` is ignored, the server uses its own (`Server::tombstone_retention`, from the config in `Server::open`, default 30 days). The harness's `Pair::tombstone_retention` sets both engine and servers (two `sync_once` tests changed retention mid-test through the engine only). No wire change.
  - **Fingerprinting:** `enable_sni = false`; new certificates have an empty subject and no SAN (rustls/webpki accept them; the pinned verifier never looks). `Identity::generate` lost its unused replica argument. Not addressed: rcgen's default validity dates, the server's TLS behaviour itself.
  - **Escaping:** `error::printable` escapes control characters (C0, DEL, C1) and bidi controls as in Rust literals; applied in `From<WireError> for Error` and to `Refused` reasons. Idempotent, so a relayed error is not escaped twice. Paths were already escaped by `RelPath`'s `Display`.
  - **`--sandbox`** stays global (so `ffsl-sync --sandbox sync …` keeps working), but `init`, `identity` and `pin` refuse it.
  - **Tests:** `config::tests::identities_are_made_where_they_are_used`, `both_remote_identities_are_made_by_init`, `init_with_a_remote_replica` (no B key); `tests/net.rs` (`Hosts::new` now copies only the config, runs `identity` on the server and `pin` on the client, and checks each key is on its own host only; `a_served_replica_needs_its_own_identity_and_a_pin`); `tests/server_limits.rs::record_sync_takes_neither_peer_nor_retention_from_the_client` (fails without the server change: B collected its tombstone at the client's retention 0); `tls::tests::nothing_names_the_service` (the ClientHello and certificates; SNI turned back on as a positive control); `error::tests::printable_escapes_control_characters`, `proto::tests::remote_error_messages_are_printable`, a `Refused` reason with escapes in `proto::tests`; `tests/cli.rs::sandbox_is_refused_where_it_does_not_apply`.
  - **Results:** `cargo test`, `cargo test --features hooks` and clippy (with and without `hooks`) pass. Manual run of the guide's demo with the release binary: after `init` on the "laptop", `identity` on the "server" and `pin` on the "laptop", each state directory holds only its own replica's `.crt`/`.key`, the two `config.toml` are byte-identical, and sync and delta transfer work as before.
  - **Docs:** `docs/usage.md` §2, §7 (Deployment, Trying it on one machine, old pairs), §8, §9, §10, §12; README; design §7.1.

### [x] T38: Crash recovery through `-K` links and in moved directories (issue #8)
- **Depends on:** T37
- **Read:** §4.3.1, §5.3 (Step 1), §5.8, §9 (Crash tests)
- **Files:** `src/index/journal.rs` (`Intent::base`, `LegacyIntent`), `src/fs/commit.rs` (`Record::new`, `recover`), `src/replica/local.rs` (`replay`, `replay_one`, `locate`, `retry_orphans`, `scan`), `tests/crash.rs`, `docs/usage.md`
- **Do:**
  - [Issue #8](https://github.com/artem1579gh/ffsl-sync/issues/8): a commit through an adopted directory link (`-K`) resolves from the link's directory and journaled its path relative to it, but replay always resolved from the replica root, so the parent check failed ("directory replaced"). And any skipped intent was then marked done: its quarantined old inodes, moved-aside objects and staged files stayed forever under `.~ffsl.*` names, which the scanner ignores (a late write through a held fd never became a conflict copy). Without `-K` the same happened to a directory renamed between a crash and the restart, and in-process after a commit error (errors replay at once).
- **Done when:**
  - `tests/crash.rs` has `-K` scenarios, which fail without the fix;
  - an intent whose directory moved is replayed once a scan finds it, and is never marked done before;
  - `cargo test`, `cargo test --features hooks` and clippy pass.
- **Notes:**
  - **Journal:** `Intent::path` is now the replica path, and the new `Intent::base` the replica path of the directory the commit resolved from (the root, or the `-K` link written through). `base` is the last field, so a record from 0.1.8 or earlier fails to decode as the new layout and is read as a `LegacyIntent` (path as recorded, base = the root). No schema change; old journals replay (a legacy `-K` intent is found by identity, below).
  - **Replay:** `commit::recover` takes the place to look at (`at`, relative to `ctx.root`). `LocalReplica::locate` finds it: the recorded place, reached as the commit reached it (from the root as before; through the `-K` link via `Disk::route` on the index, with `base` matching); else beneath any indexed directory with the recorded parent's (dev, ino) (a linear pass over the index, only for intents not found at their place). Replay then runs with that base as `ctx.root`/`ctx.base`, so the quarantine and the trash see the right replica paths.
  - **Kept, not done:** an intent not found (or moved again while replaying) stays in the journal as it is, in `LocalReplica::orphans`, and is warned about once. `scan` retries the orphans after any scan that changed something, and rescans the directories they were replayed in (replay may put a user object back; otherwise the first scan would have recorded its absence). After a full scan with no dirty paths and no errors, an orphan still not found is forgotten with a warning: every directory of the tree is indexed then, so its directory, and its names, left the tree. A tree with an unreadable directory never completes a scan, so its orphans are retried (cheaply) after changed scans instead.
  - **Considered and rejected:** having the scanner adopt reserved names whose parent (dev, ino) matches an intent. The index already maps directories to their identity after a scan; the scanner stays unaware of the journal.
  - **Tests:** `tests/crash.rs` has three `-K` scenarios (B's link `k -> real` adopted; A replaces, creates beneath a new directory, and deletes through it). Their cycle syncs twice, since what B writes through `k` returns to A as `real/…`. `check_explained` resolves an intent's replica path on disk. 1313 cases in all. Without the fix, 227 of them fail (at `check_explained`; with that check off, as "reserved names left after replay"). Unit tests: `local::tests::replay_goes_through_keep_dirlinks_links` (a new intent and a legacy one), `local::tests::intents_wait_for_their_moved_directory` (a rename's moved-aside file in a directory renamed before the restart comes back after the first scan and is indexed; an intent of a removed directory is forgotten after it); both fail with the old replay (root only, skipped = done). `journal::tests::legacy_records_are_read`.
  - **Results:** `cargo test`, `cargo test --features hooks` and clippy (with and without `hooks`) pass. No manual run with the binary: a crash at a chosen step cannot be produced by hand, and the crash suite aborts at every hook point.
  - **Docs:** `docs/usage.md` §12 (`.~ffsl.*` files after a crash); design §5.3 step 1, §5.8, §9.

### [x] T39: Materializing a followed directory keeps the peer's copies of unmanaged children (issue #9)
- **Depends on:** T38
- **Read:** §4.3.1 (`-L` write-back), §4.5 (Dangling links, Loops)
- **Files:** `src/replica/local.rs` (`materialize`, `put_entries`), `tests/symlink_matrix.rs`, `docs/usage.md`
- **Do:**
  - [Issue #9](https://github.com/artem1579gh/ffsl-sync/issues/9): with `-L` and `followed_write = replace`, materializing a followed directory left its `Unmanaged` entries (a dangling link, a loop where a directory used to be) out of the copy, and the rescan then tombstoned them with a dominating vector. The next cycle deleted the peer's copies, which "unmanaged is not a deletion" had kept (for a loop, the whole subtree: its children were tombstones already, skipped only while their ancestor was unmanaged).
- **Done when:**
  - a test reproduces the scenario of the issue (dangling child and loop child, local and remote), and fails without the fix;
  - `cargo test`, `cargo test --features hooks` and clippy pass.
- **Notes:**
  - **Fix:** `materialize` leaves out every `Unmanaged` entry beneath the link and everything the index has beneath one, and, just before the commit, stores them as tombstones with an **empty** vv: "never seen", dominated by any live version, so the peer's copies are pushed back into the new real directory (rsync `-L` from the peer would copy them too). If the commit does not replace the link (precondition failed, preserved, error), the old entries are put back. Writing them before the commit, not after, closes the crash window: a crash after the exchange but before the rescan would otherwise leave them to the next scan's tombstones. A crash before the exchange leaves the link, whose unmanaged objects the next scan indexes again, with fresh versions (only a later conflict instead of a win, never a loss).
  - **Considered and rejected:** removing the entries (the remote mirror only learns of removals through tombstone collection; T35's `changes_since` invariant); declining (conflict copies) while unmanaged children exist (every peer change beneath the link would become a conflict copy for as long as the dangling link exists); changing the scanner so a vanished unmanaged entry never becomes a tombstone (broader: it also changes what deleting a FIFO or a dangling link means).
  - **Not changed:** what the link pointed to (`d/x`, `d/sub`) stays unmanaged on B, so the pair is not fully converged there, as before.
  - **Tests:** `tests/symlink_matrix.rs::materialize_keeps_unmanaged_children` (local and remote: B's `l -> d` with `d/x -> ../f` dangling and `d/sub -> ..` a loop; A changes `l/y`; A keeps `l/x` and `l/sub/z`, B gets them, a further sync applies nothing); fails without the fix (A's `l/x` deleted). `local::tests::unmanaged_beneath_a_materialized_link_is_forgotten` (a failed materialize leaves the entries as they were, which fails without the put back; a successful one leaves empty-vv tombstones and the next scan changes nothing).
  - **Results:** `cargo test`, `cargo test --features hooks` and clippy (with and without `hooks`) pass. Manual run with the release binary (the test's scenario, a local pair, `copy-links` on both): B's `l` became a real directory holding `l/x`, `l/sub/z` and A's `l/y`; A kept its copies; the next sync applied nothing.
  - **Docs:** `docs/usage.md` §6; design §4.3.1.

### [x] T40: A refused lease keeps a writer's old file; the commit's lease checks ctime (issue #10)
- **Depends on:** T39
- **Read:** §5.3 step 4(b), (d), (f), §5.7, §5.10
- **Files:** `src/fs/lease.rs` (`Lease::take`, `NoLease`), `src/fs/commit.rs` (`take_lease`, `Quarantine`, `Pending::verdict`), `src/fs/caps.rs`, `src/fs/stat.rs` (test), `docs/usage.md`
- **Do:**
  - [Issue #10](https://github.com/artem1579gh/ffsl-sync/issues/10): the quarantine handled a refused lease like "leases unavailable" and unlinked the old inode after the 400 ms grace period, although a refusal proves someone still has the file open. An editor, database or logger writing 2 s after the replace lost that write silently; `settle_dir` (rmdir) did the same with no grace at all.
  - Related: the commit's lease only checked the inode, so a write between the pin and the lease that put the mtime back went unseen in large (unrehashed) files.
- **Done when:**
  - a quarantined file a writer still has open is kept past its grace period, becomes a conflict copy if written to or still open after a cap, and is unlinked once the writer closes it; rmdir makes it a conflict copy;
  - a write between the pin and the lease is caught by ctime;
  - tests for both fail without the fix; `cargo test`, `cargo test --features hooks` and clippy pass.
- **Notes:**
  - **Lease:** `Lease::take` tries `F_WRLCK`, then (on `EAGAIN`) `F_RDLCK`, and returns `Err(NoLease::Writer)` when both are refused (someone has the file open for writing), `Err(NoLease::Unavailable)` otherwise. A read lease is enough everywhere: it is broken by an open for writing or a truncate, and a writable mmap needs a writable fd. `held()` compares `F_GETLEASE` with the type taken. A file only readers have open is now leased too (it used to wait for its grace period).
  - **Quarantine:** an entry past its grace period whose lease is refused by a writer is kept (`Pending::writer_seen`), looked at again every `WRITER_POLL` (1 s; part of `next_deadline`, so the daemon and `serve` wake for it, without a busy loop), unlinked under a lease once the writer closes it, and turned into a conflict copy if written to or still open `writer_wait` after the replace (`DEFAULT_WRITER_WAIT`, 60 s; `set_writer_wait`). `settle_dir` turns a writer's entry into a conflict copy at once, which keeps the directory. Without leases (unsupported filesystem, not the owner), the grace period applies as before.
  - **Commit:** `take_lease` requires the leased file to be `unchanged` against the pinned fingerprint (ctime included), else `PreconditionFailed("changed since scan")` (replace and delete). A held lease at (d) then proves no write since the pin, whatever the size. The rehash of racy and small files stays: timestamps are coarse on older kernels, so ctime alone does not catch a write in the same tick as the file's previous ctime change.
  - **Considered and rejected:** a read lease only (no write lease first): equivalent for our purpose, but the write lease is what T18 tested; trying it first changes nothing where nobody has the file open. Making `sync --once` settle writer-held entries as conflict copies when it stops draining: its replay on the next run finds them (a write meanwhile makes a conflict copy), so nothing is lost by leaving them.
  - **Tests:** `commit::tests::a_writer_keeps_the_old_file_past_its_grace_period`, `a_writer_open_too_long_gets_a_conflict_copy`, `rmdir_keeps_a_child_open_for_writing`, `a_write_before_the_lease_is_seen_by_its_ctime` (replace and delete; a 1 MiB + 1 file rewritten in place at `*.pinned` with its mtime put back); all four fail without the fix. `open_fd_prevents_the_leased_unlink_until_closed` now holds a writer's fd, and checks that a reader's fd no longer delays the unlink. `lease::tests::taken_only_without_other_open_fds` (read lease, `NoLease`), `a_read_lease_is_broken_by_a_writer`.
  - **Results:** `cargo test`, `cargo test --features hooks` and clippy (with and without `hooks`) pass. Manual run with the release binary (a local pair on xfs): with `exec 3>>b/log.txt` open, a change from A replaced B's `log.txt`; `sync --once` stopped draining after 4 s and left `.~ffsl.old.*` (warned). After `echo "late write" >&3`, the next run's replay made it `log.sync-conflict-….txt` holding `v1` and `late write`, and the sync after that copied it to A; one more sync applied nothing.
  - **Docs:** `docs/usage.md` §2 (Trash), §4 (a program writing while a sync replaces); design §5.3 step 4(b), (d), (f), §5.7, §5.10.

### [x] T41: The quarantine stays within the open file limit (issue #11)
- **Depends on:** T40
- **Read:** §5.3 step 4(f), §5.8
- **Files:** `src/fs/commit.rs` (`Quarantine`, `Pending`, `SweepReport`), `src/replica/local.rs` (`apply`, `replay`, `make_room`), `src/cli.rs` (`raise_fd_limit`), `Cargo.toml` (rustix `process`), `tests/cli.rs`, `docs/usage.md`
- **Do:**
  - [Issue #11](https://github.com/artem1579gh/ffsl-sync/issues/11): each quarantined old inode held 2 fds (a duplicate of its directory's and its pin) until the sweep after the cycle, and nothing raised the soft `RLIMIT_NOFILE` (often 1024). A cycle that replaced a few hundred files hit EMFILE, and every later commit failed. A failed dup in `Quarantine::add` after the rename to the quarantine name left the old inode there untracked until a restart (its replay failed too). The startup replay of many intents ran into the same limit.
- **Done when:**
  - a cycle that replaces many more files than the limit allows for still converges, and a test for it fails without the fix;
  - `cargo test`, `cargo test --features hooks` and clippy pass.
- **Notes:**
  - **Shared directory fd:** `Pending::parent` is an `Arc<OwnedFd>`, shared by the entries of one directory (found by (dev, ino); an entry's fd keeps that inode alive, so its number is not reused).
  - **Cap:** `Quarantine::make_room`, called by `LocalReplica::apply` (before each op, so delta writes and materializes too) and by `replay` before each intent, sweeps while the quarantine holds `max` entries (default: soft `RLIMIT_NOFILE` / 8, 16 to 65536; `set_max`). Entries past their grace period are settled at once (`Due::Now`), so a writer's becomes a conflict copy then, without waiting for `writer_wait`. Entries in their grace period are unlinked under a lease if one can be taken. While still full, it sleeps until the next entry's grace period ends, at most `MAX_ROOM_WAIT` (10 s), then warns and goes on. Settled intents are forgotten in the journal as after a sweep. In `replay`, an entry swept before the final `finish` is skipped there, since its record is gone.
  - **Limit:** `cli::run` raises the soft `RLIMIT_NOFILE` to the hard limit before anything is opened, so the cap is computed from the raised limit.
  - **Order in `add`:** the dup of the directory fd (or the shared one) and the replica path come before the rename to the quarantine name. After the rename, nothing can fail.
  - **Considered and rejected:** dropping the pin, which keeps the inode number from being reused while it is compared (needed for race-freedom); unlinking entries in their grace period early when the quarantine is full, which would cut the writer protection short without leases. A hook point in `make_room` was dropped: it is not a commit step, and the attack suite requires every hook point in `commit.rs` to be attacked.
  - **Tests:** `tests/cli.rs::a_large_cycle_stays_within_the_open_file_limit` runs `sync --once` with soft and hard `RLIMIT_NOFILE` at 256 and 600 replaced files in 3 directories. Without the fix, 478 paths are not synced. `commit::tests::quarantine_entries_share_their_directory_fd`, `make_room_waits_for_the_grace_period`, `make_room_unlinks_leased_entries_at_once`, `make_room_does_not_wait_beyond_its_limit` and `make_room_keeps_a_writers_file_as_a_conflict_copy`.
  - **Results:** `cargo test`, `cargo test --features hooks`, clippy (with and without `hooks`) and the release stress test pass. Manual run of the issue's scenario with the release binary (a local pair on xfs): 2,000 files in 20 directories, all changed in A, then `sync --once` under `ulimit -n 1024` (soft and hard). All 2,000 were applied with no errors, and no `.~ffsl.old.*` was left. The next sync applied nothing.
  - **Docs:** `docs/usage.md` §4, §12; design §5.3 step 4(f).

### [x] T42: The daemon recovers by itself: watches after an overflow, stuck paths, transient errors (issue #12)
- **Depends on:** T41
- **Read:** §3 (racily clean entries, scanner errors), §5.9, §6.4 (Daemon)
- **Files:** `src/watch/inotify.rs` (`rewatch`), `src/fs/stat.rs` (`mount_point`, `StableReader::on_stale_hash`), `src/fs/root.rs` (`open_referent`), `src/replica/local.rs` (`mark_stale`, `Disk::open_file`), `src/daemon.rs` (`Retry`, `run_reopening`), `src/error.rs` (`Error::may_pass`), `src/cli.rs`, `src/scan/scanner.rs` (test), `tests/daemon.rs`, `docs/usage.md`
- **Do:**
  - [Issue #12](https://github.com/artem1579gh/ffsl-sync/issues/12): three ways the daemon degraded and never recovered.
  - `IN_Q_OVERFLOW` only triggered a full rescan. Directories made while the events were lost, and those a watch failed on after `ENOSPC`, stayed unwatched until a restart.
  - Some paths stayed dirty forever and were rescanned every second: a single-file bind mount (`EXDEV` on the read open, taken for a race), a followed link whose path is longer than `PATH_MAX` (`ENAMETOOLONG` in the in-tree lookup became `EAGAIN`), and a stale hash behind an unchanged fingerprint (every `open_read` failed at EOF, and the scan's shortcut never rehashed).
  - Any failed cycle other than a lost connection ended the daemon, e.g. `EIO`, or `ENOSPC` in the state directory.
- **Done when:**
  - after an overflow, and at each poll while the watch limit was hit, the whole tree is watched again;
  - none of the three paths is retried every second: the first two are errors, the third is rehashed; a path left unresolved is retried with backoff;
  - a cycle that fails with an error that may pass makes the daemon open the replicas again and go on; a corrupt index still ends it;
  - tests for each fail without the fix; `cargo test`, `cargo test --features hooks` and clippy pass.
- **Notes:**
  - **Watches:** `InotifySource::rewatch` runs `watch_tree(root)` on `IN_Q_OVERFLOW` and at each `LIMIT_POLL`. It is idempotent per inode. A rewatch that hits no `ENOSPC` (`limit_hit`) ends the polling and logs `every directory is watched again`. Stale watches of directories that left the tree during the lost window are not pruned: they can only report under a path that no longer exists, which a scan finds already synced.
  - **Errors, not dirty:** `read_once` and `open_err` map `EXDEV` (opening a single name beneath its directory: only a mount point) to an I/O error, `mount point inside the replica; not synced`, as `observe_dir` does for a directory. `Root::open_referent` returns the in-tree lookup's `ENAMETOOLONG` as it is, instead of comparing it with the direct route; it then becomes an I/O error everywhere.
  - **Stale hash:** `StableReader::on_stale_hash` runs when every other EOF check passed but the hash differs. `Disk::open_file` (so `open_read`, `blocks`, materialize and the metadata copy) records the path with its indexed hash in `LocalReplica::stale` (an `Arc<Mutex<…>>`: `serve` reads on other threads). `LocalReplica::scan` first marks those entries `racy` (`mark_stale`, `put_local`), if they still have that hash. The engine rescans the path after the failed step, so the next round rehashes and pushes it.
  - **Retry backoff:** `Daemon::run` keeps a `Retry { at, tries }` per unresolved path. The delay is `retry_delay · 2^(tries−1)`, at most `MAX_RETRY_DELAY` (5 min). A path no cycle leaves unresolved any more is dropped (every cycle reconciles the whole indexes). Due paths get a provisional next time when taken, so a cycle that fails with a lost connection does not loop.
  - **Reopening:** `Daemon::run_reopening(replicas, open, close, stop)`, used by the CLI. After an error with `Error::may_pass` (an I/O error, redb's `Io`, `PreviousIo` or `DatabaseAlreadyOpen`, a lost connection, a remote error other than a protocol error), it hands the replicas to `close` (the CLI drains the quarantines and drops them), waits `RESTART_DELAY` (5 s; doubling up to `MAX_RESTART_DELAY`, 5 min, while no cycle completes in between; `stop` ends the wait), and opens them again, retrying an open that fails with an error that may pass. Opening again is what makes `ENOSPC` recoverable: redb latches a failed write and answers `PreviousIo` until the database is reopened. Opening replays the journal, as after a crash. `BadIndex`, other `Db` errors, a missing root marker (documented: such a daemon stops) and protocol errors still end it. The first open's failure is the command's, as before.
  - **Considered and rejected:** reopening only the index inside `LocalReplica` (the redb lock forbids opening the new database before the old one is dropped, and a failed reopen would leave the replica without an index); retrying a missing root marker (documented to stop the daemon). `serve` does not reopen its index after such an error yet; its client daemon retries with backoff until `serve` is restarted.
  - **Tests:** `watch::inotify::tests::overflow_watches_the_tree_again`, `limit_poll_watches_the_tree_again`; `fs::stat::tests::a_mount_point_is_an_io_error`, `stale_hash_is_reported`; `scan::scanner::tests::followed_link_with_a_too_long_path_is_an_error` (fault hook `root.in_tree_lookup`); `replica::local::tests::a_stale_hash_is_rehashed_after_a_read`; `tests/daemon.rs::a_transient_failure_reopens_the_replicas` (scans fail with `PreviousIo` until reopened), `a_bad_index_ends_the_daemon`, `a_path_that_stays_unresolved_is_retried_less_often` (a `Flaky` wrapper replica). Each fails without its fix (`stale_hash_is_reported` and the first two daemon tests use new API).
  - **Results:** `cargo test`, `cargo test --features hooks`, clippy (with and without `hooks`) and the release stress test pass. Manual runs with the release binary in a user and mount namespace (`unshare -rm`, a local pair on xfs). (1) A file bind-mounted into A: one warning `mount point inside the replica; not synced` and one cycle in 4.5 s. Version 0.1.12 ran a cycle every second (5 of them). (2) The state directory on a 4 MiB tmpfs, filled up while 200 new files arrived: the daemon logged `sync cycle failed; opening the replicas again error=index database: I/O error: No space left on device`, opened the replicas again after 5 s once space was freed, and synced all 220 files. Version 0.1.12 exited with status 1, leaving 200 files unsynced.
  - **Docs:** `docs/usage.md` §5 (how the daemon recovers), §11 (a bind-mounted file), §12; design §3, §5.9, §6.4 (Daemon).

### [x] T43: The commit path keeps a concurrent chmod, handles read-only directories, and fsyncs what the index records (issue #13)
- **Depends on:** T42
- **Read:** §5.3 step 4(d)–(f), §5.4, §5.8, §6.3
- **Files:** `src/fs/commit.rs` (`unmodified`, `verify_old`, `remove_tree`, `open_own_dir`, `fsync_dir`, `Target::fsync_parent`, `set_dir_mode`, `set_referent_mode`, `stage_dir`, `fill_tree`, `undo`, `restore`, `put_back`, `Pending::sweep`), `src/engine/plan.rs` (`PhaseKind::DirModes`, `Planner::create`, `locks_out`), `src/engine/sim.rs`, `tests/sync_once.rs`, `docs/usage.md`
- **Do:**
  - [Issue #13](https://github.com/artem1579gh/ffsl-sync/issues/13): a chmod between the pin and the exchange was overwritten. `same_object` ignores the mode, the ctime a chmod bumps is not compared after a rename, and a lease does not see it.
  - Read-only directories as a non-root user: a 0555 directory from the peer was made with its final mode, so its children failed with `EACCES`. A staged copy holding one could not be removed (`remove_tree`, also in replay), leaking a `.~ffsl.<id>` tree.
  - Missing fsyncs: `set_dir_mode` and `set_referent_mode` did not fsync the directory they changed. Undo, restore, conflict copies (commit, replay, quarantine) did not fsync the parent, though the index then records the path as handled.
- **Done when:**
  - a chmod before the exchange (replace) or the move aside (delete) is kept, and the commit fails its precondition;
  - a new read-only directory gets its children, then its mode; so does one made read-only in the cycle that adds a child; an undone materialize of read-only directories leaves nothing behind;
  - tests for each fail without the fix; `cargo test`, `cargo test --features hooks` and clippy pass.
- **Notes:**
  - **Mode:** `verify_old` (replace, delete, rename, replay) requires `unmodified`: `same_object` plus the same mode. `same_object` itself is unchanged: comparing the mode of our own staged object (step 5, undo) would turn a user's chmod of our new file into an `Unstable` or a conflict copy, for no data at stake.
  - **Planner:** a fourth phase, `DirModes`, depth descending, after `Creates`. A directory mode without `u+rwx` (`locks_out`) is set there: a `SetMeta` of an existing directory moves there; a new directory is a `Mkdir` with `mode | 0o700` and an **empty** version vector in `Creates`, plus a `SetMeta` with its mode and vector in `DirModes` (precondition: a directory with the empty vector). With the pushed vector instead, an interrupted cycle would leave equal vectors with different modes, which reconcile reads as inconsistent (a rescan every cycle); with the empty one, the pushed entry dominates and the next cycle sets the mode. A mode change that grants the owner access stays in `Creates`, before the children.
  - **Removing our trees:** `remove_tree` opens each directory with `open_own_dir`, which makes it 0700 first. One without read permission (`EACCES` on `O_RDONLY`) is pinned `O_PATH|O_NOFOLLOW` and chmodded through `/proc/self/fd/N` (`fchmod` refuses an `O_PATH` fd; rustix has no `fchmodat2`), then opened as `.` beneath the pin. The same `/proc` route `TempFile::finish` uses for `linkat` without `AT_EMPTY_PATH`.
  - **fsyncs:** the pin after `fchmod` in `set_dir_mode`, `set_referent_mode`, `stage_dir` and `materialize`; each staged directory in `fill_tree`; the parent after `undo`'s exchange back, a successful `restore`, `Target::preserve`, replay's exchange back (`put_back`), and the quarantine's conflict rename (its failure is logged, not returned: the rename is done, and retrying the entry would find it gone). The new fsyncs have no hook point: they are not step boundaries, and the attack suite requires every hook point to be attacked. No test simulates a power loss.
  - **Considered and rejected:** creating every directory 0700 and setting every mode last, as the issue suggests: one more commit per directory, and a directory without its synced mode for the whole cycle; only modes without `u+rwx` need it. Making a directory that is **already** read-only on the receiving side writable for our own children: that is an in-place change of the user's directory, racing with the user's own chmod. Documented in `docs/usage.md` §12 instead. Re-checking a directory's mode after `set_dir_mode`'s `fchmod`: a chmod in between cannot be undone without another in-place change. Fsyncing after a quarantine unlink or trash move: a reverted one leaves only a reserved name behind, at the cost of an fsync per replaced file.
  - **Tests:** `commit::tests::chmod_before_the_exchange_is_kept` (replace and delete, with and without leases), `undone_materialize_removes_read_only_directories` (0555 and 0311 directories, with `O_TMPFILE` and named temp files); `engine::plan::tests::modes_that_lock_the_owner_out_come_last`; `tests/sync_once.rs::read_only_directories` (local and remote). All four fail without the fix. The model test (`engine::sim`) also picks 0555 and 0500 for directory modes now.
  - **Results:** `cargo test`, `cargo test --features hooks`, clippy (with and without `hooks`) and the release stress test pass. Manual run with the release binary (a local pair on xfs, not root): `ro` 0500 holding `ro/sub` 0555 and files synced in one cycle (8 changes). Then `w/new` added and `w` made 0555 in A: synced in one cycle (2 changes), B has 500, 555 and 555. The next sync applied nothing.
  - **Docs:** `docs/usage.md` §4 (a chmod during a sync), §11 (read-only directories), §12 (a directory already read-only on the receiving side); design §5.3 step 4(d), (e), (f), §5.4, §5.8, §6.3.

### [x] T44: Small correctness fixes: counter limit, tombstone GC, munge prefix, reserved conflict names, deep trees, followed referents (issue #14)
- **Depends on:** T43
- **Read:** §3 (version vectors, scanner errors, tombstone GC), §4.4, §4.5, §6.2
- **Files:** `src/index/vv.rs` (`within_limit`, `can_bump`), `src/index/store.rs` (`encode`, `record_sync`), `src/replica/mod.rs` (`Op::vv`, `Op::check_counters`), `src/replica/local.rs` (`apply`, `decline`, `open_indexed`), `src/replica/remote.rs`, `src/scan/scanner.rs` (`walk_dir`, `visit`, `MAX_PATH_LEN`, `follow_file`, `flush`), `src/fs/root.rs` (`reopen_file`), `src/symlink/munge.rs`, `src/fs/tmpname.rs` (`with_marker`), `docs/usage.md`
- **Do:**
  - [Issue #14](https://github.com/artem1579gh/ffsl-sync/issues/14), six small items.
  - Counters above `MAX_COUNTER` (2^62) could be written but not read back: a peer at the limit made the next local bump 2^62 + 1, and every later `changes_since` failed with `BadIndex`.
  - A retired peer ID in `peers` kept every tombstone for ever, since every listed peer had to acknowledge.
  - A link to exactly `/rsyncd-munged/` in a munging replica got an empty canonical target, which the peer's `stage_symlink` refused on every cycle.
  - The conflict copy of a file named `.~ffsl` got the reserved name `.~ffsl.sync-conflict-…`, which the scanner hides and `rename_to` refuses.
  - The scanner recursed, one fd and several stack frames per level: a deep tree could overflow a `serve` thread's 2 MiB stack, and paths longer than `PATH_MAX` were indexed but could never be opened.
  - A followed file was opened `O_RDONLY` by a second lookup after the `O_PATH` classification; a chain swapped in between could make the daemon (as root) open a device node.
- **Done when:**
  - no counter above the limit is ever written, and an op or scan that would write one fails for its path only;
  - a former peer no longer keeps tombstones;
  - the prefix alone is synced as a plain target; a conflict copy never gets a reserved name;
  - a deep tree scans in a small stack, and a path over `PATH_MAX` is an error;
  - a followed file is read through the fd that classified it;
  - tests for each fail without the fix; `cargo test`, `cargo test --features hooks` and clippy pass.
- **Notes:**
  - **Counters:** `encode` refuses a vector with a counter above `MAX_COUNTER` (`BadIndex`, nothing written: a backstop). `Op::check_counters(path, clock)` refuses an op (`InvalidOp`) when `max(op vv, clock) > MAX_COUNTER − 2` (`BUMPS_PER_OP`: a conflict rename and a declined op bump twice), before anything is committed: `LocalReplica::apply` with its index's clock, `RemoteReplica::apply`/`apply_delta` with 0 before sending (the server could not decode it, a protocol error). The scanner's `flush` reports a path it cannot bump (`!vv.can_bump(clock)`) as an error and leaves its entry. Only a broken or hostile peer gets there; the pair then stops recording changes with an error per path, instead of corrupting the index. `decline` uses `Op::vv`.
  - **Tombstone GC:** a pair has two replicas, so an index has one peer. `record_sync(peer)` removes every other ID from `peers`, with its acks (logged at info), and a tombstone needs only `peer`'s ack. The test `every_known_peer_must_acknowledge` encoded the old rule and was replaced by `a_retired_peer_does_not_keep_tombstones`.
  - **Munge:** `is_munged` requires bytes after the prefix, and `unmunge` strips only a munged target. The prefix alone is synced as it is (like any unmunged link; the scanner warns) and is written back munged twice by a munging replica. The bijection holds for non-empty canonical targets, which are all of them.
  - **Reserved names:** `with_marker` (conflict and trash names) puts a `_` in front when the stem (shortened to fit) plus the marker would start with `.~ffsl.`, and shortens the stem one byte more for it. Only `.~ffsl` + `.sync-conflict-…` can do that: trash markers start with `~`.
  - **Deep trees:** `Walk::walk_dir` keeps an explicit stack of `Level { fd, path, names }`; `visit` no longer recurses and returns a directory's fd, which `walk_dir` (or `scan_path`, for the last component or a followed ancestor) descends into. A path longer than `MAX_PATH_LEN` (4095 bytes) is an error (`path longer than PATH_MAX; not synced`) and is not entered. That bounds the depth at 2048, so the fds of a scan (the CLI raises the soft limit to the hard one). The watcher's `watch_tree` was already iterative, and `open_dir` fails on such a path.
  - **Followed referents:** `fs::root::reopen_file(pin)` opens the `O_PATH` fd's inode through `/proc/self/fd/N` (`O_RDONLY | O_NONBLOCK`, `O_NOATIME` if allowed), and refuses anything but a regular file with `EINVAL`. The scanner's `follow_file` reopens the classified fd; `LocalReplica::open_indexed` pins the referent `O_PATH` first, then reopens it (`EINVAL` becomes `Unstable`).
  - **Considered and rejected:** a separate, higher limit for stored counters than for received ones: the bumps a replica makes are sent to the peer, so some limit is always passed eventually; failing cleanly at the limit is what matters. Expiring peers not seen for a while: a pair has exactly one peer, so forgetting the others is exact. A depth cap separate from the path length: the length is what makes a path unusable, and it bounds the depth anyway. The commit's `remove_tree` and `fill_tree` still recurse, over our own staged trees, whose paths the index (now at most 4095 bytes) bounds.
  - **Tests:** `index::vv::tests::counter_limit`, `index::store::tests::a_counter_above_the_limit_is_not_written`, `replica::local::tests::counters_at_the_limit_are_refused` (an op at the limit, a conflict rename and a `SetMeta` above a clock at the limit, a scan of a changed file); `index::store::tests::a_retired_peer_does_not_keep_tombstones`; `symlink::munge::tests::examples` and `replica::local::tests::symlinks_with_munging` (the prefix alone); `fs::tmpname::tests::conflict_and_trash_names_are_never_reserved`; `scan::scanner::tests::a_deep_tree_fits_a_small_stack_and_paths_stop_at_path_max` (2057 levels in a 256 KiB stack; the old scanner overflows it); `scan::scanner::tests::a_followed_file_is_not_looked_up_again` (`l -> m -> x`, `m` swapped to a FIFO at the second in-tree lookup; the old scanner opened the FIFO and reported `l` dirty), `fs::root::tests::reopen_file_opens_only_a_pinned_regular_file`. Each fails without its fix.
  - **Results:** `cargo test`, `cargo test --features hooks`, clippy (with and without `hooks`) and the release stress test pass. Manual run with the release binary (a local pair on xfs): `.~ffsl` changed on both sides became `_.~ffsl.sync-conflict-…` on both. A tree of 2100 nested `d` directories with a file at the bottom: `sync --once` synced the 2048 directories whose paths fit and reported one error, `d/…/d` at 4097 bytes; the next run applied nothing and reported the same error.
  - **Docs:** `docs/usage.md` §2 (tombstones and a new replica ID), §4 (conflict names), §6 (`munge_links`), §11 (long paths), §12 (`version counter at its limit`); design §3 (version vectors, scanner errors, tombstones), §4.4, §4.5, §6.2.

### [x] T45: Rename the project to `ffsl-sync`
- **Depends on:** T44
- **Read:** §2 (reserved names), §5.1 (root marker), §7.1 (network phase)
- **Files:** `Cargo.toml`, `src/fs/tmpname.rs`, `src/config.rs` (`pair_dir`), `src/replica/proto/mod.rs` (`MAGIC`), `src/tls.rs` (`SERVER_NAME`), every file naming the crate or the binary; the docs.
- **Do:**
  - `files_sync` becomes `ffsl-sync` ("files folders symlinks sync"): package and binary `ffsl-sync`, crate `ffsl_sync`.
  - The names derived from the old one follow: state directory `$XDG_STATE_HOME/ffsl-sync/` (was `fsync/`), reserved prefix `.~ffsl.` (was `.~fsync.`), protocol magic `ffsl\0\0wp` (was `fsync\0wp`), TLS server name `ffsl-sync`.
- **Done when:** nothing but the release notes of 0.1.x names `files_sync`, `.~fsync.` or the `fsync` state directory; `docs/usage.md` says how to carry a pair over; `cargo test`, `cargo test --features hooks` and clippy pass.
- **Notes:**
  - **Clean break, no migration code:** an upgraded binary does not find an old pair (its state directory moved), and once the state is moved it refuses a root with only an old `.~fsync.root.*` marker, so skipping the upgrade steps deletes nothing. An old and a new peer refuse each other with `bad magic`. The steps (a last one-shot sync with the old binary, `mv` of the state directory, renaming the root marker and the trash) are in `docs/usage.md` §1, "Upgrading from `files_sync` 0.1.x".
  - **Tests:** `fs::tmpname::tests::conflict_and_trash_names_are_never_reserved` computed its extension length from the 7-byte `.~fsync`; it now uses the 6-byte `.~ffsl`.
  - **Not renamed:** the notes of releases up to 0.1.15 (never rewritten), and the local checkout's directory name.
  - **Results:** `cargo test` (406 passed), `cargo test --features hooks` (425 passed) and clippy (with and without `hooks`) pass. A local pair made and synced by the new debug binary gets `.~ffsl.root.<id>` markers and its state under `ffsl-sync/`.
  - **Docs:** `README.md` (former name), `docs/usage.md` §1 (upgrading), `RUST_LOG` filters name the crate `ffsl_sync`; design §2 (reserved names: the former ones).

## M12: performance (issue #15)

[Issue #15](https://github.com/artem1579gh/ffsl-sync/issues/15): several costs grow with the size of the whole index instead of with the size of a change, which matters for trees of 10^5–10^7 files. The issue's four items map to T46–T49 as follows. Item 4 is T46, item 2 is T47, item 1 is T48 + T49, item 3 is T48 (part) + T49. The line numbers in the issue are stale; the tasks below name functions instead. Do the tasks in order. The release that closes the issue is made after T49, not by these tasks.

### [x] T46: A metadata-only change to a file is applied in place, not as a full copy (issue #15, item 4)
- **Depends on:** T45
- **Read:** §3 (what counts as a change), §5.3 (especially step 4(b), leases, and 4(d)), §5.4, §5.8, §5.10 (the Hardlinks row), §7 (`SetMeta` under Ops)
- **Files:** `src/fs/commit.rs` (new `set_file_meta`, next to `set_dir_mode`; `take_lease`, `unmodified`, `same_object`), `src/fs/lease.rs`, `src/replica/local.rs` (the `Op::SetMeta` arm for `Kind::File`, the "Rewrite as a copy with the new metadata" branch), `tests/attack.rs` (hook-point prefixes), possibly `tests/crash.rs`, `claude/design.md`, `docs/usage.md`, `CLAUDE.md` (the "Never write in place" rule)
- **Do:**
  - **Problem:** a `SetMeta` whose mode or mtime differs from the file's is applied as `commit::replace_file` with a copy of the file's own content (`disk.open_file`). A `touch` of a 10 GB file rewrites 10 GB on the peer, needs 10 GB of free space, and puts the old inode into the quarantine (and the trash, if it is on). Over the network nothing extra crosses the wire: the server applies the op through `LocalReplica::apply` too, so fixing the local replica fixes both modes, and the protocol does not change.
  - **Fix:** add `commit::set_file_meta(ctx, path, expected: &Fingerprint, meta: FileMeta) -> Result<Outcome>`, modelled on `set_dir_mode`:
    1. Resolve the parent (`Target::resolve`) and pin the file with `openat2(… O_RDONLY|O_NOFOLLOW|O_NONBLOCK|O_CLOEXEC, RESOLVE)`. `fchmod` refuses an `O_PATH` fd. `O_NONBLOCK` keeps a FIFO swapped in from blocking. Check that the pinned object is a regular file that is `unmodified` against `expected` (same object, mode, size, mtime and ctime).
    2. Take a lease the way `replace_file` does (`take_lease` / `Lease::take`), and re-check the fingerprint (ctime included) under it.
    3. `fchmod` and `futimens` (mtime from `meta`, atime `UTIME_OMIT`) through the pinned fd, then `fsync`. Afterwards the lease must still be unbroken. Then `t.finish` with the new fingerprint, as `set_dir_mode` does.
  - **Why the lease matters** (put this in the design): setting the mtime back can hide a concurrent write. Suppose a writer that already had the file open writes between our check and `futimens`. We then restore the synced mtime and record the post-op fingerprint, so the scanner never sees the write. It is never synced, and a later change from the peer overwrites it **without a conflict copy**. A lease cannot be taken while someone has the file open for writing, and a new write open breaks it. So an unbroken lease proves that nothing wrote to the file.
  - **No lease** (`caps.leases` off, or the lease is refused or broken): after the change, rehash the content through the pinned fd and compare it with the op's hash. This reads the file but writes nothing and needs no space. If the hash does not match, do not record the new fingerprint, and report the path for a rescan (`Unstable` or `PreconditionFailed`; pick what the callers already handle). The scanner then sees a ctime that differs from the index and rehashes. Alternatively fall back to the current copy; choose one, and write the reason in Notes.
  - **Hardlinks:** if the file has `nlink > 1`, keep the current copy path. An in-place chmod would change the other links too, while §5.10 says hardlinks are independent files to us. Followed files (`via_link` entries) also keep the copy path.
  - **Crash between the change and the index put:** check how `set_dir_mode` and `replace_file` handle it (journal, §5.8; the issue #13 note in T43). For a metadata-only change, a crash there must not end in a conflict copy or a lost change. The rescan sees new metadata with an older vv and makes it a local change, which could look concurrent to the peer's vv. Find out what happens and handle it, for example with a journal intent that replay completes or rolls back. Add a crash test if `tests/crash.rs` can express it.
  - **Hook points** at each step boundary (e.g. `setmeta.before_pin`, `setmeta.pinned`, `setmeta.after_change`). `tests/attack.rs` has a test that every hook point is attacked: add the prefix there, or attack them in `commit::tests` as T31 did for the trash.
  - **Design and rules:** update §7 (`SetMeta` "rewrites a file as a copy … never in place"), §5.3/§5.4 and §5.10. In `CLAUDE.md`, narrow the hard rule "Never write in place" to content: the mode and mtime of a pinned, verified, leased inode may be changed in place, as `set_dir_mode` already does for directories. Check `docs/usage.md` for text saying a metadata change goes through the quarantine or trash.
- **Done when:**
  - a `SetMeta` (mode only, mtime only, both) on a regular file keeps its inode (same `ino`), creates no temp file, and leaves nothing in the quarantine or trash; the index records the new fingerprint;
  - a write between the pin and the change (via a hook) is never hidden, with and without leases (`caps_mut().leases = false`): either the op fails for its path, or the next scan records the write;
  - a file with `nlink > 1` and a followed file still go through the copy;
  - a test fails without the fix (e.g. the inode check);
  - `cargo test`, `cargo test --features hooks`, clippy (with and without `hooks`) and the release stress test pass;
  - manual check with the release binary (local pair, and a served pair if quick): `touch` a 1 GB file on A, then `sync --once`. B's file keeps its inode and gets the new mtime, and the sync takes well under the time of a 1 GB copy.
- **Notes:**
  - **Done:** `commit::set_file_meta(ctx, path, &Expected, FileMeta) -> Result<Option<Outcome>>` (design §5.3.1). It takes `&Expected` rather than `&Fingerprint`, since it needs the indexed hash and `racy`. `Ok(None)` means "the file has other hard links" (checked with `fstat` on the pin): the caller (`LocalReplica::run`, `Op::SetMeta`) then falls back to the copy, as it does for a `via_link` file.
  - **Without a usable lease: rehash, not the copy.** A rehash through the pin reads the file but writes nothing and needs no space, and the copy would read it too. A racy entry is rehashed even under a lease, since its indexed fingerprint cannot vouch for its content. A broken lease is dropped before the rehash, so its breaker does not wait for us. A mismatch or unstable read is `PreconditionFailed("changed during commit")` (the engine already rescans such paths); the index keeps the old ctime, so the scan rehashes and records the write.
  - **Our own `fchmod`/`futimens` do not break our lease** (`notify_change` breaks delegations only), so the lease path never rereads: `set_meta_changes_a_file_in_place` checks that `setmeta.before_rehash` is reached exactly when leases are off. The pin is an `O_RDONLY` fd, so `Lease::take` gets a read lease (our pin refuses a write lease); a read lease is what matters here.
  - **Crash between the change and the index put.** A single-attribute change merges silently (`MergeVv`, same content and mode, same mtime). With both changing, a crash between the chmod and the `futimens` left the new mode with the old mtime, and the merge took the newer mtime, so an mtime set back by the user was lost (`tests/crash.rs` `chmod_touch_file` at `setmeta.after_chmod` showed exactly that with the replay disabled). So that case records an `IntentOp::SetMeta` intent (appended to the enum: old records still decode; no reserved names, `tmp` empty, `staged` = the target metadata), and replay (`revert_meta`) puts the old mode back in that midway state only. One durable journal write per such op; a `touch` or a `chmod` alone needs none.
  - **Kept:** applied entries stay `racy`, so the next scan rereads the file once. With the lease or rehash, dropping `racy` when the synced mtime is older than the ctime by more than the racy window would be safe (a later write moves the mtime away from the recorded one) and would save that read; left as a possible optimisation (design §5.3.1).
  - **Tests:** `replica::local::tests::set_meta` (now asserts the same inode; fails without the fix), `set_meta_changes_a_file_in_place` (mode only, mtime only, both, both without leases; same inode, no `stage.`/`replace.` points, empty quarantine, no trash, index fingerprint = disk), `set_meta_copies_a_file_with_other_links`, `set_meta_never_hides_a_write` (a writer opening after the pin, and one holding the file open from before, with and without leases; fails if the rehash is skipped), `set_meta_replay_puts_the_old_mode_back`. The followed-file copy stays covered by the existing copy-links test. `tests/attack.rs`: `Op::SetFileMeta` (prefix `setmeta`, an `EditBefore` variant at `setmeta.leased`, and a no-lease caps variant). `tests/crash.rs`: scenarios `touch_file` and `chmod_touch_file` (also in `crashes_during_recovery`).
  - **Results:** `cargo test`, `cargo test --features hooks`, clippy (with and without `hooks`) and the release stress test pass. Manual check: see Performance. A served pair was not tried by hand: the server applies ops through `LocalReplica::apply`, the same code, and the protocol is unchanged.
  - **Performance** (release binary, local pair on xfs, a 1 GiB random file, `touch` + `chmod 600` on A, then `sync --once`; the binary before this task built from `c851102` in a scratch worktree, same machine, same steps):

    | | B's file | `sync --once` |
    |---|---|---|
    | before (copy) | new inode; 1 GiB written and fsynced, 1 GiB of free space needed, old inode quarantined (and trashed with `trash_days`) | 6.1 s |
    | after (in place) | same inode, new mode and mtime; nothing written but metadata, nothing quarantined or trashed | 2.4 s |

    The remaining 2.4 s are two reads of the file, no writes: A's scan rehashing the touched file (needed to see that only the metadata changed) and B's next scan rereading it once, because applied entries stay `racy` (see Kept; dropping that would save the second read). With leases the commit itself reads nothing; without them it rehashes the file once more. Over the network nothing more crosses the wire than before, as the protocol is unchanged.
  - **Docs:** design §5.3 (step 4'), new §5.3.1, §5.4, §5.8, §5.10 (Hardlinks), §7 (`SetMeta`); `CLAUDE.md` (the rule is now "never write content in place"); `README.md` (Writes); `docs/usage.md` §2 (Trash: a metadata change puts nothing there).

### [x] T47: Refresh the watched followed links only when there can be any (issue #15, item 2)
- **Depends on:** T46
- **Read:** §4.3, §4.3.1, §4.5, §5.9
- **Files:** `src/replica/local.rs` (`after_scan`, `watch_followed`, the `followed` field, and the second `watch_followed` call site near `watch`), `src/scan/scanner.rs` (where `via_link` is set: `follow_dir`, `follow_file`), possibly `ScanStats`, `tests/daemon.rs`
- **Do:**
  - **Problem:** after every scan that changed anything (`stats.changed > 0`), `watch_followed` reads every index entry (`iter_prefix(&RelPath::root())`, a full decode into a `Vec`) to find entries with `local.via_link`. It does this even when the config can never produce followed links. In a daemon on a big tree that is a full index read for every saved file.
  - **Main fix (cheap, safe):** return early when the replica cannot have followed or adopted links, and there were none the last time (`self.followed == 0`). First establish exactly which configs can produce `via_link` entries: the symlink policies that follow (`CopyLinks`, `CopyUnsafeLinks`, `CopyDirlinks`) and `keep_dirlinks` (`-K` adoption). Read the scanner to confirm, and to check whether `via_link` is also set on entries beneath a followed directory. Keep the `self.followed > 0` case. A config that turned following off still has `via_link` entries in its index until the next scan, and the watcher must drop their watches.
  - **Optional, only if it stays small:** when following is possible, avoid the full read too. Two ways: a `ScanStats` flag set only when an entry with `via_link` (or one beneath it) changed, or a secondary redb table of `via_link` paths kept in `WriteTxn::put`/`remove`. A secondary table adds an index invariant that every write path must keep, so prefer the flag or skip this part. Record the decision in Notes.
- **Done when:**
  - with the default `links` policy and no `-K`, a changing scan does not iterate the index for followed links. Prove it with a test, e.g. a test-only counter of `watch_followed` index reads, or a hook point;
  - the existing followed-link watcher tests (copy-links, copy-dirlinks, `-K`) still pass, including a link that stops being followed;
  - `cargo test`, `cargo test --features hooks` and clippy pass.
- **Notes:**
  - **Done:** `LocalReplica::watch_followed` returns before reading the index when `self.followed == 0` and `!may_follow()`. `may_follow` is `keep_dirlinks` or a policy of `copy-links`, `copy-unsafe-links`, `copy-dirlinks`. This is the right set: `via_link` is set only by the scanner's `Treatment::Follow` (from `classify`, which follows only under those three policies) and by `-K` adoption (`may_adopt` needs `keep_dirlinks`). In both cases it is set on the link's own entry only (`dir_found`, `file_found`); entries beneath a followed directory have `via_link: None`. The only other writer (`local.rs`, the write-back of a followed file) copies the existing entry's `via_link`. The check is in `watch_followed`, so both callers (`after_scan`, and `watch` at start) skip the read.
  - **`followed > 0` kept:** a config that stopped following (tests change `config` in place; a restart begins with `followed == 0`) still lists once more, so the watcher drops the old links' watches. After a restart with following off, the stale `via_link` entries are not watched; the startup scan indexes them anew.
  - **Optional part skipped** (no `ScanStats` flag, no secondary table). `ScanStats` crosses the wire, so a new field touches the protocol. The scanner also does not know whether a path lies beneath a followed directory (`OpenDir::followed` marks only the link's own directory), and a flag would have to cover new directories beneath a followed one, tombstones there, and links reclassified by a scoped scan. A missed case means a missing watch, which only shows as a delay until the periodic rescan. Not small, so with following on, a changing scan still reads the whole index. T48 may stream that read (its `iter_prefix` item).
  - **Tests** (`replica::local::tests`, using the new hook point `watch.list_followed` and a `FollowLog` event source that records `EventSource::follow`): `followed_links_are_listed_only_when_there_can_be_any` (`skip`, `links`, `safe-links`: no index read at `watch()` nor on a changing scan; `copy-links`, `copy-unsafe-links`, `copy-dirlinks`, `links` + `-K`: read); `a_link_that_stops_being_followed_is_unwatched` (`copy-links`, `copy-dirlinks`, `-K` adoption: the link is handed to the watcher; after switching to `links` without `-K`, the next changing scan lists again and hands over an empty set; the one after that reads nothing). Without the early return the first test fails; without the `followed > 0` half, the second does. There were no existing watcher tests for `copy-dirlinks`, `-K` or a link that stops being followed; these cover them. `tests/daemon.rs` `followed_referents_are_watched` (`copy-links`) still passes.
  - **Results:** `cargo test`, `cargo test --features hooks` and clippy (with and without `hooks`) pass.
  - **Docs:** design §5.9 (Followed links). No CLI, config or output change.

### [x] T48: Keep index snapshots across rounds and cycles; a scale benchmark (issue #15, items 1 and 3, first part)
- **Depends on:** T47
- **Read:** §3 (seq, tombstone GC), §6.1, §6.4, §7, §7.1 (the remote mirror), §9
- **Files:** new `tests/scale.rs` (or an `#[ignore]` test in an existing suite), `src/engine/mod.rs` (`Snapshot`, `IndexView`), `src/engine/executor.rs` (`Cycle::reconcile`, `adopt_dirlinks`, `record_sync`, `hold_mass_deletes`, `Cycle::last`), `src/replica/mod.rs` (the `Replica` trait, `PairReplica`), `src/replica/local.rs` (`changes_since`), `src/replica/remote.rs` (`Mirror`, `changes_since`, `clock`), `src/index/store.rs` (`changes_since`, `iter_prefix`, `record_sync`, `remove`), `src/scan/scanner.rs` (`Walk::tombstones`), `src/engine/sim.rs`, `tests/harness/mod.rs` (`TestReplica`, `Racing`), `tests/daemon.rs` (`Flaky`), `claude/design.md`
- **Do:**
  - **1. Benchmark first**, so this task and T49 have before/after numbers. Add an `#[ignore]`d scale test, run like the stress test (`cargo test --release --test scale -- --ignored --nocapture`). The tree size comes from an env var (default about 200k files, so it fits tmpfs; try 1M by hand). Print, without asserting timings: the initial sync time; a no-op `sync --once` cycle; a cycle after changing one file; a cycle after `touch`ing one big file (to confirm T46); and peak RSS (`VmHWM` from `/proc/self/status`; both replicas are in-process, so it covers both). Cover local and remote mode (the harness has both). Record the numbers **before** any change in Notes.
  - **2. Problem:** every round, `Cycle::reconcile` does `changes_since(0)` on both replicas. Locally that decodes every redb row into a fresh `BTreeMap`, twice more if `-K` adopted something, and in every round of every cycle, including the echo cycle after each applied change. Remotely, `RemoteReplica::changes_since` already keeps an incremental `Mirror`, but it still clones the whole mirror into a `Vec` and sorts it on every call.
  - **Goal:** after a process's first round, a round costs no full decode and no full clone of either index. Each side's snapshot is kept in memory and updated from `changes_since(last_seen_seq)`. The reconcile walk over the in-memory snapshots stays O(N) in this task; T49 removes that.
  - **What can invalidate a cached snapshot** (verify each before relying on it):
    - Puts: covered by seq. A put moves an entry to a new seq.
    - Row removals: by reading the code, the only one is tombstone GC in `WriteTxn::record_sync` (`store.rs`), and `Replica::record_sync` returns the removed paths. Check that no other code path removes index rows (`IndexStore::remove` looks test-only).
    - A remote session change: `Mirror` resets when `conn.session` changes, since the server may have restarted with another index. The cache must reset with it.
    - Another client of the same served replica: `server.rs` keeps several connections per served replica. Find out whether two clients can sync the same served replica, and whether one client's tombstone GC can remove rows another client has cached. If so, a delta by seq misses those removals. A served replica could then expose an epoch or a removal log, or the cache resets on each remote session.
  - **Where the cache lives** is the main design decision. The engine does no I/O and only talks to `Replica`; the model test (`engine::sim`) and the test replicas (`TestReplica`, `Racing`, `Flaky`) implement the trait too. Two options:
    - (a) the `Replica` implementations own the snapshot (as `RemoteReplica` owns its `Mirror`) and the trait gains a method returning a shared view (e.g. `Arc<Snapshot>`) without a deep copy;
    - (b) the engine owns it, per side, updated from `changes_since(seq)` plus the paths `record_sync` returned, with a replica epoch for resets.
    Pick one and record why. It must survive across cycles in a daemon (the `Engine` is reused; check `daemon.rs`), since the daemon's per-change echo cycles are the main beneficiary. `Cycle::last` (used by `record_sync`) and `hold_mass_deletes` (counts live entries) must keep working. Keep `changes_since` as is for the network protocol and the old-server `clock` fallback.
  - **Consistency check:** under `cfg(test)` or the `hooks` feature, compare the cached snapshot with a fresh `changes_since(0)` after every round, and fail loudly on a difference. Run the model test, `tests/sync_once.rs`, `tests/daemon.rs`, `tests/net.rs` and the stress test with it on.
  - **Scanner, small part of item 3:** `Walk::tombstones` collects `snap.iter_prefix(region)` into a `Vec` holding every entry of the region; for a full scan that is the whole index. Stream it instead with a redb range iterator (e.g. a `ReadTxn::for_each_prefix` with a closure). Keep `seen` as it is; T49 replaces it. Check `watch_followed` (T47) and the other `iter_prefix` callers for the same win, but change them only if it is trivial.
  - **Design:** §6.4 (where snapshots come from) and §7 (the trait), plus §9 for the scale benchmark.
- **Done when:**
  - the scale benchmark exists, and Notes hold its numbers before and after (local and remote, about 200k files, and 1M if it fits);
  - a test proves that after the first round of a process, a no-op cycle and a one-file cycle do not decode the whole local index or clone the whole remote mirror (e.g. a test-only counter of entries decoded or returned);
  - the consistency check passes in the model, sync, daemon, network and stress tests;
  - `cargo test`, `cargo test --features hooks`, clippy and the release stress test pass.
- **Notes:**
  - **Done:** `Replica::snapshot() -> Arc<Snapshot>`, the whole index in wire form, shared. Its default builds it anew from `changes_since(0)`; `LocalReplica` and `RemoteReplica` keep it in an `engine::SnapshotCache` (the remote's `Mirror` is now one) and read only `changes_since(highest seq seen)`. `Cycle::reconcile` asks for both snapshots each round (`Cycle::last` holds `Arc`s) and drops the last round's first, also after a `-K` adoption, since `Arc::make_mut` copies a snapshot that is still held when it changes (`CacheStats::copies`). `changes_since` is unchanged, for the protocol and the old-server `clock` fallback.
  - **Where the cache lives: (a), the replica.** Only the replica knows when its index can change behind a cache: a `LocalReplica` is the only writer of its index (redb locks it; a reopened replica starts with an empty cache), a `RemoteReplica` resets with each new session, and both see every removal, since GC runs inside `record_sync`. An engine-side cache (b) would need an epoch in the trait to notice a replica replaced under the same `Engine` (tests reopen replicas; `run_reopening` does too). With (a) the remote index is held once (the mirror *is* the snapshot), the model test and the wrapper replicas need nothing (the harness's `TestReplica`, `Racing` and the daemon test's `Flaky` forward `snapshot`), and the daemon keeps its replicas, so the caches live across its cycles. `Cycle::last` (`record_sync`) and `hold_mass_deletes` work on the shared snapshots as before.
  - **What can invalidate a snapshot, checked:** puts always take a fresh seq (`WriteTxn::put`); `put_local` keeps it but changes only `LocalMeta`, which the wire form drops. `WriteTxn::remove` is called only by `WriteTxn::record_sync` (GC); `IndexStore::remove` is used by `store.rs` tests only. `record_sync` notes the removed paths in the cache, and they leave the snapshot at the next `snapshot()`: the engine still holds the cycle's last snapshots while it calls `record_sync`, so removing them at once would copy them. A remote session change resets the cache, as the mirror did. **Two clients of one served replica:** a server serves one peer, so two clients are two processes syncing the same pair, which a local replica prevents (its index is locked); with both replicas served (allowed: `init` with two remote addresses), nothing prevents it. One process's mirror then keeps the tombstones the other's GC removed, exactly as the old `Mirror` did (it also kept its rows within a session). Such a row is a tombstone the server no longer has, and is replaced as soon as the path is put again; left as it is, written down in design §6.4.
  - **Consistency check:** under `cfg(test)` or `hooks`, every `snapshot()` compares the cache with the whole index read anew (`SnapshotCache::check`, panics with the first differing paths): locally `changes_since(0)`, remotely a second `ChangesSince { seq: 0 }` on the same connection (not counted in `entries_received`). `cargo test --features hooks` runs it in the model, sync, daemon, network (`net`, `remote`, `delta`) suites, and the stress test with `--features hooks` (see Results). Checked that it bites: without the removal note in `LocalReplica::record_sync`, `sync_once::local::tombstones_are_collected_after_the_retention_period` panics with the collected paths.
  - **Proof test:** `sync_once::index_snapshots_are_kept_across_cycles` (both modes), with `CacheStats` (`LocalReplica::snapshot_stats`, `RemoteReplica::snapshot_stats`, `TestReplica::snapshot_stats`): the first sync reads each entry once (82 per side), a no-op cycle reads none, a one-file cycle one per side, a delete with GC and a conflict at most 10, and no snapshot is ever copied. Fails without `self.last = None` (copies) and with a cache that re-reads the index on each call (reads).
  - **Scanner:** `Walk::tombstones` streams each region with the new `ReadTxn::for_each_prefix(prefix, wanted, f)` (a redb range, no `Vec`; `iter_prefix` is now built on it), and decodes only the keys `seen` does not hold. `seen` stays (T49). Also streamed, as it was trivial: `watch_followed`'s list (T47) and `ReplicaStatus::from_index`, which a server runs after every `RecordSync`. Left as they are: the journal replay's directory search by inode (returns from inside the loop; runs only for orphaned intents), `Rmdir`'s child check and `-L` copies (one subtree), `ensure_root_marker` (at open).
  - **First full read:** a snapshot read into an empty cache is built with `collect()` (a bulk build, full B-tree nodes), as the old `Snapshot::new` was, not by single inserts, which left the local initial sync's peak RSS 8 % higher at 1M files (2922 MiB vs 2714). The remote mirror still fills batch by batch, as before.
  - **Benchmark:** `tests/scale.rs` (design §9), `cargo test --release --test scale -- --ignored --nocapture --test-threads=1`. On tmpfs (`TMPDIR=/dev/shm/…`; on this machine's xfs the initial sync's fsyncs dominate: 195 s instead of 70 s at 200k). Before = the binary built from `4ae697f` (this task's tests/scale.rs only), after = this task; same machine (32 cores), each run alone. The phases are described in the file. "scoped" is a daemon cycle (`Scope::Paths` of one path), "full" a `sync --once` cycle in a warm process.

    Seconds; peak RSS in MiB (`VmHWM` of the phase, both replicas and, in remote mode, both servers):

    | phase | local 200k before | after | remote 200k before | after | local 1M before | after | remote 1M before | after |
    |---|---|---|---|---|---|---|---|---|
    | initial | 69.7 s, 575 | 68.8 s, 576 | 96.5 s, 718 | 96.5 s, 653 | 356 s, 2714 | 350 s, 2715 | 487 s, 3351 | 483 s, 3135 |
    | no-op full | 10.9 s, 392 | 10.0 s, 344 | 10.2 s, 637 | 9.8 s, 522 | 47.3 s, 1898 | 40.2 s, 1725 | 42.7 s, 3075 | 39.6 s, 2456 |
    | no-op scoped | 0.75 s, 392 | 0.23 s, 344 | 0.48 s, 637 | 0.23 s, 522 | 3.83 s, 1898 | 1.22 s, 1677 | 2.58 s, 3075 | 1.20 s, 2409 |
    | one file, scoped | 1.54 s, 392 | 0.46 s, 344 | 1.03 s, 688 | 0.46 s, 522 | 8.01 s, 1899 | 2.48 s, 1677 | 6.01 s, 3221 | 2.40 s, 2410 |
    | one file, full | 3.61 s, 392 | 2.27 s, 349 | 3.04 s, 694 | 2.32 s, 522 | 19.0 s, 1899 | 12.6 s, 1725 | 17.1 s, 3400 | 12.4 s, 2508 |
    | touch big, scoped | 1.72 s, 392 | 0.65 s, 344 | 1.23 s, 694 | 0.67 s, 522 | 7.77 s, 1899 | 2.58 s, 1677 | 6.38 s, 3405 | 2.55 s, 2461 |

    - A scoped (daemon) cycle is about 3 times faster: a round no longer decodes both indexes (local) or copies both mirrors and sorts them (remote). What is left grows with the tree: about 0.23 s per round at 200k and 1.2 s at 1M (a one-file cycle has two rounds): `reconcile`'s walk over the union of both snapshots, `adopt_dirlinks`, `hold_mass_deletes` (counts live entries) and `record_sync`'s tombstone list, all over the in-memory snapshots. That is T49's part A.
    - Full cycles gain less: their two full scans dominate (`no-op full` right after the initial sync also rehashes every file once, since applied and freshly written entries are `racy`; T46 Kept).
    - Memory: the remote mode's steady state is 20–30 % lower (no per-round copy of each mirror). The local mode's is about 10 % lower; it now keeps the snapshots between cycles (before, the engine built them per round), which costs less than the transient copies it saves. The initial sync's peak is the same (local) or lower (remote). The scanner's `seen` set of every path is still in the full-scan peak (T49 part B).
    - `touch big` (256 MiB, T46): B's file is changed in place; the rest of the time is A's rehash and B's racy reread.
  - **Results:** `cargo test`, `cargo test --features hooks` (consistency check on in every suite), clippy (with and without `hooks`) and the release stress test pass; the stress test also with `--features hooks` (the check under the daemon, both modes).
  - **Docs:** design §6.4 (Index snapshots), §7 (the trait), §7.1 (incremental index exchange), §9 (consistency check, scale benchmark); `CLAUDE.md` and `README.md` (the benchmark command). No CLI, config or output change.

### [x] T49: Reconcile only what changed, and find deletions without holding every path (issue #15, items 1 and 3)
- **Depends on:** T48
- **Read:** §3, §4.3, §6.1, §6.2, §6.3, §6.4, §5.10, §9
- **Files:** `src/engine/reconcile.rs` (`reconcile`, `decide`, `Names`, unmanaged ancestors), `src/engine/executor.rs` (`Cycle`, `Engine::run`, `hold_mass_deletes`), `src/engine/sim.rs`, `src/scan/scanner.rs` (`Walk`, `seen`, `protected`, `regions`, `tombstones`, `walk_dir`, `pending`, `flush`), `src/index/store.rs` (range iteration), `tests/scan.rs`, `tests/model.rs`, `tests/scale.rs`, `claude/design.md`
- **Do:**
  - **First measure.** Run the T48 benchmark and profile one no-op cycle, a one-file cycle and a full scan (e.g. `perf`, or timings per phase). Write down where time and memory go after T48. If a one-file cycle at 1M files is already fast and peak memory is not dominated by the parts below, **stop and ask the user** whether to continue, and put the numbers in Notes. This task is the riskiest in M12. If it is too big for one session, split it into T49 (reconcile) and T50 (scanner) in this file before starting, rather than doing half of each.
  - **Part A: incremental reconcile.** `reconcile(a, b, now)` builds a `BTreeSet` of the union of all paths and calls `decide` on every one, every round. Add a variant that reconciles only a given set of paths against the full in-memory snapshots from T48. A cycle uses the full reconcile on a process's first cycle, and on the daemon's periodic full rescan (`Daemon::DEFAULT_RESCAN`) as a safety net. Otherwise it uses the incremental one over:
    - every path that changed on either side since the last converged cycle (from the seqs T48 tracks);
    - the paths left over from the last cycle: unresolved, failed, held back by the mass-deletion guard, unmanaged warnings;
    - closed under the rules that span paths. Ancestors are needed for `BeneathUnmanaged` and for ordering. The whole subtree is needed when a path's kind changes to or from `Dir` on either side (a directory replaced by a file deletes its subtree; an `Rmdir` needs its descendants). Read `reconcile.rs` and `plan.rs` for every place that looks beyond one path, including how conflict names (`Names::chosen`) are chosen, and list them in Notes.
    - **Key test:** a proptest (reuse `engine::sim`'s generators) that for random index pairs and random change sets, the incremental reconcile over the closure returns exactly the full reconcile's actions for those paths, and the full reconcile has no actions outside the closure. Run the model test with incremental reconcile on.
  - **Part B: scanner merge-walk.** A full scan keeps every path in `seen: HashSet<Vec<u8>>` to find, at the end, indexed paths that were not seen (`tombstones`). Replace this with a per-directory comparison. After listing directory D, walk D's **direct** children in the index and tombstone those not in the listing. Pitfalls:
    - Index keys are raw bytes in byte order, so D's direct children are not contiguous: `D/b-x` sorts before `D/b/c` (`-` < `/`). Skip a child's subtree by seeking past `D/child/` (to `D/child0`, since `0` is `/` + 1), not by scanning it, so the pass is O(children · log N) per directory.
    - Never tombstone beneath a directory that could not be read (`EACCES`, errors), a dirty or `protected` path, an unmanaged entry, or a followed directory handled elsewhere. Respect `Scope::Paths` regions: a scoped scan of one path only tombstones within its regions, as now.
    - Check that `pending` (all updates of a scan, flushed in one transaction) is not the next memory hog on an initial scan. Batch the flush only if a single transaction is not needed for correctness, and say why in Notes.
    - **A wrong tombstone becomes a deletion on the peer.** Keep the old `seen`-based tombstone pass under `cfg(test)` as an oracle. Add a proptest over random trees, with names containing `-`, `.`, bytes below `/`, `0xFF` and non-UTF-8, plus unreadable directories, dirty paths and scoped scans, asserting that the new tombstones equal the old ones.
  - **Design:** §6.4 (incremental reconcile, its closure, the full-reconcile safety net), §3 or wherever the scanner's deletion detection is described, §9 (the new property tests).
- **Done when:**
  - both property tests pass and fail when the closure or the merge-walk is broken on purpose (e.g. drop the subtree rule or the seek; say so in Notes);
  - the scale benchmark shows a one-file cycle whose time no longer grows with the tree size (compare 200k and 1M files, or the largest that fits), and a full scan whose peak RSS no longer includes a set of every path. The numbers go in Notes, next to T48's;
  - `cargo test`, `cargo test --features hooks`, clippy and the release stress test pass;
  - a manual daemon run on a large tree (local pair, release binary): editing one file syncs it, and the `sync cycle done` logs show nothing unexpected.
- **Notes:**
  - **Measured first** (200k files, local, timers per phase; `perf` is not installed): after T48 a scoped round still spent 65 ms in `adopt_dirlinks`, 150 ms in `reconcile`, 8 ms in `hold_mass_deletes` and 9 ms in `record_sync`'s tombstone list, all walks over both snapshots; a one-file scoped cycle took 2.5 s at 1M. Not fast, so the task went ahead, in one piece (no split into T49/T50).
  - **Part A, done:** `reconcile_paths(a, b, paths, now)` decides at `paths` only; `reconcile` is the same walk over the merged keys of both snapshots (no `BTreeSet` of the union any more). The nearest unmanaged ancestor is looked up in the snapshots instead of collected along the walk. `reconcile::Carry` holds what a reconcile leaves: both snapshots' `Mark`s and the paths of all its actions (skips and rescans too, so unresolved, failed, held-back and unmanaged paths). `Carry::paths` adds the paths changed since on either side. The `Engine` keeps the carry between cycles (a `Mutex`; a clone starts without, a failed cycle drops it). The first round of a `Scope::Full` cycle (process start, daemon rescan, watcher overflow, `sync --once`) reconciles wholly; so does any round whose snapshots cannot tell what changed.
  - **What changed, per snapshot:** `Snapshot` keeps its live count, its tombstones (a `BTreeSet`) and a journal of the paths each update put or removed, with a lineage (unique per cache, new on `reset`, and per `Snapshot::new`) and a version. The journal holds max(4 096, entries/8) paths and is cleared beyond that (and on a first read), which falls back to a whole reconcile. `hold_mass_deletes` reads the live count, `record_sync` the tombstones, so no round walks a whole snapshot any more. `SnapshotCache::check` (T48) also checks the count and the tombstones.
  - **What looks beyond one path** (reconcile.rs and plan.rs): (1) `decide` reads only the path's two entries. (2) `BeneathUnmanaged`: the nearest ancestor `Unmanaged` on either side, now a lookup in the snapshots. (3) `Names::choose`: names live on either side (lookups) and `chosen`, the names picked earlier in the call, in path order: every conflict and resurrection is an action, so in the set, and is picked in the same order. (4) `resurrect`: the deleting side's entries beneath the directory (`descendants`, a lookup) and the actions beneath it (all in the set). (5) `plan` sees only the action list. (6) Outside: `adopt_dirlinks` (a symlink against a directory always has an action, so it looks at the set only). Hence the set is exactly "last actions plus changed paths": no ancestor or subtree closure is needed, because nothing is decided from what was walked before; a directory turned into a file has its subtree tombstoned by the scanner, which changes every path in it.
  - **Part A tests:** `reconcile::tests::incremental_reconcile_matches_the_whole` (2 048 cases: random index pairs of every kind on `a`/`a-b`/`a.b` paths, kept in `SnapshotCache`s, random puts and removals; the journals cover every changed path, and `reconcile_paths` over `Carry::paths` equals `reconcile`). The engine's model test (`engine::sim`) now keeps its snapshots in caches and checks every reconcile after the first. `tests/model.rs` alternates full cycles with cycles scoped to the root, whose first round is incremental. Under `cfg(test)`/`hooks` the executor compares every incremental reconcile with the whole one (`check_incremental`). Broken on purpose, each caught by the proptest: `Carry::paths` without the carried paths; unmanaged ancestors collected along the walk as before; the journal not noting removed rows. With the carried paths dropped, the executor's check also fails `daemon::…stuck` under `--features hooks`; with `Carry::paths` returning nothing, `tests/model.rs` fails, which shows it does run incrementally (it cannot catch a missing carry by itself: every one of its cycles converges, so nothing is left over).
  - **Part B, done:** no `seen` set. `ReadTxn::for_each_unlisted(dir, listed, f)` walks a directory's children in the index and hands over what is beneath the names the listing lacks; a listed name's subtree is skipped by seeking to `dir/name0`, and the names with entries beneath them are returned. The walk calls it after each listing (`Walk::list`), and tombstones beneath a listed child found gone or not a directory (`tombstone_beneath`), the latter only for children the index has entries beneath (calling it for every file cost 0.5 s per side per 200k files). `scan_path` keeps a small `noted` set of the ancestors it visits (a shared ancestor is recorded once, as `seen` did); an ancestor that was a directory a moment ago and is not now is reported dirty instead of tombstoned beneath (a race only; the old pass tombstoned beneath it what had not been found). Beneath unmanaged entries (a loop, a special file) the index is tombstoned as before: the old pass did the same, and the oracle demands it.
  - **`pending`:** not a memory hog on an initial scan: updates were already committed every 1 024 (`FLUSH_EVERY`), and tombstones now go the same way as the walk queues them, instead of all at the end. A single transaction is not needed: every update checks its base seq on its own, as the batches of visits always did. Left: one vanished subtree's tombstones are queued at once.
  - **Part B tests:** the old pass is kept under `cfg(test)`/`hooks` as an oracle in every scan (`Walk::check_tombstones`; off once a write failed at commit, the counter-limit case). `scan::scanner::tests::merge_walk_tombstones_what_the_old_pass_did` (512 cases): random trees over the names `a`, `a-b`, `a.b`, `a!`, `a \x01`, `a0`, `a\xff`, `b`, `\xff`, stale and orphaned index rows, random edits, unreadable directories (`chmod 000`), paths made dirty by a test knob (`Scanner::dirty`, `cfg(test)`), full or scoped scans; after a full scan the index must match the disk (but beneath what could not be read). Broken on purpose, each caught: seeking past a name by incrementing its last byte (misses `a0`, `a\xff`); stopping at the first listed subtree instead of seeking; not tombstoning beneath a child found not to be a directory.
  - **Stack (issue #14):** `a_deep_tree_fits_a_small_stack…` (256 KiB, debug build) overflowed in the deepest `flush`: redb inserting 4 KB keys needs about 230 KiB there, and the old code already sat between 230 and 256 KiB. `scan_path` now finds its region in `reach` and walks it from its own small frame: 14.4 KiB from `scan` to that `flush`, against 14.6 before.
  - **Benchmark:** `tests/scale.rs` also prints each phase's peak heap above its start (a counting global allocator: the RSS peak cannot show memory the allocator already had), and a phase "scan A, full". Before = `ab77b7b` (T48) with this task's `tests/scale.rs`, after = this task; tmpfs, same machine, each run alone. Seconds, peak RSS and peak heap growth in MiB:

    | phase | local 200k before | after | remote 200k before | after | local 1M before | after | remote 1M before | after |
    |---|---|---|---|---|---|---|---|---|
    | initial | 69.6 s, 575, +404 | 70.0 s, 576, +411 | 96.9 s, 654, +442 | 97.4 s, 654, +450 | 362 s, 2714, +1993 | 362 s, 2714, +1993 | 491 s, 3137, +2196 | 500 s, 3063, +2203 |
    | no-op full | 9.91 s, 344, +69 | 9.55 s, 342, +61 | 9.74 s, 522, +69 | 9.65 s, 517, +61 | 40.7 s, 1725, +315 | 38.7 s, 1640, +247 | 40.2 s, 2462, +312 | 39.0 s, 2308, +246 |
    | no-op scoped | 0.228 s, +6 | 0.000 s, +0 | 0.229 s, +6 | 0.001 s, +0 | 1.22 s, +24 | 0.000 s, +0 | 1.20 s, +24 | 0.001 s, +0 |
    | one file, scoped | 0.460 s | 0.012 s | 0.465 s | 0.012 s | 2.49 s | 0.091 s | 2.42 s | 0.049 s |
    | one file, full | 2.33 s, +10 | 1.95 s, +0 | 2.35 s, +10 | 1.95 s, +0 | 12.9 s, +85 | 10.2 s, +0 | 12.7 s, +85 | 10.0 s, +0 |
    | touch big, scoped | 0.679 s | 0.205 s | 0.654 s | 0.205 s | 2.59 s | 0.211 s | 2.56 s | 0.205 s |
    | scan A, full | 1.13 s, +10 | 1.06 s, +0 | 1.14 s, +10 | 1.06 s, +0 | 5.28 s, +85 | 4.66 s, +0 | 5.22 s, +85 | 4.65 s, +0 |

    - A daemon's cycles no longer walk the indexes: the echo cycle takes under a millisecond at any size, and a one-file cycle is 20–50 times faster. Its 91 ms at 1M (local) is not a walk: timed per phase, the reconcile took 42 µs and the snapshots under 1 µs, but the one `apply` 68 ms, against 1 ms for the same `apply` in the next phase. Probably redb freeing, at its next commit, the pages the preceding full cycle rewrote (its `racy` updates); not a cost per cycle.
    - A full scan no longer holds a set of every path: +85 MiB of heap at 1M (+10 at 200k) before, +0 after. A full cycle's remaining heap growth (+247 MiB at 1M) is not the scan's.
    - Full cycles are 5–20 % faster (one whole reconcile per cycle instead of one per round, no `seen` set); the initial sync is unchanged, being all copying.
  - **Manual daemon run** (release binary, local pair of 200 200 entries on tmpfs, `RUST_LOG=ffsl_sync=debug`): the first full cycle reconciled wholly ("nothing to do"); editing `d117/f0500` on A synced it in about 0.3 s (mostly the watcher's debounce): a scoped cycle of 21 ms that reconciled 1 path ("sync cycle done rounds=1 applied=1"), then the echo cycle, which reconciled 0 paths and had nothing to do. No warnings.
  - **Left O(N) per cycle:** a `serve` with a status directory writes its report after every `RecordSync` (`ReplicaStatus::from_index` walks the index, T48 streamed it); the scale benchmark's servers have none. And tombstone GC lists every tombstone each cycle (O(tombstones), over the wire in remote mode).
  - **Results:** `cargo test`, `cargo test --features hooks` (both new checks on in every suite), clippy (with and without `hooks`) and the release stress test pass; the stress test also with `--features hooks`.
  - **Docs:** design §3 (Deletions), §6.1 (Per path), §6.4 (Kept with the entries, Incremental reconcile), §9 (both property tests, the oracle, the benchmark's heap and scan phase). No CLI, config or output change (two new debug log lines), so `docs/usage.md` and `README.md` are unchanged.
