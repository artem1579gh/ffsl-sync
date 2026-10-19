//! `serve`: runs one replica of a pair for its peer, over TLS (design §7.1).
//!
//! A [`Server`] wraps a [`LocalReplica`] and answers the wire protocol
//! ([`crate::replica::proto`]) on every connection its peer opens: TCP, then
//! TLS 1.3 with mutual authentication (each side accepts only the device ID
//! the config pins for the other, [`crate::tls`]), then the protocol
//! handshake (replica IDs). Requests are answered in order, each under the
//! replica's lock, so the CAS checks run here, next to the files, exactly as
//! for a local pair.
//!
//! A `Watch` request turns its connection into a hint stream: the server
//! answers `Watching`, then only pushes the replica watcher's hints (one
//! watcher, fanned out to every hint connection). The client uses a
//! separate connection for it ([`crate::replica::RemoteReplica`]).
//!
//! The server is reachable by anyone before the TLS handshake, so what a
//! connection may take is bounded ([`Limits`], issue #5): the number of
//! connections and of handshakes in progress (in all and per IP address), a
//! deadline for the whole handshake, and a timeout for a client that stops
//! sending or reading in the middle of a request (an upload holds the
//! replica's lock). Each frame's decoded size is bounded by the protocol
//! ([`crate::budget`]), and a `RecordSync` list by the replica's own
//! tombstones.
//!
//! Quarantined old inodes are swept after every `RecordSync` (the end of a
//! peer's sync cycle) and when their grace period ends ([`ServerHandle::sweep`]).
//! With [`Server::status_dir`], a [`ServedReport`] for `status` is saved at
//! start, after every `RecordSync` and after every sweep (the index is
//! locked while the server runs).

use std::collections::{BTreeMap, HashMap};
use std::io::{self, Read, Write};
use std::net::{IpAddr, Shutdown, SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, TryLockError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender, select};
use rustls::{ServerConfig, ServerConnection, StreamOwned};

use crate::config::{PairConfig, ReplicaId};
use crate::error::{Error, Result};
use crate::fs::RelPath;
use crate::index::{PeerState, VersionVector};
use crate::replica::proto::{
    BATCH_BYTES, CLOCK_VERSION, Content, ContentStream, DELTA_VERSION, PROTOCOL_VERSION, Request,
    Response, WireError, batches, read_frame, send_content, server_handshake_upto, write_frame,
};
use crate::replica::remote::tune_socket;
use crate::replica::{ContentReader, LocalReplica, Replica};
use crate::status::ServedReport;
use crate::tls::Identity;
use crate::watch::Hint;

type Tls = StreamOwned<ServerConnection, TcpStream>;

/// The most counters a peer's tombstone vector in a `RecordSync` may have.
/// A pair's vectors have one per replica; this only stops a peer from
/// making every one of our tombstones cost megabytes.
const MAX_PEER_COUNTERS: usize = 64;

/// What the clients of a server may take (issue #5). A connection over a
/// limit is closed at once, before its TLS handshake.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    /// Open connections, in all (each is a thread and two fds).
    pub connections: usize,
    /// Connections still in their TLS or protocol handshake...
    pub handshakes: usize,
    /// ... and of those, from one IP address, so that a single host cannot
    /// take every handshake slot.
    pub handshakes_per_ip: usize,
    /// The whole handshake must be done within this time of the accept.
    pub handshake_deadline: Duration,
    /// Once a request has arrived, every read and write of the client's
    /// connection must make progress within this time until it is answered
    /// (a client stopped in the middle of an upload holds up every other
    /// request). Between requests, a connection may idle for ever.
    pub request_timeout: Duration,
}

impl Default for Limits {
    fn default() -> Limits {
        Limits {
            connections: 64,
            handshakes: 8,
            handshakes_per_ip: 4,
            handshake_deadline: Duration::from_secs(10),
            request_timeout: Duration::from_secs(60),
        }
    }
}

