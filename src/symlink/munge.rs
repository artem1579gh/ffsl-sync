//! rsync `--munge-links` (design §4.4).
//!
//! A replica with munging on stores `/rsyncd-munged/` + target on disk; the
//! index always holds the canonical (unmunged) target. Munging always
//! prepends the prefix and unmunging strips exactly one, so a canonical target
//! that already starts with the prefix is munged twice and the mapping is a
//! bijection between canonical targets (never empty) and munged on-disk
//! targets.

/// rsync's `SYMLINK_PREFIX`.
pub const PREFIX: &[u8] = b"/rsyncd-munged/";

/// The on-disk form of the canonical `target`.
pub fn munge(target: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(PREFIX.len() + target.len());
    out.extend_from_slice(PREFIX);
    out.extend_from_slice(target);
    out
}

/// Whether an on-disk target is a munged one: the munge prefix followed by
/// a target. The prefix alone is not: no canonical target is empty (a
/// symlink cannot point nowhere), so we never write it, and unmunged it
/// would be a target no replica can create (issue #14).
pub fn is_munged(raw: &[u8]) -> bool {
    raw.len() > PREFIX.len() && raw.starts_with(PREFIX)
}

/// The canonical form of the on-disk `raw` target: one prefix stripped. A
/// target that is not munged (a link the user created directly in a munged
/// replica) is returned unchanged; callers that care check [`is_munged`].
pub fn unmunge(raw: &[u8]) -> Vec<u8> {
    if is_munged(raw) {
        raw[PREFIX.len()..].to_vec()
    } else {
        raw.to_vec()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn examples() {
        assert_eq!(munge(b"x"), b"/rsyncd-munged/x");
        assert_eq!(munge(b"/etc/passwd"), b"/rsyncd-munged//etc/passwd");
        assert_eq!(unmunge(b"/rsyncd-munged/../x"), b"../x");
        // Exactly one prefix is stripped.
        assert_eq!(munge(b"/rsyncd-munged/x"), b"/rsyncd-munged//rsyncd-munged/x");
        assert_eq!(unmunge(b"/rsyncd-munged//rsyncd-munged/x"), b"/rsyncd-munged/x");
        // Not munged: unchanged.
        assert!(!is_munged(b"/rsyncd-munge/x"));
        assert_eq!(unmunge(b"/rsyncd-munge/x"), b"/rsyncd-munge/x");
        assert_eq!(unmunge(b"x"), b"x");
        // The prefix alone is not munged: it would unmunge to nothing.
        assert!(!is_munged(PREFIX));
        assert_eq!(unmunge(PREFIX), PREFIX);
    }

    /// Bytes biased towards prefixes of the munge prefix.
    fn arb_target() -> impl Strategy<Value = Vec<u8>> {
        prop_oneof![
            prop::collection::vec(any::<u8>(), 0..40),
            (0usize..3, prop::collection::vec(any::<u8>(), 0..10)).prop_map(|(n, tail)| {
                let mut v = PREFIX.repeat(n);
                v.extend(tail);
                v
            }),
            (0..PREFIX.len()).prop_map(|n| PREFIX[..n].to_vec()),
        ]
    }

    proptest! {
        #[test]
        fn round_trip(x in arb_target()) {
            // Canonical targets are never empty.
            prop_assume!(!x.is_empty());
            let m = munge(&x);
            prop_assert!(is_munged(&m));
            prop_assert_eq!(unmunge(&m), x);
        }

        #[test]
        fn munge_is_injective(x in arb_target(), y in arb_target()) {
            prop_assert_eq!(munge(&x) == munge(&y), x == y);
        }
    }
}
