//! What a client can take from `serve` (issue #5): connections and
//! handshakes are bounded, a handshake has a deadline, a client stopped in
//! the middle of an upload holds up the others only until the request
//! timeout (and never a shutdown), and a `RecordSync` list only keeps what
//! the server's own tombstones need. A `RecordSync` names the session's
//! peer, and its retention is the server's (issue #7).

use std::io::{ErrorKind, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use ffsl_sync::config::{self, PairConfig, ReplicaId};
use ffsl_sync::engine::Engine;
use ffsl_sync::fs::RelPath;
use ffsl_sync::fs::commit::FileMeta;
use ffsl_sync::index::{PeerState, VersionVector};
use ffsl_sync::replica::proto::{
    Content, Request, Response, client_handshake, read_frame, write_frame,
};
use ffsl_sync::replica::{LocalReplica, Op, Precondition, RemoteReplica, Replica};
use ffsl_sync::server::{Limits, Server, ServerHandle};
use ffsl_sync::tls::{Identity, server_name};
use rustls::{ClientConnection, StreamOwned};
use tempfile::TempDir;

struct Fx {
    _state: TempDir,
    _ra: TempDir,
    rb: TempDir,
    cfg: PairConfig,
    pair_dir: PathBuf,
    server: ServerHandle,
}

type Session = StreamOwned<ClientConnection, TcpStream>;

impl Fx {
    /// Serves B with `limits`.
    fn new(limits: Limits) -> Fx {
        let (state, ra, rb) = (tmp(), tmp(), tmp());
        let (mut cfg, _) = config::init(state.path(), "p", ra.path(), rb.path()).unwrap();
        let pair_dir = config::pair_dir(state.path(), "p").unwrap();
        let [a, b] = &cfg.replicas;
        let identity = Identity::load(&pair_dir, b.id, b.device.unwrap()).unwrap();
        let tls = identity.server_config(a.device.unwrap()).unwrap();
        let replica = LocalReplica::open(b, &pair_dir).unwrap();
        let server = Server::new(replica, a.id, tls)
            .limits(limits)
            .spawn(TcpListener::bind("127.0.0.1:0").unwrap())
            .unwrap();
        cfg.replicas[1].remote = Some(server.local_addr().to_string());
        Fx {
            _state: state,
            _ra: ra,
            rb,
            cfg,
            pair_dir,
            server,
        }
    }

    fn ids(&self) -> (ReplicaId, ReplicaId) {
        (self.cfg.replicas[0].id, self.cfg.replicas[1].id)
    }

    /// B, as A reaches it.
    fn client(&self) -> ffsl_sync::Result<RemoteReplica> {
        RemoteReplica::connect(&self.cfg.replicas[1], &self.cfg.replicas[0], &self.pair_dir)
    }

    /// A protocol session with the server, as A, to send it anything.
    fn session(&self) -> Session {
        let [a, b] = &self.cfg.replicas;
        let identity = Identity::load(&self.pair_dir, a.id, a.device.unwrap()).unwrap();
        let tls = identity.client_config(b.device.unwrap()).unwrap();
        let tcp = TcpStream::connect(self.server.local_addr()).unwrap();
        let mut s = StreamOwned::new(ClientConnection::new(tls, server_name()).unwrap(), tcp);
        client_handshake(&mut s, a.id, b.id).unwrap();
        s
    }

    /// A bare TCP connection to the server.
    fn tcp(&self) -> TcpStream {
        TcpStream::connect(self.server.local_addr()).unwrap()
    }

    /// Starts uploading a file in `s` and stops after the first chunk.
    fn stall_upload(&self, s: &mut Session) {
        let mut vv = VersionVector::new();
        vv.set(self.ids().0, 1);
        let op = Op::WriteFile {
            meta: FileMeta {
                mode: 0o644,
                mtime_ns: 0,
            },
            hash: [0; 32],
            vv,
        };
        let apply = Request::Apply {
            path: RelPath::new("f").unwrap(),
            op,
            pre: Precondition::Absent,
            content: true,
        };
        write_frame(s, &apply).unwrap();
        write_frame(s, &Request::Content(Content::Chunk(vec![1; 1000]))).unwrap();
        // Let the server take the replica's lock and wait for more.
        std::thread::sleep(Duration::from_millis(300));
    }
}

fn tmp() -> TempDir {
    tempfile::tempdir().unwrap()
}

/// Whether the server closed `tcp` within `wait`.
fn closed_within(tcp: &mut TcpStream, wait: Duration) -> bool {
    tcp.set_read_timeout(Some(wait)).unwrap();
    let mut buf = [0; 64];
    match tcp.read(&mut buf) {
        Ok(0) => true,
        Ok(_) => panic!("the server sent something"),
        Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => false,
        Err(_) => true,
    }
}

/// Polls `cond` for up to 5 s.
fn eventually(what: &str, cond: impl Fn() -> bool) {
    let start = Instant::now();
    while !cond() {
        assert!(start.elapsed() < Duration::from_secs(5), "{what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn a_flood_of_idle_connections_is_bounded_and_passes() {
    let fx = Fx::new(Limits {
        connections: 8,
        handshakes: 4,
        handshakes_per_ip: 2,
        handshake_deadline: Duration::from_secs(2),
        ..Limits::default()
    });
    let b = fx.client().unwrap();
    b.clock().unwrap();
    assert_eq!(fx.server.connections(), 1);

    // Every connection is from 127.0.0.1: two get a handshake slot, the
    // others are closed at once.
    let mut flood: Vec<TcpStream> = (0..40).map(|_| fx.tcp()).collect();
    let held: Vec<TcpStream> = flood
        .iter_mut()
        .filter_map(|t| {
            (!closed_within(t, Duration::from_millis(200))).then(|| t.try_clone().unwrap())
        })
        .collect();
    assert_eq!(held.len(), 2);
    assert_eq!(fx.server.connections(), 1 + 2);

    // The session of the peer goes on.
    b.clock().unwrap();

    // The held ones are closed at the deadline; then the peer connects.
    for mut t in held {
        assert!(closed_within(&mut t, Duration::from_secs(4)));
    }
    eventually("idle handshakes closed", || fx.server.connections() == 1);
    fx.client().unwrap().clock().unwrap();
}

#[test]
fn a_slow_handshake_meets_its_deadline() {
    let fx = Fx::new(Limits {
        handshake_deadline: Duration::from_secs(1),
        ..Limits::default()
    });
    let mut slow = fx.tcp();
    // A TLS record header announcing 16 KiB, then a byte at a time, each
    // well within any per-read timeout.
    let start = Instant::now();
    slow.write_all(&[0x16, 0x03, 0x01, 0x40, 0x00]).unwrap();
    let mut cut = false;
    while start.elapsed() < Duration::from_secs(5) {
        if slow.write_all(&[0]).is_err() {
            cut = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(cut, "still open after {:?}", start.elapsed());
    assert!(
        start.elapsed() >= Duration::from_millis(900),
        "{:?}",
        start.elapsed()
    );
}

#[test]
fn too_many_connections_are_refused() {
    let fx = Fx::new(Limits {
        connections: 3,
        ..Limits::default()
    });
    let sessions: Vec<Session> = (0..3).map(|_| fx.session()).collect();
    assert_eq!(fx.server.connections(), 3);
    let mut extra = fx.tcp();
    assert!(closed_within(&mut extra, Duration::from_secs(2)));
    assert!(fx.client().and_then(|b| b.clock()).is_err());
    drop(sessions);
    eventually("sessions closed", || fx.server.connections() == 0);
    fx.client().unwrap().clock().unwrap();
}

#[test]
fn a_stalled_upload_holds_up_the_others_until_the_request_timeout() {
    let fx = Fx::new(Limits {
        request_timeout: Duration::from_secs(1),
        ..Limits::default()
    });
    let b = fx.client().unwrap();
    b.clock().unwrap();
    let mut stalled = fx.session();
    fx.stall_upload(&mut stalled);

    // Answered once the stalled request times out (it never was before).
    let (tx, rx) = crossbeam_channel::bounded(1);
    std::thread::spawn(move || tx.send(b.clock().map(|_| b)));
    let b = rx
        .recv_timeout(Duration::from_secs(5))
        .expect("held up")
        .unwrap();
    b.clock().unwrap();
    // The stalled client's connection is closed already, without an
    // answer, and nothing was written.
    stalled
        .sock
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let start = Instant::now();
    assert!(!matches!(
        read_frame::<_, Response>(&mut stalled),
        Ok(Some(_))
    ));
    assert!(
        start.elapsed() < Duration::from_secs(1),
        "{:?}",
        start.elapsed()
    );
    assert!(!fx.rb.path().join("f").exists());
}

#[test]
fn a_stalled_upload_does_not_hold_up_a_shutdown() {
    let mut fx = Fx::new(Limits::default());
    let mut stalled = fx.session();
    fx.stall_upload(&mut stalled);

    // `serve`'s loop sweeps between waits for a signal: it does not wait
    // for the replica.
    let start = Instant::now();
    fx.server.sweep();
    assert!(
        start.elapsed() < Duration::from_millis(500),
        "{:?}",
        start.elapsed()
    );
    // Closing the connections ends the upload, and the request lets go.
    fx.server.shutdown();
    drop(fx.server.replica());
    assert!(
        start.elapsed() < Duration::from_secs(3),
        "{:?}",
        start.elapsed()
    );
}

#[test]
fn record_sync_keeps_only_current_tombstones() {
    let fx = Fx::new(Limits::default());
    let mut s = fx.session();
    let (a, _) = fx.ids();
    let mut vv = VersionVector::new();
    vv.set(a, 1);
    // Batches of tombstones B does not have: all dropped as they come.
    let junk: Vec<_> = (0..10_000)
        .map(|i| {
            (
                RelPath::new(format!("junk{i}")).unwrap(),
                vv.clone(),
                PeerState::Absent,
            )
        })
        .collect();
    for more in [true, true, true, false] {
        let batch = Request::RecordSync {
            peer: a,
            tombstones: junk.clone(),
            retention: Duration::ZERO,
            more,
        };
        write_frame(&mut s, &batch).unwrap();
    }
    assert_eq!(
        read_frame::<_, Response>(&mut s).unwrap(),
        Some(Response::Collected {
            paths: Vec::new(),
            more: false
        })
    );

    // A peer vector of absurd length is a protocol error.
    let long: VersionVector = (1..=100).map(|id| (ReplicaId(id), 1)).collect();
    let batch = Request::RecordSync {
        peer: a,
        tombstones: vec![(RelPath::new("t").unwrap(), vv, PeerState::Tombstone(long))],
        retention: Duration::ZERO,
        more: false,
    };
    write_frame(&mut s, &batch).unwrap();
    assert!(matches!(
        read_frame::<_, Response>(&mut s),
        Err(_) | Ok(None)
    ));
}

/// Issue #7: the server records a sync with the session's peer only, and
/// keeps tombstones for its own retention, whatever the client says.
#[test]
fn record_sync_takes_neither_peer_nor_retention_from_the_client() {
    let fx = Fx::new(Limits::default());
    let ra = fx.cfg.replicas[0].root.clone();
    let mut a = LocalReplica::open(&fx.cfg.replicas[0], &fx.pair_dir).unwrap();
    let mut b = fx.client().unwrap();
    // A client asking for no retention at all.
    let engine = Engine::new().tombstone_retention(Duration::ZERO);
    std::fs::write(ra.join("f"), b"x").unwrap();
    engine.sync_once(&mut a, &mut b).unwrap();
    assert!(fx.rb.path().join("f").exists());
    std::fs::remove_file(ra.join("f")).unwrap();
    let mut collected = [0; 2];
    for _ in 0..3 {
        let report = engine.sync_once(&mut a, &mut b).unwrap();
        for (sum, n) in collected.iter_mut().zip(report.collected) {
            *sum += n;
        }
    }
    // A (local) follows the engine; B keeps its tombstone for 30 days.
    assert_eq!(collected, [1, 0]);
    drop(b);

    // A RecordSync on behalf of another replica ends the connection.
    let mut s = fx.session();
    let (_, b_id) = fx.ids();
    let batch = Request::RecordSync {
        peer: b_id,
        tombstones: Vec::new(),
        retention: Duration::ZERO,
        more: false,
    };
    write_frame(&mut s, &batch).unwrap();
    assert!(matches!(
        read_frame::<_, Response>(&mut s),
        Err(_) | Ok(None)
    ));
}