/// The replica and its peer, as served.
pub struct Server {
    replica: LocalReplica,
    peer: ReplicaId,
    tls: Arc<ServerConfig>,
    max_version: u32,
    status_dir: Option<PathBuf>,
    limits: Limits,
}

impl Server {
    /// Serves `replica` to the replica `peer` with the TLS config `tls`
    /// (which decides which client certificate is accepted).
    /// The replica's peer is reached over the network, so its symlinks are
    /// not trusted unless the config says so (design §4.6).
    pub fn new(replica: LocalReplica, peer: ReplicaId, tls: Arc<ServerConfig>) -> Server {
        Server {
            replica: replica.remote_peer(true),
            peer,
            tls,
            max_version: PROTOCOL_VERSION,
            status_dir: None,
            limits: Limits::default(),
        }
    }

    /// Replaces the default [`Limits`].
    pub fn limits(mut self, limits: Limits) -> Server {
        self.limits = limits;
        self
    }

    /// Saves a [`ServedReport`] in `pair_dir` at start, after every
    /// `RecordSync` and after every quarantine sweep, for `status` (which
    /// cannot open the index while the server holds it).
    pub fn status_dir(mut self, pair_dir: PathBuf) -> Server {
        self.status_dir = Some(pair_dir);
        self
    }

    /// Speaks protocol versions up to `version` only (tests force older
    /// sessions with it).
    pub fn max_protocol(mut self, version: u32) -> Server {
        self.max_version = version;
        self
    }

    /// Opens replica `side` (0 = A, 1 = B) of `cfg` and its identity from
    /// `pair_dir`; accepts only the other replica's pinned certificate.
    pub fn open(cfg: &PairConfig, side: usize, pair_dir: &Path) -> Result<Server> {
        let (me, peer) = (&cfg.replicas[side], &cfg.replicas[1 - side]);
        let identity = Identity::load(pair_dir, me.id, me.device()?)?;
        let tls = identity.server_config(peer.device()?)?;
        let replica = LocalReplica::open(me, pair_dir)?;
        Ok(Server::new(replica, peer.id, tls))
    }

    /// Starts accepting connections on `listener`, in a thread of its own.
    pub fn spawn(self, listener: TcpListener) -> Result<ServerHandle> {
        let addr = listener
            .local_addr()
            .map_err(|e| Error::io("listener address", e))?;
        let (stop_tx, stop) = crossbeam_channel::bounded(0);
        let shared = Arc::new(Shared {
            id: self.replica.id(),
            peer: self.peer,
            replica: Mutex::new(self.replica),
            tls: self.tls,
            max_version: self.max_version,
            status_dir: self.status_dir,
            limits: self.limits,
            handshakes: Mutex::new(Handshakes::default()),
            addr,
            subscribers: Arc::new(Mutex::new(Vec::new())),
            fanout: AtomicBool::new(false),
            conns: Mutex::new(BTreeMap::new()),
            next_conn: AtomicU64::new(0),
            stopping: AtomicBool::new(false),
            stop,
        });
        let accept = {
            let shared = shared.clone();
            std::thread::Builder::new()
                .name("accept".into())
                .spawn(move || accept_loop(&shared, listener))
                .map_err(|e| Error::io("spawn accept thread", e))?
        };
        shared.save_status(&shared.replica());
        tracing::info!(%addr, replica = %shared.id, peer = %shared.peer, "serving");
        Ok(ServerHandle {
            shared,
            addr,
            accept: Some(accept),
            _stop: Some(stop_tx),
        })
    }
}

/// A running server. Dropping it stops the server (see
/// [`ServerHandle::shutdown`]).
pub struct ServerHandle {
    shared: Arc<Shared>,
    addr: SocketAddr,
    accept: Option<JoinHandle<()>>,
    /// Dropped to end the hint streams.
    _stop: Option<Sender<()>>,
}

impl ServerHandle {
    /// The address it listens on.
    pub fn local_addr(&self) -> SocketAddr {
        self.addr
    }

