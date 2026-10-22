//! Two processes over 127.0.0.1 (T21): `serve` runs replica B, and
//! `sync --once` / `daemon` run replica A against it. Each side has its own
//! state home, as two hosts would: the server's gets the config, copied
//! from the client's after `init`, and makes B's identity itself, whose
//! device ID the client pins (issue #7).

use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use ffsl_sync::config::{self, PairConfig};
use ffsl_sync::tls::{DeviceId, Identity};
use tempfile::TempDir;

fn bin(state_home: &Path, args: &[&str]) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_ffsl-sync"));
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

/// Stops `child` with SIGTERM and waits; it must exit successfully.
fn terminate(mut child: Child) -> Output {
    let pid = child.id() as libc::pid_t;
    // SAFETY: plain kill(2) on our own child.
    assert_eq!(unsafe { libc::kill(pid, libc::SIGTERM) }, 0);
    let end = Instant::now() + Duration::from_secs(20);
    loop {
        if child.try_wait().unwrap().is_some() {
            break;
        }
        if Instant::now() > end {
            let _ = child.kill();
            panic!("pid {pid} did not stop on SIGTERM");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success(), "{:?}", out.status);
    out
}

fn wait_for(path: &Path, content: &[u8]) {
    let end = Instant::now() + Duration::from_secs(20);
    while fs::read(path).ok().as_deref() != Some(content) {
        assert!(Instant::now() < end, "{} never arrived", path.display());
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Files and their contents under `root` (no reserved names).
fn files(root: &Path) -> Vec<(PathBuf, Vec<u8>)> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for e in fs::read_dir(&dir).unwrap() {
            let e = e.unwrap();
            if e.file_name().to_string_lossy().starts_with(".~ffsl.") {
                continue;
            }
            let path = e.path();
            if e.file_type().unwrap().is_dir() {
                stack.push(path.clone());
                out.push((path.strip_prefix(root).unwrap().to_owned(), Vec::new()));
            } else {
                let data = fs::read(&path).unwrap();
                out.push((path.strip_prefix(root).unwrap().to_owned(), data));
            }
        }
    }
    out.sort();
    out
}

struct Hosts {
    client: TempDir,
    server: TempDir,
    ra: TempDir,
    rb: TempDir,
    server_log: PathBuf,
}

impl Hosts {
    /// `init` on the client's host, the config copied to the server's,
    /// B's identity made there and pinned on the client's.
    fn new() -> Hosts {
        let t = || tempfile::tempdir().unwrap();
        let hosts = Hosts {
            client: t(),
            server: t(),
            ra: t(),
            rb: t(),
            server_log: PathBuf::new(),
        };
        let (a, b) = (
            hosts.ra.path().to_str().unwrap(),
            hosts.rb.path().to_str().unwrap(),
        );
        let out = ok(
            hosts.client.path(),
            &["init", "p", "--a", a, "--b", b, "--b-remote", "127.0.0.1:9"],
        );
        assert!(out.contains("served at 127.0.0.1:9"), "{out}");
        assert!(out.contains("ffsl-sync identity p b"), "{out}");
        let (from, to) = (
            hosts.pair_dir(hosts.client.path()),
            hosts.pair_dir(hosts.server.path()),
        );
        fs::create_dir_all(&to).unwrap();
        fs::copy(from.join(config::CONFIG_FILE), to.join(config::CONFIG_FILE)).unwrap();
        let out = ok(hosts.server.path(), &["identity", "p", "b"]);
        let pin = out
            .lines()
            .find_map(|l| {
                l.trim()
                    .strip_prefix("on the host that syncs the pair, pin it with: ")
            })
            .unwrap_or_else(|| panic!("no pin command in {out}"));
        let args: Vec<&str> = pin.split(' ').skip(1).collect();
        assert_eq!(args[..3], ["pin", "p", "b"], "{pin}");
        ok(hosts.client.path(), &args);
        // The configs agree, and each key is on its own host only.
        let cfg = hosts.cfg();
        assert_eq!(PairConfig::load(hosts.server.path(), "p").unwrap(), cfg);
        for (side, here, there) in [(0, &from, &to), (1, &to, &from)] {
            for f in <[PathBuf; 2]>::from(Identity::paths(here, cfg.replicas[side].id)) {
                assert!(f.exists(), "{}", f.display());
                assert!(
                    !there.join(f.file_name().unwrap()).exists(),
                    "{}",
                    f.display()
                );
            }
        }
        Hosts {
            server_log: hosts.server.path().join("serve.log"),
            ..hosts
        }
    }

    fn pair_dir(&self, home: &Path) -> PathBuf {
        config::pair_dir(home, "p").unwrap()
    }

    fn cfg(&self) -> PairConfig {
        PairConfig::load(self.client.path(), "p").unwrap()
    }

    fn set_cfg(&self, cfg: &PairConfig) {
        cfg.save(self.client.path()).unwrap();
    }

    /// Starts `serve p b` on a free port; returns it with its address.
    fn serve(&self) -> (Child, String) {
        self.serve_with(&[])
    }

    /// [`Hosts::serve`] with global `flags`.
    fn serve_with(&self, flags: &[&str]) -> (Child, String) {
        let log = fs::File::create(&self.server_log).unwrap();
        let mut args = flags.to_vec();
        args.extend(["serve", "p", "b", "--listen", "127.0.0.1:0"]);
        let mut child = bin(self.server.path(), &args)
            // Rejected handshakes are logged at debug (issue #5).
            .env("RUST_LOG", "info,ffsl_sync::server=debug")
            .stdout(Stdio::piped())
            .stderr(log)
            .spawn()
            .unwrap();
        let mut line = String::new();
        BufReader::new(child.stdout.as_mut().unwrap())
            .read_line(&mut line)
            .unwrap();
        let addr = line
            .trim()
            .rsplit(" on ")
            .next()
            .unwrap_or_else(|| panic!("no address in {line:?}: {}", self.log()))
            .to_owned();
        assert!(addr.starts_with("127.0.0.1:"), "{line:?}: {}", self.log());
        (child, addr)
    }

    fn log(&self) -> String {
        fs::read_to_string(&self.server_log).unwrap_or_default()
    }
}

#[test]
fn sync_daemon_and_pinning_between_two_processes() {
    let hosts = Hosts::new();
    let (ra, rb) = (hosts.ra.path(), hosts.rb.path());
    let (server, addr) = hosts.serve();
    let mut cfg = hosts.cfg();
    cfg.replicas[1].remote = Some(addr.clone());
    hosts.set_cfg(&cfg);

    // sync --once.
    fs::create_dir_all(ra.join("docs")).unwrap();
    fs::write(ra.join("docs/a.txt"), b"from a").unwrap();
    fs::write(rb.join("b.txt"), b"from b").unwrap();
    fs::write(rb.join("big"), vec![7u8; 1 << 20]).unwrap();
    let out = ok(hosts.client.path(), &["sync", "--once", "p"]);
    assert!(out.contains("4 change(s) applied"), "{out}");
    assert_eq!(files(ra), files(rb));
    let status = ok(hosts.client.path(), &["status", "p"]);
    assert!(status.contains(&format!("served at {addr}")), "{status}");

    // daemon: changes on either side arrive, B's through the pushed hints.
    let daemon = bin(hosts.client.path(), &["daemon", "p"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    std::thread::sleep(Duration::from_millis(500));
    fs::write(rb.join("from_b"), b"pushed").unwrap();
    wait_for(&ra.join("from_b"), b"pushed");
    fs::write(ra.join("docs/from_a"), b"sent").unwrap();
    wait_for(&rb.join("docs/from_a"), b"sent");
    let out = terminate(daemon);
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("daemon for \"p\" stopped"),
        "{out:?}"
    );
    assert_eq!(files(ra), files(rb));

    // The server presents a certificate other than the one A pins.
    let mut wrong = cfg.clone();
    wrong.replicas[1].device = Some(DeviceId::of(b"another certificate"));
    hosts.set_cfg(&wrong);
    let out = run(hosts.client.path(), &["sync", "--once", "p"]);
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success());
    assert!(err.contains("is not the pinned"), "{err}");

    // A presents a certificate other than the one the server pins for it.
    let pair_dir = hosts.pair_dir(hosts.client.path());
    let a = cfg.replicas[0].id;
    for f in <[PathBuf; 2]>::from(Identity::paths(&pair_dir, a)) {
        fs::remove_file(f).unwrap();
    }
    let stranger = Identity::generate().unwrap();
    stranger.save(&pair_dir, a).unwrap();
    let mut wrong = cfg.clone();
    wrong.replicas[0].device = Some(stranger.device());
    hosts.set_cfg(&wrong);
    let out = run(hosts.client.path(), &["sync", "--once", "p"]);
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success());
    assert!(err.contains("connection to 127.0.0.1"), "{err}");
    assert!(fs::read(ra.join("docs/a.txt")).unwrap() == b"from a");

    let out = terminate(server);
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("stopped serving"),
        "{out:?}"
    );
    let log = hosts.log();
    assert!(log.contains("rejected: TLS handshake failed"), "{log}");
    assert!(log.contains("is not the pinned"), "{log}");
}

