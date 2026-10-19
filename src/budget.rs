//! A memory budget for decoding one untrusted message (design §7.1).
//!
//! A frame's bytes are bounded by `MAX_FRAME`, but its decoded form can be
//! much larger: an empty [`RelPath`](crate::fs::RelPath) is 1 byte on the
//! wire and 24 in memory, a `None` block index 1 byte and 8. So
//! [`read_frame`](crate::replica::proto::read_frame) decodes each frame
//! [`within`] a budget, which the decoders of every collection that can
//! grow with the message charge: the buffer of each list ([`vec`], the
//! counters of a version vector) and the allocation of each path. Bytes
//! decoded one for one (file content, symlink targets, messages) are not
//! charged: the frame limit bounds them already.
//!
//! Outside [`within`] (decoding the index, the journal) nothing is charged.

use std::cell::Cell;
use std::fmt;
use std::marker::PhantomData;

use serde::Deserialize;
use serde::de::{Deserializer, Error, SeqAccess, Visitor};

/// What one heap allocation is charged on top of its bytes: the allocator's
/// overhead, roughly.
pub const ALLOC_OVERHEAD: usize = 16;

thread_local! {
    /// The bytes left to charge, while a [`within`] call runs.
    static LEFT: Cell<Option<usize>> = const { Cell::new(None) };
    /// A charge failed in the running [`within`] call.
    static EXCEEDED: Cell<bool> = const { Cell::new(false) };
}

/// Runs `decode` with a budget of `limit` bytes, for this thread. Returns
/// its result and whether a charge failed (the decoder's error then says
/// little: postcard drops the message of a custom error).
pub fn within<R>(limit: usize, decode: impl FnOnce() -> R) -> (R, bool) {
    struct Restore(Option<usize>, bool);
    impl Drop for Restore {
        fn drop(&mut self) {
            LEFT.with(|l| l.set(self.0));
            EXCEEDED.with(|e| e.set(self.1));
        }
    }
    let _restore = Restore(
        LEFT.with(|l| l.replace(Some(limit))),
        EXCEEDED.with(|e| e.replace(false)),
    );
    let r = decode();
    (r, EXCEEDED.with(Cell::get))
}

/// Charges `bytes` to the running budget, if any; fails once it is spent.
pub fn charge<E: Error>(bytes: usize) -> Result<(), E> {
    LEFT.with(|l| match l.get() {
        None => Ok(()),
        Some(left) if bytes <= left => {
            l.set(Some(left - bytes));
            Ok(())
        }
        Some(_) => {
            EXCEEDED.with(|e| e.set(true));
            Err(E::custom("memory budget exceeded"))
        }
    })
}

/// For a push onto a full list of `T` of `capacity` elements: charges its
/// growth (it doubles, by at least 4) and returns the new capacity.
pub fn grow<T, E: Error>(capacity: usize) -> Result<usize, E> {
    let extra = capacity.max(4);
    charge(extra.saturating_mul(size_of::<T>()))?;
    Ok(capacity + extra)
}

/// Serde `deserialize_with` for a `Vec<T>` that charges its buffer to the
/// budget as it grows (the encoded length is not trusted for the
/// allocation either).
pub fn vec<'de, D: Deserializer<'de>, T: Deserialize<'de>>(d: D) -> Result<Vec<T>, D::Error> {
    struct V<T>(PhantomData<T>);

    impl<'de, T: Deserialize<'de>> Visitor<'de> for V<T> {
        type Value = Vec<T>;

        fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
            f.write_str("a sequence")
        }

        fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Vec<T>, A::Error> {
            let mut out = Vec::new();
            while let Some(item) = seq.next_element()? {
                if out.len() == out.capacity() {
                    let cap = grow::<T, A::Error>(out.capacity())?;
                    out.reserve_exact(cap - out.len());
                }
                out.push(item);
            }
            Ok(out)
        }
    }

    d.deserialize_seq(V(PhantomData))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Serialize;

    #[derive(Debug, Serialize, Deserialize, PartialEq)]
    struct Msg(#[serde(deserialize_with = "vec")] Vec<u64>);

    fn decode(bytes: &[u8]) -> postcard::Result<Msg> {
        postcard::from_bytes(bytes)
    }

    #[test]
    fn lists_are_charged_while_a_budget_runs() {
        let bytes = postcard::to_stdvec(&Msg(vec![0; 1000])).unwrap();
        // 1000 one-byte elements, 8 bytes each in memory.
        assert_eq!(bytes.len(), 1002);
        let (r, exceeded) = within(8 * 1024, || decode(&bytes));
        assert!(r.is_ok() && !exceeded);
        let (r, exceeded) = within(4 * 1024, || decode(&bytes));
        assert!(r.is_err() && exceeded);
        // No budget: nothing is charged.
        assert_eq!(decode(&bytes).unwrap().0.len(), 1000);
    }

    #[test]
    fn the_budget_is_per_call_and_restored() {
        let bytes = postcard::to_stdvec(&Msg(vec![7; 100])).unwrap();
        for _ in 0..10 {
            assert!(within(1024, || decode(&bytes)).0.is_ok());
        }
        let ((), exceeded) = within(1024, || {
            assert!(within(16, || decode(&bytes)).1);
            // The outer budget is back, untouched by the inner one.
            assert!(decode(&bytes).is_ok());
        });
        assert!(!exceeded);
        assert!(LEFT.with(Cell::get).is_none());
    }
}