    /// The served replica. Holding it holds up every request.
    pub fn replica(&self) -> MutexGuard<'_, LocalReplica> {
        self.shared.replica()
    }

    /// The number of open connections.
    pub fn connections(&self) -> usize {
        lock(&self.shared.conns).len()
    }

    /// Sweeps the quarantine if an entry is due; returns when the next one
    /// will be. While a request holds the replica, it does not wait: it
    /// returns a time a little later, to try again then (so a caller that
    /// also waits for a signal sees it at once).
    pub fn sweep(&self) -> Option<Instant> {
        let mut replica = match self.shared.replica.try_lock() {
            Ok(guard) => guard,
            Err(TryLockError::Poisoned(e)) => e.into_inner(),
            Err(TryLockError::WouldBlock) => {
                return Some(Instant::now() + Duration::from_millis(100));
            }
        };
        if replica
            .quarantine()
            .next_deadline()
            .is_some_and(|d| d <= Instant::now())
        {
            replica.sweep_quarantine();
            self.shared.save_status(&replica);
        }
        replica.quarantine().next_deadline()
    }

    /// Closes every open connection (the clients reconnect); for tests.
    pub fn close_connections(&self) {
        for conn in lock(&self.shared.conns).values() {
            let _ = conn.shutdown(Shutdown::Both);
        }
    }

    /// Shuts the server down and gives back its replica, once every
    /// connection thread has let go of it.
    pub fn into_replica(mut self) -> LocalReplica {
        self.shutdown();
        let mut shared = self.shared.clone();
        drop(self);
        loop {
            match Arc::try_unwrap(shared) {
                Ok(s) => return s.replica.into_inner().unwrap_or_else(|e| e.into_inner()),
                Err(s) => {
                    shared = s;
                    std::thread::sleep(Duration::from_millis(5));
                }
            }
        }
    }

    /// Stops accepting, closes every connection and ends the hint streams.
    /// A request being answered finishes first: take [`Self::replica`]
    /// afterwards to wait for it.
    pub fn shutdown(&mut self) {
        if self.shared.stopping.swap(true, Ordering::SeqCst) {
            return;
        }
        self._stop = None;
        // Wake the accept loop.
        let _ = TcpStream::connect_timeout(&self.addr, Duration::from_secs(1));
        if let Some(t) = self.accept.take() {
            let _ = t.join();
        }
        self.close_connections();
        tracing::info!(addr = %self.addr, "stopped serving");
    }
}

impl Drop for ServerHandle {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// What every connection thread shares.
struct Shared {
    id: ReplicaId,
    peer: ReplicaId,
    replica: Mutex<LocalReplica>,
    tls: Arc<ServerConfig>,
    /// The highest protocol version spoken.
    max_version: u32,
    /// Where the status report goes, if anywhere.
    status_dir: Option<PathBuf>,
    limits: Limits,
    /// The connections in their handshake.
    handshakes: Mutex<Handshakes>,
    /// The address it listens on.
    addr: SocketAddr,
    /// The hint connections' channels.
    subscribers: Arc<Mutex<Vec<Sender<Hint>>>>,
    /// The fan-out thread runs.
    fanout: AtomicBool,
    /// Open connections, to close them on shutdown.
    conns: Mutex<BTreeMap<u64, TcpStream>>,
    next_conn: AtomicU64,
    stopping: AtomicBool,
    /// Disconnected on shutdown.
    stop: Receiver<()>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl Shared {
    fn replica(&self) -> MutexGuard<'_, LocalReplica> {
        lock(&self.replica)
    }