#[test]
fn serve_needs_an_address_and_an_identity() {
    let hosts = Hosts::new();
    // No --listen and no remote address for A.
    let out = run(hosts.client.path(), &["serve", "p", "a"]);
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("--listen"), "{err}");
    // The server's host has no key for A.
    let out = run(
        hosts.server.path(),
        &["serve", "p", "a", "--listen", "127.0.0.1:0"],
    );
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains(".crt"), "{err}");
    // Nothing listens at B's placeholder address.
    let out = run(hosts.client.path(), &["sync", "--once", "p"]);
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("127.0.0.1:9"), "{err}");
}

/// `--sandbox` on both sides: the server may write its own root (which the
/// shared config marks remote), the client its local one.
#[test]
fn sandboxed_serve_and_sync() {
    if ffsl_sync::sandbox::abi().is_none() {
        eprintln!("landlock unsupported here; skipping");
        return;
    }
    let hosts = Hosts::new();
    let (server, addr) = hosts.serve_with(&["--sandbox"]);
    let mut cfg = hosts.cfg();
    cfg.replicas[1].remote = Some(addr);
    hosts.set_cfg(&cfg);
    fs::write(hosts.ra.path().join("a"), b"1").unwrap();
    fs::write(hosts.rb.path().join("b"), b"2").unwrap();
    ok(hosts.client.path(), &["--sandbox", "sync", "--once", "p"]);
    assert_eq!(files(hosts.ra.path()), files(hosts.rb.path()));
    terminate(server);
    assert!(hosts.log().contains("landlock sandbox"), "{}", hosts.log());
}

