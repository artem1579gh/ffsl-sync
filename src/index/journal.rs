//! The intent journal (design §5.3 step 1, §5.8).
//!
//! A commit that creates reserved (`.~ffsl.*`) names, or changes a file's
//! mode and mtime in place, first records an [`Intent`] here, in the replica's index database, and commits it durably.
//! The intent names every reserved name the commit may use (chosen up front)
//! and, once known, the identity of the objects involved. After a crash,
//! [`LocalReplica::open`](crate::replica::LocalReplica::open) replays every
//! intent still recorded ([`commit::recover`](crate::fs::commit::recover)):
//! our own staged objects are removed, user objects caught at a reserved name
//! are verified and then quarantined or put back, and quarantined old inodes
//! are quarantined again. A reserved name that no intent mentions is not
//! ours, so it is left alone.
//!
//! States, in order (an intent may skip some):
//! - [`Started`](IntentState::Started): names chosen; nothing of ours exists
//!   yet, and nothing of the user's has been moved to a reserved name;
//! - [`TempWritten`](IntentState::TempWritten): the staged object N is
//!   recorded (`staged`), always before anything is exchanged with it;
//! - [`Exchanged`](IntentState::Exchanged): the user's object was moved to a
//!   reserved name (exchange, or move-aside for a delete or rename);
//! - [`Quarantined`](IntentState::Quarantined): the commit is done, but the
//!   old inode waits in quarantine under the `old` name.
//!
//! Done is no state: the record is deleted, in the same transaction as the
//! index update for a successful `apply` ([`WriteTxn::finish_intents`]).
//! Replay inspects the names rather than trusting the state, so updates other
//! than the first record and [`TempWritten`](IntentState::TempWritten) need
//! not be durable, and replaying an intent that already finished does
//! nothing (its reserved names are empty).

use std::sync::{Arc, Mutex};

use redb::{Database, Durability, ReadableDatabase, ReadableTable, TableDefinition};
use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::fs::commit::Expected;
use crate::fs::{Fingerprint, RelPath};
use crate::index::store::WriteTxn;

pub(crate) const INTENTS: TableDefinition<u64, &[u8]> = TableDefinition::new("intents");

/// Key of an intent record.
pub type IntentId = u64;

/// The kind of commit an intent belongs to; it gives `tmp` its role.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum IntentOp {
    /// `create_file`, `create_symlink`, `mkdir`: `tmp` holds the staged object.
    Create,
    /// `replace_file`, `replace_symlink`: `tmp` holds the staged object, and
    /// after the exchange the old one; `old` is its quarantine name.
    Replace,
    /// `delete`: `tmp` is the `.del.` name the target is moved aside to;
    /// `old` its quarantine name.
    Delete,
    /// `rename_to`: `tmp` is the name the target is moved aside to.
    Rename,
    /// `materialize`: like `Replace`, but the staged object is a directory
    /// tree, all of it ours (removed with its content).
    Materialize,
    /// `set_file_meta` changing both the mode and the mtime in place. It
    /// uses no reserved name (`tmp` is empty); `expected` is the file as
    /// checked, and `staged` the same with the new mode and mtime. Replay
    /// puts the old mode back if the crash came between the two changes.
    SetMeta,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum IntentState {
    Started,
    TempWritten,
    Exchanged,
    Quarantined,
}

/// One commit in progress (or one quarantined old inode).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Intent {
    pub op: IntentOp,
    /// The commit's target, as a replica path.
    pub path: RelPath,
    /// The target's parent directory at resolution; replay only works in
    /// that same directory (wherever it is now).
    pub parent: Fingerprint,
    /// The staging or move-aside name (see [`IntentOp`]).
    pub tmp: Vec<u8>,
    /// The quarantine name for the old inode (replace, delete).
    pub old: Option<Vec<u8>>,
    /// The object the commit expects at `path` (replace, delete, rename).
    pub expected: Option<Expected>,
    /// The staged object N, once it is known.
    pub staged: Option<Fingerprint>,
    pub state: IntentState,
    /// The replica path of the directory the commit resolved `path` from:
    /// the root, or a directory link written through (`-K`, design
    /// §4.3.1). Replay goes the same way (issue #8). Last, so a record
    /// without it is told apart ([`LegacyIntent`]).
    pub base: RelPath,
}

/// An intent as recorded before `base` (0.1.8 and earlier): its `path` is
/// relative to the directory the commit resolved it from, the root or a
/// `-K` link's, which it does not say. Read with `base` = the root; replay
/// finds a `-K` one's directory by its identity, like a moved one.
#[derive(Deserialize)]
struct LegacyIntent {
    op: IntentOp,
    path: RelPath,
    parent: Fingerprint,
    tmp: Vec<u8>,
    old: Option<Vec<u8>>,
    expected: Option<Expected>,
    staged: Option<Fingerprint>,
    state: IntentState,
}