    /// Saves the status report on `replica` (this server's, locked by the
    /// caller), if [`Server::status_dir`] asked for one.
    fn save_status(&self, replica: &LocalReplica) {
        let Some(dir) = &self.status_dir else { return };
        let addr = self.addr.to_string();
        let saved = ServedReport::of(replica, self.peer, &addr).and_then(|r| r.save(dir, self.id));
        if let Err(e) = saved {
            tracing::warn!(error = %e, "cannot save the status report");
        }
    }
}

/// The connections in their handshake, in all and per IP address.
#[derive(Default)]
struct Handshakes {
    total: usize,
    per_ip: HashMap<IpAddr, usize>,
}

/// A connection's place among the [`Handshakes`], given back when dropped.
struct HandshakeSlot {
    shared: Arc<Shared>,
    ip: IpAddr,
}

impl HandshakeSlot {
    /// Takes a place for a connection from `ip`, if the limits allow it.
    fn take(shared: &Arc<Shared>, ip: IpAddr) -> Option<HandshakeSlot> {
        let mut hs = lock(&shared.handshakes);
        let from_ip = hs.per_ip.get(&ip).copied().unwrap_or(0);
        if hs.total >= shared.limits.handshakes || from_ip >= shared.limits.handshakes_per_ip {
            return None;
        }
        hs.total += 1;
        hs.per_ip.insert(ip, from_ip + 1);
        Some(HandshakeSlot {
            shared: shared.clone(),
            ip,
        })
    }
}

impl Drop for HandshakeSlot {
    fn drop(&mut self) {
        let mut hs = lock(&self.shared.handshakes);
        hs.total -= 1;
        if let Some(n) = hs.per_ip.get_mut(&self.ip) {
            *n -= 1;
            if *n == 0 {
                hs.per_ip.remove(&self.ip);
            }
        }
    }
}

fn accept_loop(shared: &Arc<Shared>, listener: TcpListener) {
    for tcp in listener.incoming() {
        if shared.stopping.load(Ordering::SeqCst) {
            return;
        }
        let tcp = match tcp {
            Ok(tcp) => tcp,
            Err(e) => {
                tracing::warn!(error = %e, "accept failed");
                // E.g. EMFILE: don't spin.
                std::thread::sleep(Duration::from_millis(100));
                continue;
            }
        };
        let Ok(addr) = tcp.peer_addr() else { continue };
        let peer = addr.to_string();
        // Closed at once (dropped) when over a limit. Handshakes are bounded
        // more tightly, so the connections over the total are the peer's own.
        if lock(&shared.conns).len() >= shared.limits.connections {
            tracing::warn!(%peer, limit = shared.limits.connections, "too many connections; closing a new one");
            continue;
        }
        let Some(slot) = HandshakeSlot::take(shared, addr.ip()) else {
            tracing::debug!(%peer, "too many handshakes in progress; closing a new connection");
            continue;
        };
        let id = shared.next_conn.fetch_add(1, Ordering::Relaxed);
        if let Ok(clone) = tcp.try_clone() {
            lock(&shared.conns).insert(id, clone);
        }
        let thread = {
            let shared = shared.clone();
            let peer = peer.clone();
            std::thread::Builder::new()
                .name(format!("conn-{id}"))
                .spawn(move || {
                    match handle(&shared, tcp, &peer, slot) {
                        Ok(()) => tracing::debug!(%peer, "connection closed"),
                        // The client went away (e.g. it dropped a read).
                        Err(e @ Error::Io { .. }) => {
                            tracing::debug!(%peer, error = %e, "connection ended");
                        }
                        Err(e) => tracing::warn!(%peer, error = %e, "connection ended"),
                    }
                    lock(&shared.conns).remove(&id);
                })
        };
        if let Err(e) = thread {
            tracing::warn!(%peer, error = %e, "cannot start a connection thread");
            lock(&shared.conns).remove(&id);
        }
    }
}

/// Serves one connection until the client closes it.
fn handle(shared: &Shared, tcp: TcpStream, peer: &str, slot: HandshakeSlot) -> Result<()> {
    tune_socket(&tcp);
    let deadline = Instant::now() + shared.limits.handshake_deadline;
    let mut conn = ServerConnection::new(shared.tls.clone()).map_err(|e| Error::Tls {
        reason: e.to_string(),
    })?;
    let mut sock = Deadline {
        tcp: &tcp,
        deadline,
    };
    while conn.is_handshaking() {
        if let Err(e) = conn.complete_io(&mut sock) {
            // Send the alert, if any, so the client learns why.
            let _ = conn.complete_io(&mut sock);
            // Anyone can get this far: not worth more than a debug line.
            tracing::debug!(%peer, "rejected: TLS handshake failed: {e}");
            return Ok(());
        }
    }
    // The client is our peer now (TLS checked its certificate).
    let mut stream = rustls::Stream::new(&mut conn, &mut sock);
    let session = server_handshake_upto(&mut stream, shared.id, shared.peer, shared.max_version)?;
    drop(slot);
    let mut tls = StreamOwned::new(conn, tcp);
    tracing::debug!(%peer, version = session.version, "client connected");
    loop {
        // Idle between requests for as long as the client likes.
        timeouts(&tls.sock, None)?;
        let Some(req) = read_frame::<_, Request>(&mut tls)? else {
            break;
        };
        timeouts(&tls.sock, Some(shared.limits.request_timeout))?;
        if !serve(shared, &mut tls, session.version, req)? {
            break;
        }
    }
    Ok(())
}

/// Sets the read and write timeouts of `tcp`.
fn timeouts(tcp: &TcpStream, t: Option<Duration>) -> Result<()> {
    tcp.set_read_timeout(t)
        .and_then(|()| tcp.set_write_timeout(t))
        .map_err(|e| Error::io("set socket timeouts", e))
}

/// A connection in its handshake: every read and write must be done by
/// `deadline` (a socket timeout alone would let a client send a byte per
/// timeout for ever).
struct Deadline<'a> {
    tcp: &'a TcpStream,
    deadline: Instant,
}

