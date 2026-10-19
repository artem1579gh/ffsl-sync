//! Symlinks from an untrusted peer never lead out of the root (design §4.6,
//! issue #4). A replica that follows links (`-L`, `--copy-unsafe-links`,
//! `-k`) or adopts them out of its root (`-K` with `keep_dirlinks_unsafe`)
//! writes a peer's unsafe target munged, so it leads nowhere, and indexes it
//! as the plain symlink it was sent as. The peer is untrusted when it is
//! reached over the network (remote mode here), unless `trust_peer_links`
//! says otherwise; a local peer is trusted (rsync's semantics, unchanged).

mod harness;

use std::fs;
use std::io::Read;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use files_sync::config::SymlinkPolicy::{self, *};
use files_sync::fs::commit::FileMeta;
use files_sync::index::{Entry, Kind, VersionVector};
use files_sync::replica::{Op, Outcome, Precondition, Replica};
use files_sync::scan::Scope;
use harness::*;

/// A case's directory `P`: the roots `a/` and `b/`, the state directory,
/// and `outside/` with a secret file and directory next to them.
struct World {
    dir: tempfile::TempDir,
    pair: Pair,
}

impl World {
    fn new(m: Mode, a: impl Into<Opts>, b: impl Into<Opts>) -> World {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path();
        for d in ["a", "b", "state", "outside/sd"] {
            fs::create_dir_all(p.join(d)).unwrap();
        }
        fs::write(p.join("outside/secret"), "SECRET").unwrap();
        fs::write(p.join("outside/sd/key"), "KEY").unwrap();
        let pair = Pair::open_at_with(&p.join("a"), &p.join("b"), &p.join("state"), a, b).over(m);
        World { dir, pair }
    }

    fn outside(&self, rel: &str) -> PathBuf {
        self.dir
            .path()
            .canonicalize()
            .unwrap()
            .join("outside")
            .join(rel)
    }
}

const FOLLOWING: [SymlinkPolicy; 3] = [CopyLinks, CopyUnsafeLinks, CopyDirlinks];

fn untrusted(m: Mode) -> bool {
    m == Mode::Remote
}

fn munged(target: &str) -> String {
    format!("/rsyncd-munged/{target}")
}

fn vv_of(id: files_sync::config::ReplicaId) -> VersionVector {
    let mut vv = VersionVector::new();
    vv.bump(id);
    vv
}

fn symlink_op(target: &str) -> Op {
    Op::Symlink {
        target: target.as_bytes().to_vec(),
        mtime_ns: 0,
        vv: vv_of(ID_A),
    }
}

/// The indexed entry at `rel`, as the peer sees it.
fn entry_of(r: &dyn Replica, rel: &str) -> Option<Entry> {
    r.changes_since(0)
        .unwrap()
        .into_iter()
        .find(|(p, _)| *p == rp(rel))
        .map(|(_, e)| e)
}

fn contents(root: &Path) -> Vec<String> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for e in fs::read_dir(&dir).unwrap() {
            let path = e.unwrap().path();
            if path
                .file_name()
                .unwrap()
                .as_bytes()
                .starts_with(b".~fsync.")
            {
                continue;
            }
            let md = path.symlink_metadata().unwrap();
            if md.is_dir() {
                stack.push(path);
            } else if md.is_file() {
                out.push(fs::read_to_string(&path).unwrap());
            }
        }
    }
    out
}