impl From<LegacyIntent> for Intent {
    fn from(i: LegacyIntent) -> Intent {
        Intent {
            op: i.op,
            path: i.path,
            parent: i.parent,
            tmp: i.tmp,
            old: i.old,
            expected: i.expected,
            staged: i.staged,
            state: i.state,
            base: RelPath::root(),
        }
    }
}

fn dberr(e: impl Into<redb::Error>) -> Error {
    Error::Db(e.into())
}

fn encode(intent: &Intent) -> Result<Vec<u8>> {
    postcard::to_stdvec(intent).map_err(|e| Error::BadIndex {
        reason: format!("encode intent: {e}"),
    })
}

fn decode(id: IntentId, value: &[u8]) -> Result<Intent> {
    // A legacy record ends where `base` would start.
    postcard::from_bytes(value)
        .or_else(|e| {
            postcard::from_bytes::<LegacyIntent>(value)
                .map(Intent::from)
                .map_err(|_| e)
        })
        .map_err(|e| Error::BadIndex {
            reason: format!("intent {id}: {e}"),
        })
}

/// The `intents` table of one index database.
///
/// Commits record intents through [`Journal::begin`] and [`Journal::update`];
/// the ids they begin are collected until [`Journal::take_open`], so the
/// caller can finish them together with its index update.
pub struct Journal {
    db: Arc<Database>,
    open: Mutex<Vec<IntentId>>,
}

impl std::fmt::Debug for Journal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Journal").finish_non_exhaustive()
    }
}

impl Journal {
    /// The journal in `db`, whose `intents` table must exist.
    pub(crate) fn new(db: Arc<Database>) -> Journal {
        Journal {
            db,
            open: Mutex::new(Vec::new()),
        }
    }

    /// A journal in memory, for tests of the commit functions alone.
    pub fn in_memory() -> Result<Journal> {
        let db = Database::builder()
            .create_with_backend(redb::backends::InMemoryBackend::new())
            .map_err(dberr)?;
        let txn = db.begin_write().map_err(dberr)?;
        txn.open_table(INTENTS).map_err(dberr)?;
        txn.commit().map_err(dberr)?;
        Ok(Journal::new(Arc::new(db)))
    }

    fn write(
        &self,
        durable: bool,
        f: impl FnOnce(&mut redb::Table<u64, &[u8]>) -> Result<()>,
    ) -> Result<()> {
        let mut txn = self.db.begin_write().map_err(dberr)?;
        if !durable {
            txn.set_durability(Durability::None).map_err(dberr)?;
        }
        {
            let mut table = txn.open_table(INTENTS).map_err(dberr)?;
            f(&mut table)?;
        }
        txn.commit().map_err(dberr)
    }

    /// Records a new intent durably and returns its id. The id stays open
    /// until [`Journal::take_open`].
    pub fn begin(&self, intent: &Intent) -> Result<IntentId> {
        let value = encode(intent)?;
        let mut id = 0;
        self.write(true, |t| {
            id = t.last().map_err(dberr)?.map_or(0, |(k, _)| k.value()) + 1;
            t.insert(id, value.as_slice()).map_err(dberr)?;
            Ok(())
        })?;
        self.open.lock().expect("journal lock").push(id);
        Ok(id)
    }

    /// Rewrites the intent `id`; `durable` waits until it is on disk.
    pub fn update(&self, id: IntentId, intent: &Intent, durable: bool) -> Result<()> {
        let value = encode(intent)?;
        self.write(durable, |t| {
            t.insert(id, value.as_slice()).map_err(dberr)?;
            Ok(())
        })
    }

    /// The ids begun since the last call, oldest first.
    pub fn take_open(&self) -> Vec<IntentId> {
        std::mem::take(&mut *self.open.lock().expect("journal lock"))
    }

    /// Every recorded intent, by id.
    pub fn pending(&self) -> Result<Vec<(IntentId, Intent)>> {
        let txn = self.db.begin_read().map_err(dberr)?;
        let table = txn.open_table(INTENTS).map_err(dberr)?;
        let mut out = Vec::new();
        for row in table.iter().map_err(dberr)? {
            let (k, v) = row.map_err(dberr)?;
            out.push((k.value(), decode(k.value(), v.value())?));
        }
        Ok(out)
    }

    /// Finishes intents without an index update (a failed commit, or a
    /// replayed one): see [`WriteTxn::finish_intents`]. Not durable.
    pub fn finish(&self, done: &[IntentId], quarantined: &[IntentId]) -> Result<()> {
        if done.is_empty() && quarantined.is_empty() {
            return Ok(());
        }
        self.write(false, |t| finish(t, done, quarantined))
    }