impl Deadline<'_> {
    /// Sets the socket timeout `set` to the time left.
    fn arm(&self, set: fn(&TcpStream, Option<Duration>) -> io::Result<()>) -> io::Result<()> {
        let left = self.deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "handshake deadline passed",
            ));
        }
        set(self.tcp, Some(left))
    }
}

impl Read for Deadline<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.arm(TcpStream::set_read_timeout)?;
        (&mut &*self.tcp).read(buf)
    }
}

impl Write for Deadline<'_> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.arm(TcpStream::set_write_timeout)?;
        (&mut &*self.tcp).write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        (&mut &*self.tcp).flush()
    }
}

/// Sends `result` as the answer: the response, or the error.
fn answer<W: Write + ?Sized>(w: &mut W, result: Result<Response>) -> Result<()> {
    let msg = result.unwrap_or_else(|e| Response::Error(WireError::from(&e)));
    write_frame(w, &msg)
}

fn content_only(msg: Request) -> Result<Option<Content>> {
    match msg {
        Request::Content(c) => Ok(Some(c)),
        _ => Err(Error::Protocol {
            reason: "unexpected request inside a content stream".into(),
        }),
    }
}

/// Answers one request on a session of protocol `version`. Returns whether
/// the connection goes on with more requests (not after `Watch`).
fn serve(shared: &Shared, tls: &mut Tls, version: u32, req: Request) -> Result<bool> {
    let v2 = matches!(
        req,
        Request::Blocks { .. } | Request::ReadBlocks { .. } | Request::ApplyDelta { .. }
    );
    if v2 && version < DELTA_VERSION {
        return Err(Error::Protocol {
            reason: format!("a delta request on a protocol v{version} session"),
        });
    }
    let v3 = matches!(req, Request::Clock | Request::Witness { .. });
    if v3 && version < CLOCK_VERSION {
        return Err(Error::Protocol {
            reason: format!("a clock request on a protocol v{version} session"),
        });
    }
    match req {
        Request::Scan { scope } => {
            let r = shared.replica().scan(scope).map(|mut stats| {
                // Within a message, whatever the tree.
                let left_out = stats.bound();
                if left_out > 0 {
                    tracing::warn!(
                        left_out,
                        "too many paths could not be scanned to tell the peer about them all (see above)"
                    );
                }
                stats
            });
            answer(tls, r.map(Response::Scanned))?;
        }
        Request::ChangesSince { seq } => {
            let changes = shared.replica().changes_since(seq);
            match changes {
                Ok(entries) => {
                    let parts = batches(entries, BATCH_BYTES);
                    let last = parts.len() - 1;
                    for (i, entries) in parts.into_iter().enumerate() {
                        let more = i < last;
                        write_frame(tls, &Response::Changes { entries, more })?;
                    }
                }
                Err(e) => answer(tls, Err(e))?,
            }
        }
        Request::OpenRead { path, expect } => {
            let reader = shared.replica().open_read(&path, &expect);
            send_read(tls, &path, reader)?;
        }
        Request::Blocks { path, expect } => {
            let r = shared.replica().blocks(&path, &expect);
            answer(tls, r.map(Response::Blocks))?;
        }
        Request::ReadBlocks {
            path,
            expect,
            blocks,
        } => {
            let reader = shared.replica().read_blocks(&path, &expect, &blocks);
            send_read(tls, &path, reader)?;
        }
        Request::ApplyDelta {
            path,
            op,
            pre,
            delta,
        } => {
            let mut stream = ContentStream::new(&mut *tls, content_only);
            let result = shared
                .replica()
                .apply_delta(&path, op, pre, &delta, &mut stream);
            if !stream.is_finished() {
                stream.drain()?;
            }
            answer(tls, result.map(Response::Applied))?;
        }
        Request::Apply {
            path,
            op,
            pre,
            content,
        } => {
            let result = if content {
                let mut stream = ContentStream::new(&mut *tls, content_only);
                let result =
                    shared
                        .replica()
                        .apply(&path, op, pre, Some(&mut stream as &mut dyn Read));
                // The precondition may have failed before the content was
                // read: consume it, so the next frame is the next request.
                if !stream.is_finished() {
                    stream.drain()?;
                }
                result
            } else {
                shared.replica().apply(&path, op, pre, None)
            };
            answer(tls, result.map(Response::Applied))?;
        }
        Request::Adopt { path } => {
            let r = shared.replica().adopt(&path);
            answer(tls, r.map(Response::Adopted))?;
        }
        Request::Clock => {
            let r = shared.replica().clock();
            answer(tls, r.map(Response::Clock))?;
        }
        Request::Witness { floor } => {
            let mut replica = shared.replica();
            let r = replica.witness(floor).and_then(|()| replica.clock());
            answer(tls, r.map(Response::Clock))?;
        }
        Request::RecordSync {
            peer,
            tombstones,
            retention,
            more,
        } => {
            let tombstones = record_sync_list(shared, tls, peer, retention, tombstones, more)?;
            let removed = tombstones.and_then(|tombstones| {
                let mut replica = shared.replica();
                let removed = replica.record_sync(peer, tombstones, retention);
                // The end of the peer's cycle: what can be leased goes now.
                if replica.quarantine().next_deadline().is_some() {
                    replica.sweep_quarantine();
                }
                // Before the answer: when the peer's cycle ends, the report
                // is current.
                shared.save_status(&replica);
                removed
            });
            match removed {
                Ok(paths) => {
                    let parts = batches(paths, BATCH_BYTES);
                    let last = parts.len() - 1;
                    for (i, paths) in parts.into_iter().enumerate() {
                        let more = i < last;
                        write_frame(tls, &Response::Collected { paths, more })?;
                    }
                }
                Err(e) => answer(tls, Err(e))?,
            }
        }
        Request::Watch => {
            push_hints(shared, tls)?;
            return Ok(false);
        }
        Request::Content(_) => {
            return Err(Error::Protocol {
                reason: "content outside an Apply".into(),
            });
        }
    }
    Ok(true)
}

