//! Leases (`F_SETLEASE`, design §5.3 step 4(b), (d) and (f)).
//!
//! The kernel grants a write lease (`F_WRLCK`) on a file only while no other
//! open file description refers to it, writable mmaps included (`O_PATH` fds
//! and the lease's own fd do not count), and a read lease (`F_RDLCK`) while
//! none is open for writing (a writable shared mmap needs such an fd too).
//! While we hold a lease, anyone's `open` that conflicts with it (any open
//! for a write lease, an open for writing or a truncate for a read lease)
//! breaks it: the opener waits until we release the lease (at most
//! `/proc/sys/fs/lease-break-time`), and `F_GETLEASE` stops returning the
//! lease's type. So a lease that is still [`held`](Lease::held) proves that
//! nobody could have written to the file since it was taken, and nobody can
//! until it is released (by dropping the [`Lease`]). [`Lease::take`] tries a
//! write lease first, then a read lease (issue #10): a file only readers
//! have open can be leased too, and a refused read lease proves that a
//! writer has the file open ([`NoLease::Writer`]).
//!
//! Leases only ever shorten waits or close windows: they need a local
//! filesystem and the caller to own the file (or `CAP_LEASE`), so every
//! other failure to take one just means "no lease" and the caller falls back
//! to the grace timer (§5.10).
//!
//! A lease break also sends `SIGIO` to the holder, and `SIGIO` kills a
//! process by default. [`Lease::take`] therefore makes the process ignore
//! `SIGIO` unless it already handles it.

use std::os::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd};
use std::sync::Once;

use rustix::fs::SeekFrom;

use crate::error::Result;
use crate::fs::stat::{Discard, FileKind, Fingerprint, open_for_read, stable_read_with};

/// A lease on one file, held until dropped.
#[derive(Debug)]
pub struct Lease {
    /// `O_RDONLY`; the lease belongs to this open file description.
    fd: OwnedFd,
    /// `F_WRLCK` or `F_RDLCK`.
    kind: libc::c_int,
}

/// Why [`Lease::take`] gave no lease.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NoLease {
    /// Even a read lease was refused: someone has the file open for writing
    /// (or holds a lease on it).
    Writer,
    /// Not the expected regular file, or leases are unavailable here (an
    /// unsupported filesystem, a file we don't own).
    Unavailable,
}

impl Lease {
    /// Takes a lease on the regular file `parent/name`, which must be the
    /// inode `expect` describes (dev and ino): a write lease if nobody else
    /// has the file open, else a read lease if nobody has it open for
    /// writing. Opening the file breaks someone else's lease on it, if there
    /// is one.
    pub fn take(
        parent: BorrowedFd<'_>,
        name: &[u8],
        expect: &Fingerprint,
    ) -> std::result::Result<Lease, NoLease> {
        if expect.kind != FileKind::File {
            return Err(NoLease::Unavailable);
        }
        ignore_sigio();
        let fd = match open_for_read(parent, name) {
            Ok(fd) => fd,
            Err(e) => {
                tracing::debug!(name = %name.escape_ascii(), error = %e, "cannot open file to lease");
                return Err(NoLease::Unavailable);
            }
        };
        match Fingerprint::of_fd(fd.as_fd()) {
            Ok(fp) if fp.same_file(expect) && fp.kind == FileKind::File => {}
            _ => return Err(NoLease::Unavailable),
        }
        for kind in [libc::F_WRLCK, libc::F_RDLCK] {
            // SAFETY: fcntl on an fd we own, with integer arguments only.
            if unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETLEASE, kind) } == 0 {
                return Ok(Lease { fd, kind });
            }
            let e = std::io::Error::last_os_error();
            if e.raw_os_error() != Some(libc::EAGAIN) {
                tracing::debug!(name = %name.escape_ascii(), error = %e, "no lease");
                return Err(NoLease::Unavailable);
            }
        }
        tracing::debug!(name = %name.escape_ascii(), "no lease: open for writing");
        Err(NoLease::Writer)
    }

    /// The lease is unbroken: nobody has opened the file in a way that
    /// conflicts with it since it was taken.
    pub fn held(&self) -> bool {
        // SAFETY: fcntl on an fd we own, with integer arguments only.
        unsafe { libc::fcntl(self.fd.as_raw_fd(), libc::F_GETLEASE) == self.kind }
    }

    /// A write lease: nobody else had the file open at all.
    pub fn is_write(&self) -> bool {
        self.kind == libc::F_WRLCK
    }

    /// The leased file's current fingerprint.
    pub fn fingerprint(&self) -> Result<Fingerprint> {
        Fingerprint::of_fd(self.fd.as_fd())
    }

    /// Hashes the leased file through the lease's own open file description
    /// (opening the file again would break the lease), with the checks of
    /// [`stable_read`](crate::fs::stable_read): fingerprint unchanged during
    /// the read, byte count equal to the size, and `parent/name` still the
    /// same inode at the end.
    pub fn hash(&self, parent: BorrowedFd<'_>, name: &[u8]) -> Result<(Fingerprint, [u8; 32])> {
        stable_read_with(
            name,
            || {
                let fd = rustix::io::fcntl_dupfd_cloexec(&self.fd, 0)?;
                rustix::fs::seek(&fd, SeekFrom::Start(0))?;
                Ok(fd)
            },
            |f2| match Fingerprint::at_opt(parent, name)? {
                Some(f3) if f2.same_file(&f3) => Ok(None),
                Some(_) => Ok(Some("name replaced during read")),
                None => Ok(Some("name removed during read")),
            },
            &mut Discard,
        )
    }
}

