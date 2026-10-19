# Release notes

All times are UTC.

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