/// Reads the rest of a `RecordSync` (whose first batch is `batch`) and
/// joins its batches into one list. Each batch keeps only the paths that
/// hold that tombstone in our index now, the last batch naming a path wins,
/// so the list is never longer than our own tombstones, whatever the peer
/// sends (issue #5). The outer error is the connection's; the inner one
/// (the index failed) is the answer, once every batch is read.
fn record_sync_list(
    shared: &Shared,
    tls: &mut Tls,
    peer: ReplicaId,
    retention: Duration,
    mut batch: Vec<(RelPath, VersionVector, PeerState)>,
    mut more: bool,
) -> Result<Result<Vec<(RelPath, VersionVector, PeerState)>>> {
    let mut list = BTreeMap::new();
    let mut failed = None;
    loop {
        if batch.iter().any(|(_, _, state)| {
            matches!(state, PeerState::Tombstone(vv) if vv.len() > MAX_PEER_COUNTERS)
        }) {
            return Err(Error::Protocol {
                reason: format!("a RecordSync vector of more than {MAX_PEER_COUNTERS} counters"),
            });
        }
        if failed.is_none() {
            match shared
                .replica()
                .index()
                .retain_current_tombstones(&mut batch)
            {
                Ok(()) => list.extend(batch.into_iter().map(|(p, vv, st)| (p, (vv, st)))),
                Err(e) => failed = Some(e),
            }
        }
        if !more {
            break;
        }
        match read_frame::<_, Request>(tls)? {
            Some(Request::RecordSync {
                peer: p,
                tombstones,
                retention: r,
                more: m,
            }) if p == peer && r == retention => {
                batch = tombstones;
                more = m;
            }
            _ => {
                return Err(Error::Protocol {
                    reason: "RecordSync batch expected".into(),
                });
            }
        }
    }
    Ok(match failed {
        Some(e) => Err(e),
        None => Ok(list.into_iter().map(|(p, (vv, st))| (p, vv, st)).collect()),
    })
}