/// Makes the process ignore `SIGIO` (sent to a lease holder when its lease
/// is broken), unless a handler is installed already.
pub fn ignore_sigio() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        // SAFETY: sigaction with a zeroed (valid) struct; only the
        // disposition of SIGIO is changed, and only from the default.
        unsafe {
            let mut old: libc::sigaction = std::mem::zeroed();
            if libc::sigaction(libc::SIGIO, std::ptr::null(), &mut old) != 0 {
                return;
            }
            if old.sa_sigaction == libc::SIG_DFL {
                let mut ign: libc::sigaction = std::mem::zeroed();
                ign.sa_sigaction = libc::SIG_IGN;
                libc::sigemptyset(&mut ign.sa_mask);
                libc::sigaction(libc::SIGIO, &ign, std::ptr::null_mut());
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fs::Root;
    use std::fs;
    use std::io::Write;

    fn fx(data: &[u8]) -> (tempfile::TempDir, Root, Fingerprint) {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("f"), data).unwrap();
        let root = Root::open(dir.path()).unwrap();
        let fp = Fingerprint::at(root.fd(), b"f").unwrap();
        (dir, root, fp)
    }

    #[test]
    fn taken_only_without_other_open_fds() {
        let (dir, root, fp) = fx(b"data");
        let lease = Lease::take(root.fd(), b"f", &fp).expect("tmpfs supports leases");
        assert!(lease.held());
        let (now, hash) = lease.hash(root.fd(), b"f").unwrap();
        assert!(now.unchanged(&fp));
        assert_eq!(hash, *blake3::hash(b"data").as_bytes());
        assert!(lease.held(), "reading through the lease keeps it");
        drop(lease);

        assert!(Lease::take(root.fd(), b"f", &fp).unwrap().is_write());

        // A reader's fd: only a read lease.
        let reader = fs::File::open(dir.path().join("f")).unwrap();
        let lease = Lease::take(root.fd(), b"f", &fp).expect("a read lease");
        assert!(!lease.is_write());
        assert!(lease.held());
        drop(lease);
        // A writer's fd refuses both (issue #10).
        let writer = fs::File::options()
            .append(true)
            .open(dir.path().join("f"))
            .unwrap();
        assert_eq!(
            Lease::take(root.fd(), b"f", &fp).unwrap_err(),
            NoLease::Writer
        );
        drop(writer);
        assert!(!Lease::take(root.fd(), b"f", &fp).unwrap().is_write());
        drop(reader);
        assert!(Lease::take(root.fd(), b"f", &fp).unwrap().is_write());

        // Another inode at the name, or a directory: no lease.
        fs::write(dir.path().join("g"), b"other").unwrap();
        assert_eq!(
            Lease::take(root.fd(), b"g", &fp).unwrap_err(),
            NoLease::Unavailable
        );
        fs::create_dir(dir.path().join("d")).unwrap();
        let d = Fingerprint::at(root.fd(), b"d").unwrap();
        assert_eq!(
            Lease::take(root.fd(), b"d", &d).unwrap_err(),
            NoLease::Unavailable
        );
    }

    /// A read lease is broken by an open for writing, not by a reader's.
    #[test]
    fn a_read_lease_is_broken_by_a_writer() {
        let (dir, root, fp) = fx(b"data");
        let path = dir.path().join("f");
        let _reader = fs::File::open(&path).unwrap();
        let lease = Lease::take(root.fd(), b"f", &fp).unwrap();
        assert!(!lease.is_write());
        let _another = fs::File::open(&path).unwrap();
        assert!(lease.held(), "readers don't break it");
        let writer = std::thread::spawn(move || {
            let mut f = fs::File::options().append(true).open(path).unwrap();
            f.write_all(b"+late").unwrap();
        });
        let t0 = std::time::Instant::now();
        while lease.held() {
            assert!(t0.elapsed().as_secs() < 10, "lease never broken");
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        drop(lease);
        writer.join().unwrap();
        assert_eq!(fs::read(dir.path().join("f")).unwrap(), b"data+late");
    }

    #[test]
    fn an_open_breaks_it() {
        let (dir, root, fp) = fx(b"data");
        let lease = Lease::take(root.fd(), b"f", &fp).unwrap();
        let path = dir.path().join("f");
        // The opener blocks until the lease is released.
        let writer = std::thread::spawn(move || {
            let mut f = fs::File::options().append(true).open(path).unwrap();
            f.write_all(b"+late").unwrap();
        });
        let t0 = std::time::Instant::now();
        while lease.held() {
            assert!(t0.elapsed().as_secs() < 10, "lease never broken");
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        assert!(!writer.is_finished(), "the opener waits for the release");
        drop(lease);
        writer.join().unwrap();
        assert_eq!(fs::read(dir.path().join("f")).unwrap(), b"data+late");
    }
}