/// Changing one byte of a file above `DELTA_MIN_SIZE` sends it as a
/// block-level delta, and `sync --once` says so (T28).
#[test]
fn sync_once_reports_delta_transfers() {
    let hosts = Hosts::new();
    let (ra, rb) = (hosts.ra.path(), hosts.rb.path());
    let (server, addr) = hosts.serve();
    let mut cfg = hosts.cfg();
    cfg.replicas[1].remote = Some(addr);
    hosts.set_cfg(&cfg);

    let size = 2 * ffsl_sync::engine::executor::DELTA_MIN_SIZE as usize;
    let mut data: Vec<u8> = (0..size).map(|i| (i * 7 % 251) as u8).collect();
    fs::write(ra.join("data.bin"), &data).unwrap();
    let out = ok(hosts.client.path(), &["sync", "--once", "p"]);
    assert!(out.contains("1 change(s) applied"), "{out}");
    assert!(!out.contains("delta"), "a create goes whole: {out}");

    data[1_000_000] ^= 0xff;
    fs::write(ra.join("data.bin"), &data).unwrap();
    let out = ok(hosts.client.path(), &["sync", "--once", "p"]);
    assert!(
        out.contains("1 change(s) applied in 1 round(s), 1 sent as delta"),
        "{out}"
    );
    assert_eq!(fs::read(rb.join("data.bin")).unwrap(), data);
    terminate(server);
}

