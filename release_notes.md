# Release notes

All times are UTC.

## 0.1.12 — 2026.10.21 10:03:14

Fixes [issue #11](https://github.com/artem1579gh/files_sync/issues/11): a sync that replaced or deleted many files at once could run out of open files.

- **What happened:** each file a sync replaced or deleted held open files until the end of the sync. With the usual limit of 1024 open files, a sync that changed more than a few hundred files (e.g. after a `git checkout`) failed every further change with `Too many open files`. Rarely, an old file could also be left behind as `.~fsync.old.*` until the next start, and keep its directory from being removed.
- **Now:** `files_sync` raises its limit on open files to the hard limit (`ulimit -Hn`) when it starts. Replaced files in the same directory share what they hold open, and only so many of them are kept at a time. When that many are waiting, the sync removes the ones it can. On a filesystem without leases, it may pause for a fraction of a second until the oldest can go.
- **Also:** while that many are waiting, a replaced file that a program still has open for writing becomes a conflict copy at once, not after a minute. The program's later writes go there, as before.

## 0.1.11 — 2026.10.21 09:37:54

Fixes [issue #10](https://github.com/artem1579gh/files_sync/issues/10): a write by a program that kept a file open while a sync replaced or deleted it could be lost.

- **What happened:** the old file was removed 0.4 s after the sync replaced or deleted it, even when the program (an editor, a database, a logger) still had it open for writing. A write after that went to a removed file. Removing a directory did the same at once to the files deleted in it.
- **Now:** the old file is kept until the program closes it. If the program writes to it, or still has it open a minute later, it becomes a conflict copy, and the program's later writes go there. A directory being removed is kept, holding that conflict copy. A file that is only open for reading no longer delays the removal.
- **Also:** a write just before a sync replaced or deleted a file is now noticed even if it put the file's modification time back. Before, it could go unnoticed in files of 1 MiB or more. This needs a lease on the file: the sync's user owns it, and no other program has it open for writing at that moment.

## 0.1.10 — 2026.10.20 16:19:03

Fixes [issue #9](https://github.com/artem1579gh/files_sync/issues/9): with `copy-links` (`-L`) and the default `followed_write = "replace"`, a change from the other side beneath a followed directory link could delete files on the other side.

- **What happened:** the link is turned into a real copy of the directory before the change is applied. What is not synced in it (a dangling link, a link loop, a FIFO) is left out of the copy, and was then recorded as deleted, so the next sync deleted the other side's copies of those paths. For a loop where a directory used to be, that was the whole subtree.
- **Now:** those paths count as never seen on this side, so the other side's files are copied into the new directory instead. What the link pointed to is not touched.

## 0.1.9 — 2026.10.20 14:42:24

Fixes [issue #8](https://github.com/artem1579gh/files_sync/issues/8): after a crash, or a failed commit, recovery could leave `.~fsync.*` files behind for good.

- **`keep_dirlinks` (`-K`):** a commit through a kept link to a directory is now recovered through that same link. Before, recovery looked for it from the root, gave up, and left its temp, moved-aside and quarantined files in the link's directory. A file written through an open handle in that time stayed hidden there instead of becoming a conflict copy.
- **Renamed directories:** a commit whose directory was renamed before the restart is no longer given up on. It is recovered as soon as a scan finds the directory under its new name. It is dropped only once a complete scan shows the directory is gone from the tree.
- Journals written by older versions are still read.

## 0.1.8 — 2026.10.20 14:15:54

Fixes [issue #7](https://github.com/artem1579gh/files_sync/issues/7): `init --b-remote` wrote the server's private key on the client, and the guide had it copied to the server (copying the whole state directory also put the client's key there). Also fixes four smaller network issues.

- **Each key stays on its own host:** `init` no longer makes the key of a replica served elsewhere. The new `files_sync identity <pair> <side>` makes it on the server and prints its device ID, and the new `files_sync pin <pair> <side> <ID>` pins that ID on the client. Only the config, which holds no secrets, and a device ID go between the hosts. Existing pairs keep working; delete the client's key from the server.
- **`RecordSync`:** the server no longer trusts the client's values. It records the sync only for its connected peer, and keeps tombstones for the retention in its own config.
- **Harder to fingerprint:** the client sends no server name (SNI), and new certificates carry no name.
- **Escaped server text:** the server's error messages and refusal reasons are printed with control characters escaped, so a server cannot send escape sequences to your terminal.
- **`--sandbox`:** `init`, `identity` and `pin` now refuse it instead of silently ignoring it.

## 0.1.7 — 2026.10.19 19:57:13

Fixes [issue #6](https://github.com/artem1579gh/files_sync/issues/6): a replica root that held the pair's state directory, or overlapped the other root, was refused only by `init`, by its path, and only for local replicas. Serving a home directory with the default `~/.local/state` inside it synced the replica's private key and index to the peer, and never stopped syncing.

- **Checked at every open:** `sync`, `daemon` and `serve` now refuse a root that holds the state directory or lies inside it. They compare the directories themselves, not their paths, so symlinks do not hide it.
- **Overlapping roots:** also refused when one replica is served on the same host (the loopback setup), recognised by its root marker, and through symlinks.
- **Reserved temp names:** the config and status files are now written through temp files with the reserved `.~fsync.` prefix.

## 0.1.6 — 2026.10.19 14:11:00

Fixes [issue #5](https://github.com/artem1579gh/files_sync/issues/5): `serve` had no resource limits. Anyone who could reach its port could exhaust its threads and file descriptors before authenticating, and the peer could freeze it or make it allocate far more memory than a message's size suggests.

- **Connections:** at most 64 at a time, of which at most 8 in their handshake (4 from one IP address). A connection over a limit is closed at once. The whole handshake must be done within 10 s. Rejected handshakes are logged at `debug` only.
- **Stalled clients:** once a request has arrived, a client that makes no progress for 60 s is disconnected, so a client suspended in the middle of an upload holds up the other requests for at most that long, not for ever. `serve` now stops at once on SIGTERM, even then.
- **Memory:** a message may take at most 64 MiB once decoded, and the tombstone list at the end of a sync cycle is cut down to the server's own tombstones as it arrives. A block read can no longer ask for the same block twice.
- **Large scans:** a scan reports at most 10 000 changed and 10 000 unscannable paths. More changed paths are rescanned through their parent directories, so a tree with very many unreadable files no longer breaks every sync cycle.

## 0.1.5 — 2026.10.19 12:19:18

Fixes [issue #4](https://github.com/artem1579gh/files_sync/issues/4): over the network, a malicious peer could plant symlinks that made a replica following links read files outside its root (`copy-links`, `copy-unsafe-links`, `copy-dirlinks`), or write through them (`keep_dirlinks` with `keep_dirlinks_unsafe`).

- **Untrusted links are munged:** a replica no longer trusts the symlinks of a peer reached over the network. On a replica that could follow or adopt a link out of its root, a peer's link whose target is absolute or leaves the tree is written with the `/rsyncd-munged/` prefix, as rsync's daemon does. It is synced as the symlink it was sent as, but never followed or adopted. Safe targets, and links in local pairs, are unchanged.
- **New setting:** `trust_peer_links = true/false` per replica overrides the default (trusted for a local pair, untrusted over the network).
- **Docs:** the user guide describes what an authenticated peer can and cannot do with each symlink setting.

## 0.1.4 — 2026.10.06 10:31:00

Fixes [issue #3](https://github.com/artem1579gh/files_sync/issues/3): after a conflict, saving the winning file again before the sync finished could replace the new save with its own older version, without a conflict copy.

- **Conflict resolution:** the merged version now carries a counter of the replica that records it (the conflict's loser), not of the winner. A new save on the winner is then a conflict like any other, and both versions are kept.
- **Clock exchange:** the two replicas now compare their version clocks before every rescan within a sync, not only before the first scan.
- Also covered against an older `serve` (protocol v2), which cannot take part in the clock exchange.

## 0.1.3 — 2026.10.05 15:04:50

Fixes [issue #2](https://github.com/artem1579gh/files_sync/issues/2): a lost or restored index silently overwrote the replica's unsynced edits with the other side's older versions.

- **Clock exchange:** before each sync, the two replicas compare their version clocks, and the lower one is raised to the higher. A replica whose index was deleted or restored from a backup keeps its unsynced changes: they are copied to the other side, or kept as conflict copies where both sides changed a file. Files it deleted since its last sync come back from the other side.
- **Warning:** a replica whose index is new while its root was synced before logs `this replica was synced before, but its index is new (lost or deleted?)`.
- **Network protocol v3:** adds the clock exchange. Older peers still connect; an older `serve` does not protect its own replica's index.

## 0.1.2 — 2026.10.05 10:45:26

Fixes [issue #1](https://github.com/artem1579gh/files_sync/issues/1): an empty or unmounted replica root deleted every file on the other replica.

- **Root marker:** each replica root holds a `.~fsync.root.<replica-id>` file. A root without it is refused instead of being synced as "everything deleted". This covers, for example, the empty mount point of a disk that is not mounted. Existing pairs get their markers on their next sync.
- **Mass-deletion guard:** a sync that would delete more than `max_delete_percent` (default 50%) of a replica's entries, and more than 10 of them, holds those deletions back. `sync --once --allow-mass-delete` applies them; a daemon warns and keeps running.
- **Trash (optional):** with `trash_days` set on a replica, files its syncs replace or delete are kept in `.~fsync.trash/` for that many days instead of being removed.

## 0.1.1 — 2026.10.02 22:15:00

Initial release.

- Two-way sync between two local directories (`sync --once`, or the inotify-driven `daemon`), or over the network (`serve`, mutual TLS with pinned certificates, block-level deltas).
- Race-free by design: every change is a compare-and-swap commit, and nothing is written in place. Concurrent edits never cause silent data loss, torn files or writes outside a root.
- Version vectors per file. Real conflicts keep both versions, the loser as a `sync-conflict` copy.
- Every rsync symlink mode (`links`, `skip`, `copy-links`, `copy-unsafe-links`, `safe-links`, `copy-dirlinks`, `--munge-links`, `-K`).
- Crash recovery through an intent journal, tombstone garbage collection, an optional Landlock sandbox and `status`.
