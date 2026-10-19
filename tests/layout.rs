//! A replica root that holds the pair's state directory, or overlaps the
//! other root, is refused whenever a replica is opened, not only by `init`
//! (issue #6): when served, behind a symlink, and over loopback.

use std::fs;
use std::io::{BufRead, BufReader};
use std::os::unix::fs::symlink;
use std::path::Path;
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use files_sync::config::PairConfig;

fn bin(state_home: &Path, args: &[&str]) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_files_sync"));
    cmd.args(args)
        .env("XDG_STATE_HOME", state_home)
        .env_remove("RUST_LOG");
    cmd
}

fn run(state_home: &Path, args: &[&str]) -> Output {
    bin(state_home, args).output().unwrap()
}

fn ok(state_home: &Path, args: &[&str]) -> String {
    let out = run(state_home, args);
    assert!(
        out.status.success(),
        "{args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap()
}

/// `child`'s output once it exits; it must exit within 20 s.
fn exits(mut child: Child) -> Output {
    let end = Instant::now() + Duration::from_secs(20);
    while child.try_wait().unwrap().is_none() {
        if Instant::now() > end {
            let _ = child.kill();
            panic!("still running: {:?}", child.wait_with_output().unwrap());
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    child.wait_with_output().unwrap()
}

fn serve(state_home: &Path) -> Child {
    bin(state_home, &["serve", "p", "b", "--listen", "127.0.0.1:0"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap()
}

fn refused(out: &Output, reason: &str) {
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "{out:?}");
    assert!(err.contains("invalid replica root"), "{err}");
    assert!(err.contains(reason), "{err}");
}

/// The issue's case: B is the server's `$HOME`, with the default state
/// home inside it. Before the fix, `serve` ran, synced its own key and
/// index, and its report started a new cycle after every one.
#[test]
fn serve_refuses_a_root_that_holds_its_state_directory() {
    let t = tempfile::tempdir().unwrap();
    let (home, a) = (t.path().join("home"), t.path().join("a"));
    let state_home = home.join(".local/state");
    fs::create_dir_all(&state_home).unwrap();
    fs::create_dir_all(&a).unwrap();
    let (a, b) = (a.to_str().unwrap(), home.to_str().unwrap());
    // `init` does not check a remote root.
    ok(
        &state_home,
        &["init", "p", "--a", a, "--b", b, "--b-remote", "127.0.0.1:9"],
    );
    let out = exits(serve(&state_home));
    refused(&out, "contains the state directory");
    // Refused before anything was written to it.
    assert!(fs::read_dir(&home).unwrap().all(|e| {
        !e.unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".~fsync.root.")
    }));
}

/// The config changed after `init`: B's root is now a symlink to a
/// directory that holds the state home. A lexical check misses it.
#[test]
fn sync_refuses_a_state_directory_behind_a_symlink() {
    let t = tempfile::tempdir().unwrap();
    let (a, b, outer) = (
        t.path().join("a"),
        t.path().join("b"),
        t.path().join("outer"),
    );
    let state_home = outer.join("state");
    for d in [&a, &b, &state_home] {
        fs::create_dir_all(d).unwrap();
    }
    let s = |p: &Path| p.to_str().unwrap().to_owned();
    ok(&state_home, &["init", "p", "--a", &s(&a), "--b", &s(&b)]);

    let link = t.path().join("link");
    symlink(&outer, &link).unwrap();
    let mut cfg = PairConfig::load(&state_home, "p").unwrap();
    cfg.replicas[1].root = link;
    cfg.save(&state_home).unwrap();
    let out = run(&state_home, &["sync", "--once", "p"]);
    refused(&out, "contains the state directory");
    // Only A's marker (A is opened first). Before the fix, the key and the
    // index arrived in A.
    let names: Vec<_> = fs::read_dir(&a)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert!(
        names.len() == 1 && names[0].starts_with(".~fsync.root."),
        "{names:?}"
    );
}

/// Both replicas on one host, B served on 127.0.0.1: `init` checks no
/// overlap with a remote root, so B inside A got through. A finds B's
/// marker at B's path here.
#[test]
fn overlapping_roots_over_loopback_are_refused() {
    for b_inside_a in [true, false] {
        let t = tempfile::tempdir().unwrap();
        let state_home = t.path().join("state");
        let (a, b) = if b_inside_a {
            (t.path().join("a"), t.path().join("a/sub"))
        } else {
            (t.path().join("b/sub"), t.path().join("b"))
        };
        for d in [&a, &b, &state_home] {
            fs::create_dir_all(d).unwrap();
        }
        let s = |p: &Path| p.to_str().unwrap().to_owned();
        ok(
            &state_home,
            &[
                "init",
                "p",
                "--a",
                &s(&a),
                "--b",
                &s(&b),
                "--b-remote",
                "127.0.0.1:9",
            ],
        );
        let mut server = serve(&state_home);
        let mut line = String::new();
        BufReader::new(server.stdout.as_mut().unwrap())
            .read_line(&mut line)
            .unwrap();
        let addr = line.trim().rsplit(" on ").next().unwrap().to_owned();
        assert!(addr.starts_with("127.0.0.1:"), "{line:?}");
        let mut cfg = PairConfig::load(&state_home, "p").unwrap();
        cfg.replicas[1].remote = Some(addr);
        cfg.save(&state_home).unwrap();

        let out = run(&state_home, &["sync", "--once", "p"]);
        let reason = if b_inside_a {
            "contains"
        } else {
            "lies inside"
        };
        refused(&out, reason);
        let _ = server.kill();
        let _ = server.wait();
    }
}
