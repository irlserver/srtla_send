//! Serial-number arithmetic for SRT sequence numbers (RFC 1982 style, mod 2^31).
//!
//! SRT sequence numbers are 31 bits wide: they count up to `0x7fff_ffff`, wrap
//! back to `0`, and start from a randomized value, so a live stream crosses the
//! wrap at an arbitrary moment rather than only after hours of uptime.
//! Comparing them as plain `i32`s is therefore wrong exactly once per cycle: a
//! post-wrap sequence (small) sorts *below* a pre-wrap one (huge), so every
//! fresh ACK looks like a duplicate and every "newly acked" range looks empty.
//!
//! The fix is the same one SRT itself uses: compare the *distance* between two
//! sequences inside the 31-bit space and call the shorter direction the
//! ordering. Two sequences are only comparable while they are less than half
//! the space (2^30) apart, which is many hours of real traffic and far beyond
//! anything the packet log holds; a wider distance is ambiguous by
//! construction and is reported as "before", i.e. stale.

/// Number of distinct SRT sequence numbers.
const SEQ_SPACE: u32 = 0x8000_0000;

/// Half the sequence space: the largest distance that still has an unambiguous
/// direction.
const SEQ_HALF: u32 = SEQ_SPACE / 2;

/// The 31 significant bits of an SRT sequence number.
pub const SEQ_MASK: i32 = 0x7fff_ffff;

/// Sentinel for `SrtlaConnection::highest_acked_seq` meaning "no cumulative ACK
/// seen yet" — the initial state and the state after every reset.
///
/// It sits outside the 31-bit sequence space, so no real (normalized) sequence
/// can ever collide with it; code must test for it explicitly instead of
/// letting it take part in a comparison, since serial ordering is meaningless
/// against a value that is not a sequence number.
pub const NO_ACK_YET: i32 = i32::MIN;

/// Reduce a wire word to the 31-bit sequence space.
///
/// SRT carries sequence numbers with the MSB clear; a word that arrives with it
/// set is corrupt (or hostile). Masking at the boundary keeps every comparison
/// below well-defined and keeps lookups consistent with the packet log, which
/// is keyed by sequences taken from data headers (MSB clear by construction).
#[inline]
#[must_use]
pub fn seq_normalize(seq: i32) -> i32 {
    seq & SEQ_MASK
}

/// Signed distance `a - b` within the 31-bit space, in `[-2^30, 2^30)`.
///
/// Positive means `a` is ahead of `b`, negative means behind, zero means equal.
/// Both inputs are masked first, so this is total over all `i32`s except the
/// [`NO_ACK_YET`] sentinel, which callers must handle before getting here.
#[inline]
#[must_use]
pub fn seq_diff(a: i32, b: i32) -> i32 {
    let d = (a as u32).wrapping_sub(b as u32) & (SEQ_MASK as u32);
    if d >= SEQ_HALF {
        // The short way round is backwards: re-read the distance as negative.
        d.wrapping_sub(SEQ_SPACE) as i32
    } else {
        d as i32
    }
}

/// True when `a` is strictly ahead of `b` in serial order (wrap-aware `a > b`).
#[inline]
#[must_use]
pub fn seq_after(a: i32, b: i32) -> bool {
    seq_diff(a, b) > 0
}

/// The sequence following `seq`, wrapping `0x7fff_ffff` back to `0`.
#[inline]
#[must_use]
pub fn seq_next(seq: i32) -> i32 {
    ((seq as u32).wrapping_add(1) & (SEQ_MASK as u32)) as i32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normal_ordering() {
        assert!(seq_after(10, 5));
        assert!(!seq_after(5, 10));
        assert_eq!(seq_diff(10, 5), 5);
        assert_eq!(seq_diff(5, 10), -5);
    }

    #[test]
    fn equal_is_not_after() {
        assert!(!seq_after(42, 42));
        assert_eq!(seq_diff(42, 42), 0);
        assert!(!seq_after(0, 0));
        assert!(!seq_after(SEQ_MASK, SEQ_MASK));
    }

    #[test]
    fn wrap_boundary_counts_as_advancing() {
        // 0x7fff_ffff -> 0 is one step forward, not a two-billion step back.
        assert!(seq_after(0, SEQ_MASK));
        assert_eq!(seq_diff(0, SEQ_MASK), 1);
        assert_eq!(seq_next(SEQ_MASK), 0);

        // ...and a short range straddling the wrap keeps its width.
        assert_eq!(seq_diff(2, SEQ_MASK - 1), 4);
        assert!(seq_after(2, SEQ_MASK - 1));
        assert!(!seq_after(SEQ_MASK - 1, 2));
    }

    #[test]
    fn half_space_is_the_ordering_horizon() {
        // Just inside the horizon: still readable as "ahead".
        assert!(seq_after(SEQ_HALF as i32 - 1, 0));
        assert_eq!(seq_diff(SEQ_HALF as i32 - 1, 0), SEQ_HALF as i32 - 1);

        // At and past the horizon the direction is ambiguous, so it reads as
        // "behind" — a far-future (bogus) ACK is treated as stale, never as a
        // giant forward jump.
        assert!(!seq_after(SEQ_HALF as i32, 0));
        assert_eq!(seq_diff(SEQ_HALF as i32, 0), -(SEQ_HALF as i32));
    }

    #[test]
    fn diff_is_antisymmetric() {
        for (a, b) in [(0, 1), (7, 900_001), (SEQ_MASK, 3), (12345, 12345)] {
            assert_eq!(seq_diff(a, b), -seq_diff(b, a), "a={a} b={b}");
        }
    }

    #[test]
    fn normalize_clears_a_corrupt_msb() {
        assert_eq!(seq_normalize(0), 0);
        assert_eq!(seq_normalize(SEQ_MASK), SEQ_MASK);
        assert_eq!(seq_normalize(-1), SEQ_MASK); // 0xffff_ffff
        assert_eq!(seq_normalize(i32::MIN), 0); // 0x8000_0000
    }

    #[test]
    fn next_never_leaves_the_space() {
        assert_eq!(seq_next(0), 1);
        assert_eq!(seq_next(SEQ_MASK - 1), SEQ_MASK);
        assert_eq!(seq_next(SEQ_MASK), 0);
        assert_eq!(seq_next(seq_next(SEQ_MASK)), 1);
    }

    #[test]
    fn sentinel_is_outside_the_sequence_space() {
        assert!(NO_ACK_YET != seq_normalize(NO_ACK_YET));
        for s in [0, 1, SEQ_MASK, SEQ_MASK - 1, 123_456] {
            assert_ne!(s, NO_ACK_YET);
        }
    }
}