/// `status` on the host that serves B (T27): B is read from the report of
/// the running `serve`, then from its index; A is on the client host, not
/// "never synced".
#[test]
fn status_on_the_serving_host() {
    let hosts = Hosts::new();
    let (ra, rb) = (hosts.ra.path(), hosts.rb.path());
    let (client, server_home) = (hosts.client.path(), hosts.server.path());
    let (server, addr) = hosts.serve();
    let mut cfg = hosts.cfg();
    cfg.replicas[1].remote = Some(addr.clone());
    hosts.set_cfg(&cfg);

    // Before any sync: serve's first report.
    let st = ok(server_home, &["status", "p"]);
    assert!(st.contains("0 entries (0 tombstones)"), "{st}");
    assert!(st.contains("last sync:   never"), "{st}");

    fs::create_dir(ra.join("d")).unwrap();
    fs::write(ra.join("d/a"), b"1").unwrap();
    fs::write(rb.join("b"), b"2").unwrap();
    ok(client, &["sync", "--once", "p"]);

    let check = |st: &str, served: &str| {
        let (a, b) = st.split_once("\n  b: ").unwrap_or_else(|| panic!("{st}"));
        assert!(
            a.contains("on the client host, not here (run `status` there)"),
            "{st}"
        );
        assert!(!a.contains("entries") && !a.contains("never"), "{st}");
        assert!(b.contains(served), "{st}");
        assert!(b.contains("3 entries (0 tombstones)"), "{st}");
        assert!(b.contains("last sync:") && !b.contains("never"), "{st}");
        assert!(!st.contains("daemon"), "{st}");
    };
    let st = ok(server_home, &["status", "p"]);
    check(
        &st,
        &format!("served here on {addr} (`serve` running; its report from "),
    );

    terminate(server);
    let st = ok(server_home, &["status", "p"]);
    check(&st, "served here at 127.0.0.1:9 (`serve` not running)");

    // The client's view is unchanged.
    let st = ok(client, &["status", "p"]);
    assert!(
        st.contains(&format!("served at {addr} (run `status` there)")),
        "{st}"
    );
    assert!(st.contains("3 entries (0 tombstones)"), "{st}");
    assert!(!st.contains("served here") && !st.contains("never"), "{st}");
}

/// Issue #7: B's identity is made on its server, never by `init` on the
/// client. Until the client pins its device ID, neither side starts, and
/// both say what to do.
#[test]
fn a_served_replica_needs_its_own_identity_and_a_pin() {
    let t = || tempfile::tempdir().unwrap();
    let (client, server, ra, rb) = (t(), t(), t(), t());
    let (a, b) = (ra.path().to_str().unwrap(), rb.path().to_str().unwrap());
    ok(
        client.path(),
        &["init", "p", "--a", a, "--b", b, "--b-remote", "127.0.0.1:9"],
    );
    let err = |home: &Path, args: &[&str]| {
        let out = run(home, args);
        assert!(!out.status.success(), "{args:?}");
        String::from_utf8_lossy(&out.stderr).into_owned()
    };
    let e = err(client.path(), &["sync", "--once", "p"]);
    assert!(
        e.contains("no pinned device id") && e.contains("ffsl-sync identity"),
        "{e}"
    );

    let (from, to) = (
        config::pair_dir(client.path(), "p").unwrap(),
        config::pair_dir(server.path(), "p").unwrap(),
    );
    fs::create_dir_all(&to).unwrap();
    fs::copy(from.join(config::CONFIG_FILE), to.join(config::CONFIG_FILE)).unwrap();
    let serve = ["serve", "p", "b", "--listen", "127.0.0.1:0"];
    let e = err(server.path(), &serve);
    assert!(e.contains("no pinned device id"), "{e}");

    let first = ok(server.path(), &["identity", "p", "b"]);
    assert!(first.contains("new identity"), "{first}");
    let again = ok(server.path(), &["identity", "p", "b"]);
    assert!(!again.contains("new identity"), "{again}");
    let device = |out: &str| {
        out.lines()
            .find_map(|l| l.trim().strip_prefix("device "))
            .unwrap()
            .to_owned()
    };
    assert_eq!(device(&first), device(&again));
    // A's identity is on the client: the server makes none, and the
    // client does not pin one by hand.
    let e = err(server.path(), &["identity", "p", "a"]);
    assert!(e.contains("made on another host"), "{e}");
    let e = err(client.path(), &["pin", "p", "a", &device(&first)]);
    assert!(e.contains("is on this host"), "{e}");
    let e = err(client.path(), &["pin", "p", "b", "xyz"]);
    assert!(e.contains("64 hex digits"), "{e}");

    let pinned = ok(client.path(), &["pin", "p", "b", &device(&first)]);
    assert!(
        pinned.contains(&format!("pinned device {}", device(&first))),
        "{pinned}"
    );
    let pinned = ok(client.path(), &["pin", "p", "b", &device(&first)]);
    assert!(pinned.contains("(unchanged)"), "{pinned}");
    assert_eq!(
        PairConfig::load(client.path(), "p").unwrap(),
        PairConfig::load(server.path(), "p").unwrap()
    );
}