/// The issue's read attack, step by step over the protocol: the peer plants
/// a link to a file (and to a directory) outside the root, asks for a scan,
/// then for the content. An untrusted peer gets a symlink, never the file.
fn planted_link_is_not_followed(m: Mode) {
    for policy in FOLLOWING {
        let mut w = World::new(m, Links, policy);
        let secret = w.outside("secret").display().to_string();
        let sd = w.outside("sd").display().to_string();
        let b = &mut w.pair.b.replica;
        for (at, target) in [
            ("x", secret.as_str()),
            ("d", sd.as_str()),
            ("r", "../outside/secret"),
        ] {
            let out = b.apply(&rp(at), symlink_op(target), Precondition::Absent, None);
            assert!(
                matches!(out, Ok(Outcome::Applied(_))),
                "{policy:?} {at}: {out:?}"
            );
        }
        b.scan(Scope::Paths(vec![rp("x"), rp("d"), rp("r")]))
            .unwrap();
        for (at, target) in [
            ("x", secret.as_str()),
            ("d", sd.as_str()),
            ("r", "../outside/secret"),
        ] {
            let e = entry_of(&w.pair.b.replica, at).unwrap();
            let raw = w.pair.b.readlink(at);
            if untrusted(m) {
                let want = Kind::Symlink {
                    target: target.as_bytes().to_vec(),
                };
                assert_eq!(e.kind, want, "{policy:?} {at}");
                assert_eq!(raw, munged(target), "{policy:?} {at}");
            } else {
                assert_eq!(raw, target, "{policy:?} {at}");
            }
        }
        // A forged expectation does not get the secret either.
        let forged = Entry::new(
            Kind::File {
                size: 6,
                hash: *blake3::hash(b"SECRET").as_bytes(),
            },
            0o644,
            0,
            vv_of(ID_B),
        );
        let b = &w.pair.b.replica;
        if untrusted(m) {
            let read = b.open_read(&rp("x"), &forged).and_then(|mut r| {
                let mut s = String::new();
                r.read_to_string(&mut s)
                    .map_err(|e| files_sync::Error::io("read", e))?;
                Ok(s)
            });
            assert!(read.is_err(), "{policy:?}: {read:?}");
        }
    }
}

/// Through the engine: what the peer's links point to outside B's root
/// never arrives at the peer, and the pair converges on the links.
fn planted_link_syncs_as_a_link(m: Mode) {
    for policy in FOLLOWING {
        let mut w = World::new(m, Links, policy);
        let secret = w.outside("secret").display().to_string();
        let sd = w.outside("sd").display().to_string();
        w.pair.a.symlink("x", &secret);
        w.pair.a.symlink("d", &sd);
        w.pair.sync();
        w.pair.sync();
        if !untrusted(m) {
            continue;
        }
        assert_eq!(w.pair.b.readlink("x"), munged(&secret), "{policy:?}");
        assert_eq!(w.pair.b.readlink("d"), munged(&sd), "{policy:?}");
        assert_eq!(w.pair.a.readlink("x"), secret, "{policy:?}");
        assert_eq!(w.pair.a.readlink("d"), sd, "{policy:?}");
        assert!(contents(w.pair.a.root()).is_empty(), "{policy:?}");
        w.pair.assert_converged();
        // The peer can still delete it.
        w.pair.a.rm("x");
        w.pair.sync();
        assert!(!w.pair.b.exists("x"), "{policy:?}");
        w.pair.assert_converged();
    }
}

/// The issue's write attack: with `-K` and `keep_dirlinks_unsafe`, the
/// peer plants a link to a directory outside the root, asks for its
/// adoption, and writes through it. An untrusted peer's link is not adopted,
/// so nothing is written outside.
fn planted_link_is_not_adopted(m: Mode) {
    let kept = Opts {
        keep_dirlinks: true,
        keep_dirlinks_unsafe: true,
        ..Opts::default()
    };
    let mut w = World::new(m, Links, kept);
    let sd = w.outside("sd").display().to_string();
    let b = &mut w.pair.b.replica;
    let out = b.apply(&rp("d"), symlink_op(&sd), Precondition::Absent, None);
    assert!(matches!(out, Ok(Outcome::Applied(_))), "{out:?}");
    let adopted = b.adopt(&rp("d")).unwrap();
    assert_eq!(adopted, !untrusted(m));
    if !untrusted(m) {
        return;
    }
    let data = b"ssh-ed25519 AAAA attacker";
    let op = Op::WriteFile {
        meta: FileMeta {
            mode: 0o600,
            mtime_ns: 0,
        },
        hash: *blake3::hash(data).as_bytes(),
        vv: vv_of(ID_A),
    };
    let mut content: &[u8] = data;
    let out = b.apply(
        &rp("d/authorized_keys"),
        op,
        Precondition::Absent,
        Some(&mut content),
    );
    assert!(!matches!(out, Ok(Outcome::Applied(_))), "{out:?}");
    assert!(!w.outside("sd/authorized_keys").exists());
    assert_eq!(w.pair.b.readlink("d"), munged(&sd));
}