/// Answers a read (`OpenRead`, `ReadBlocks`): `Reading` and the content,
/// or the error.
fn send_read(tls: &mut Tls, path: &RelPath, reader: Result<Box<dyn ContentReader>>) -> Result<()> {
    match reader {
        Ok(mut reader) => {
            write_frame(tls, &Response::Reading)?;
            // A source failure ended the stream with `Abort`: the client has
            // the error, the connection is in step.
            if let Err(e) = send_content(tls, &mut reader, Response::Content)? {
                tracing::debug!(%path, error = %e, "read aborted");
            }
            Ok(())
        }
        Err(e) => answer(tls, Err(e)),
    }
}

/// Answers `Watch`, then pushes hints until the client goes or the server
/// stops.
fn push_hints(shared: &Shared, tls: &mut Tls) -> Result<()> {
    // Subscribed before the watcher starts, so nothing in between is lost.
    let (tx, rx) = crossbeam_channel::unbounded();
    lock(&shared.subscribers).push(tx.clone());
    let source = shared.replica().watch();
    match source {
        Some(source) => {
            if !shared.fanout.swap(true, Ordering::SeqCst) {
                let subscribers = shared.subscribers.clone();
                std::thread::Builder::new()
                    .name("hint-fanout".into())
                    .spawn(move || {
                        for hint in source {
                            lock(&subscribers).retain(|s| s.send(hint.clone()).is_ok());
                        }
                    })
                    .map_err(|e| Error::io("spawn hint thread", e))?;
            }
        }
        None => {
            lock(&shared.subscribers).retain(|s| !s.same_channel(&tx));
            return write_frame(tls, &Response::Watching(false));
        }
    }
    drop(tx);
    write_frame(tls, &Response::Watching(true))?;
    loop {
        select! {
            recv(rx) -> hint => match hint {
                Ok(hint) => write_frame(tls, &Response::Hint(hint))?,
                Err(_) => return Ok(()),
            },
            recv(shared.stop) -> _ => return Ok(()),
        }
    }
}