    /// Deletes the records of quarantine entries that were settled. Not
    /// durable: replaying one finds its `old` name empty or not ours.
    pub fn forget(&self, ids: &[IntentId]) -> Result<()> {
        self.finish(ids, &[])
    }
}

/// Deletes the `done` records and marks the `quarantined` ones
/// [`IntentState::Quarantined`].
fn finish(
    t: &mut redb::Table<u64, &[u8]>,
    done: &[IntentId],
    quarantined: &[IntentId],
) -> Result<()> {
    for &id in done {
        t.remove(id).map_err(dberr)?;
    }
    for &id in quarantined {
        let Some(value) = t.get(id).map_err(dberr)?.map(|v| v.value().to_vec()) else {
            continue;
        };
        let mut intent = decode(id, &value)?;
        intent.state = IntentState::Quarantined;
        t.insert(id, encode(&intent)?.as_slice()).map_err(dberr)?;
    }
    Ok(())
}

impl WriteTxn {
    /// Finishes intents in this transaction, so they are done exactly when
    /// the index update is (§5.3 step 5): the `done` records are deleted,
    /// and the `quarantined` ones (whose old inode is still in quarantine)
    /// are kept as [`IntentState::Quarantined`].
    pub fn finish_intents(&mut self, done: &[IntentId], quarantined: &[IntentId]) -> Result<()> {
        let mut t = self.raw().open_table(INTENTS).map_err(dberr)?;
        finish(&mut t, done, quarantined)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fs::FileKind;

    fn fp(ino: u64) -> Fingerprint {
        Fingerprint {
            dev: 1,
            ino,
            size: 3,
            mtime_ns: -5,
            ctime_ns: 7,
            mode: 0o644,
            kind: FileKind::File,
        }
    }

    fn intent(op: IntentOp) -> Intent {
        Intent {
            op,
            path: RelPath::new(&b"d/f\xff"[..]).unwrap(),
            parent: fp(2),
            tmp: b".~ffsl.0123456789abcdef".to_vec(),
            old: Some(b".~ffsl.old.0123456789abcdef".to_vec()),
            expected: Some(Expected {
                fp: fp(3),
                hash: Some([7; 32]),
                racy: true,
            }),
            staged: None,
            state: IntentState::Started,
            base: RelPath::new("d").unwrap(),
        }
    }

    #[test]
    fn begin_update_finish() {
        let j = Journal::in_memory().unwrap();
        let a = j.begin(&intent(IntentOp::Replace)).unwrap();
        let b = j.begin(&intent(IntentOp::Delete)).unwrap();
        let c = j.begin(&intent(IntentOp::Create)).unwrap();
        assert!(a < b && b < c);
        assert_eq!(j.take_open(), [a, b, c]);
        assert!(j.take_open().is_empty());

        let mut i = intent(IntentOp::Replace);
        i.staged = Some(fp(9));
        i.state = IntentState::TempWritten;
        j.update(a, &i, false).unwrap();
        let all = j.pending().unwrap();
        assert_eq!(all.len(), 3);
        assert_eq!(all[0], (a, i.clone()));

        j.finish(&[b], &[a]).unwrap();
        let all = j.pending().unwrap();
        assert_eq!(all.iter().map(|(id, _)| *id).collect::<Vec<_>>(), [a, c]);
        assert_eq!(all[0].1.state, IntentState::Quarantined);
        j.forget(&[a, c, 99]).unwrap();
        assert!(j.pending().unwrap().is_empty());
    }

    /// A record from before `base` (issue #8) still reads, with the root
    /// as its base; a truncated record does not.
    #[test]
    fn legacy_records_are_read() {
        #[derive(Serialize)]
        struct Legacy {
            op: IntentOp,
            path: RelPath,
            parent: Fingerprint,
            tmp: Vec<u8>,
            old: Option<Vec<u8>>,
            expected: Option<Expected>,
            staged: Option<Fingerprint>,
            state: IntentState,
        }
        let i = intent(IntentOp::Replace);
        let old = postcard::to_stdvec(&Legacy {
            op: i.op,
            path: i.path.clone(),
            parent: i.parent,
            tmp: i.tmp.clone(),
            old: i.old.clone(),
            expected: i.expected,
            staged: Some(fp(9)),
            state: IntentState::Exchanged,
        })
        .unwrap();
        let read = decode(1, &old).unwrap();
        assert_eq!(
            read,
            Intent {
                staged: Some(fp(9)),
                state: IntentState::Exchanged,
                base: RelPath::root(),
                ..i.clone()
            }
        );
        let new = encode(&i).unwrap();
        assert_eq!(decode(2, &new).unwrap(), i);
        assert!(decode(3, &old[..old.len() - 1]).is_err());
    }
}