/// Beneath a directory adopted out of the root (`-K`, the user's own link,
/// `keep_dirlinks_unsafe`), a link is judged relative to that directory:
/// `k/l -> ../sd` stays inside the tree as the peer sees it, but on disk it
/// would leave the adopted directory.
fn link_beneath_an_adopted_directory(m: Mode) {
    let kept = Opts {
        keep_dirlinks: true,
        keep_dirlinks_unsafe: true,
        ..Opts::default()
    };
    let mut w = World::new(m, Links, kept);
    w.pair.b.symlink("k", "../outside/sd");
    w.pair.a.write("k/y", "Y");
    w.pair.sync();
    assert!(w.pair.b.adopted("k"));
    w.pair.a.symlink("k/l", "../sd");
    w.pair.sync();
    let want = if untrusted(m) {
        munged("../sd")
    } else {
        "../sd".to_owned()
    };
    let raw = fs::read_link(w.outside("sd/l")).unwrap();
    assert_eq!(raw.to_str().unwrap(), want);
    w.pair.assert_converged();
}

/// `trust_peer_links` overrides the default both ways.
fn trust_peer_links_overrides_the_default(m: Mode) {
    for trust in [false, true] {
        let b = Opts {
            policy: CopyLinks,
            trust_peer_links: Some(trust),
            ..Opts::default()
        };
        let mut w = World::new(m, Links, b);
        let secret = w.outside("secret").display().to_string();
        let out = w
            .pair
            .b
            .replica
            .apply(&rp("x"), symlink_op(&secret), Precondition::Absent, None);
        assert!(matches!(out, Ok(Outcome::Applied(_))), "{out:?}");
        let want = if trust {
            secret.clone()
        } else {
            munged(&secret)
        };
        assert_eq!(w.pair.b.readlink("x"), want, "trust = {trust}");
    }
}

/// A safe target is written as it is, also from an untrusted peer.
fn safe_targets_are_kept(m: Mode) {
    let mut w = World::new(m, Links, CopyLinks);
    w.pair.a.write("f", "F");
    w.pair.a.mkdir("s");
    w.pair.sync();
    let b = &mut w.pair.b.replica;
    let out = b.apply(&rp("s/l"), symlink_op("../f"), Precondition::Absent, None);
    assert!(matches!(out, Ok(Outcome::Applied(_))), "{out:?}");
    assert_eq!(w.pair.b.readlink("s/l"), "../f");
}

/// A link copied when a followed directory is turned into a real one
/// (`-k`, an incoming change beneath it) lands nearer the root than it was,
/// so its target is judged at its new place: `t/u/l -> ../../f` is safe,
/// but `m/l -> ../../f` (for `m -> t/u`) would leave the root.
fn materialized_link_is_judged_at_its_new_place(m: Mode) {
    let mut w = World::new(m, CopyDirlinks, CopyDirlinks);
    let b = &w.pair.b;
    b.write("f", "F");
    b.mkdir("t/u");
    b.symlink("t/u/l", "../../f");
    b.symlink("m", "t/u");
    w.pair.sync();
    assert!(w.pair.a.is_dir("m"));
    w.pair.a.write("m/new", "N");
    w.pair.sync();
    let b = &w.pair.b;
    assert!(b.is_dir("m"), "{:?}", b.ls());
    assert_eq!(b.read("m/new"), "N");
    assert_eq!(b.readlink("t/u/l"), "../../f");
    let want = if untrusted(m) {
        munged("../../f")
    } else {
        "../../f".to_owned()
    };
    assert_eq!(b.readlink("m/l"), want);
    assert_eq!(w.pair.a.readlink("m/l"), want);
    w.pair.assert_converged();
}

both_modes! {
    planted_link_is_not_followed,
    planted_link_syncs_as_a_link,
    planted_link_is_not_adopted,
    link_beneath_an_adopted_directory,
    trust_peer_links_overrides_the_default,
    safe_targets_are_kept,
    materialized_link_is_judged_at_its_new_place,
}
