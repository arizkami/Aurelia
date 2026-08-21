//! A fast, non-cryptographic 128-bit content digest.
//!
//! The image cache is content addressed: two calls that hand it the same bytes
//! must resolve to the same [`crate::ImageId`] without decoding twice. That
//! needs a digest with a collision probability low enough that a collision is
//! never the explanation for a bug, and a throughput high enough that hashing a
//! multi-megabyte PNG is cheaper than decoding it (which it is, by two orders of
//! magnitude).
//!
//! This is a multiply-fold construction in the wyhash/xxh3 family: every 32-byte
//! block feeds two independent 64-bit lanes through a 64x64->128 multiply, and
//! the lanes are cross-mixed after each block so that *both* lanes depend on
//! *every* input byte. Keeping the lanes cross-fed is what makes the output
//! genuinely 128-bit wide: with independent lanes, an input that differs only in
//! bytes belonging to one lane would collide at 64-bit strength.
//!
//! It is deliberately **not** cryptographic. An attacker who chooses both inputs
//! can find collisions; nothing in a graphics asset pipeline gives them a reason
//! to, and the alternative (a SHA-2 or BLAKE3 dependency) costs a dependency and
//! roughly 10x the time per byte.

/// Arbitrary odd 64-bit constants with good bit distribution, used to break up
/// structural regularity in the input (long runs of zeroes, repeated rows).
const SECRET: [u64; 4] =
    [0xa076_1d64_78bd_642f, 0xe703_7ed1_a0b4_28db, 0x8ebc_6af0_9c88_c6e3, 0x5899_65cb_7534_2b2e];

/// 64x64 -> 128 multiply, folded back to 64 bits.
///
/// The fold is what provides avalanche: the high half of the product depends on
/// essentially every input bit, so xoring it into the low half spreads a
/// single-bit input change across the whole word.
#[inline(always)]
fn fold_mul(a: u64, b: u64) -> u64 {
    let product = u128::from(a).wrapping_mul(u128::from(b));
    (product as u64) ^ ((product >> 64) as u64)
}

/// Reads eight little-endian bytes at `offset`, or zero when out of range.
///
/// Returning zero instead of panicking keeps this function total; every call
/// site is already bounds-checked by the loop condition, so the fallback is
/// unreachable in practice but costs nothing and removes a panic path.
#[inline(always)]
fn u64_at(bytes: &[u8], offset: usize) -> u64 {
    match bytes.get(offset..).and_then(<[u8]>::first_chunk::<8>) {
        Some(chunk) => u64::from_le_bytes(*chunk),
        None => 0,
    }
}

/// Reads four little-endian bytes at `offset`, widened, or zero when out of range.
#[inline(always)]
fn u32_at(bytes: &[u8], offset: usize) -> u64 {
    match bytes.get(offset..).and_then(<[u8]>::first_chunk::<4>) {
        Some(chunk) => u64::from(u32::from_le_bytes(*chunk)),
        None => 0,
    }
}

/// Packs a 1..=3 byte tail into a word, touching the first, middle and last byte.
///
/// Reading three positions rather than concatenating means a three-byte tail and
/// a one-byte tail with the same first byte still produce different words.
#[inline(always)]
fn tail_small(tail: &[u8]) -> u64 {
    debug_assert!(!tail.is_empty() && tail.len() <= 3);
    (u64::from(tail[0]) << 16)
        | (u64::from(tail[tail.len() >> 1]) << 8)
        | u64::from(tail[tail.len() - 1])
}

/// Hashes `bytes` under `seed`, returning a 128-bit digest.
///
/// The seed separates key domains: a raw pixel buffer and an encoded file that
/// happen to share bytes must not collide, so each caller uses its own seed.
/// The length is folded into both output halves, so appending or removing zero
/// bytes always changes the digest.
pub fn digest(seed: u64, bytes: &[u8]) -> u128 {
    let len = bytes.len() as u64;
    let mut lane_a = seed ^ SECRET[0];
    let mut lane_b = seed.rotate_left(32) ^ SECRET[1];

    let mut rest = bytes;
    while rest.len() >= 32 {
        let t0 = fold_mul(u64_at(rest, 0) ^ SECRET[2], u64_at(rest, 8) ^ lane_a);
        let t1 = fold_mul(u64_at(rest, 16) ^ SECRET[3], u64_at(rest, 24) ^ lane_b);
        // Cross-feed: both lanes must depend on all 32 bytes, or the effective
        // collision strength drops to 64 bits for inputs that differ in one lane.
        lane_a = t0 ^ t1.rotate_left(29);
        lane_b = t1 ^ t0.rotate_left(17);
        rest = &rest[32..];
    }
    while rest.len() >= 16 {
        let t0 = fold_mul(u64_at(rest, 0) ^ SECRET[1], u64_at(rest, 8) ^ lane_a);
        lane_a = t0;
        lane_b = fold_mul(lane_b ^ SECRET[2], t0.rotate_left(23));
        rest = &rest[16..];
    }

    // Tail: read the final bytes with overlap so no byte is skipped and the
    // arrangement differs by length class.
    let (x, y) = match rest.len() {
        0 => (SECRET[0], SECRET[3]),
        1..=3 => (tail_small(rest), SECRET[3] ^ len),
        4..=7 => (u32_at(rest, 0), u32_at(rest, rest.len() - 4)),
        _ => (u64_at(rest, 0), u64_at(rest, rest.len() - 8)),
    };

    let lo = fold_mul(lane_a ^ x ^ len, lane_b ^ y ^ SECRET[1]);
    let hi = fold_mul(lane_b ^ x ^ SECRET[3], lane_a ^ y ^ len ^ SECRET[2]);
    (u128::from(hi) << 64) | u128::from(lo)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_input_is_stable_and_seed_sensitive() {
        assert_eq!(digest(0, &[]), digest(0, &[]));
        assert_ne!(digest(0, &[]), digest(1, &[]));
    }

    #[test]
    fn single_bit_flips_change_the_digest_at_every_length() {
        // Every length class (tail 0..3, 4..7, 8..15, 16..31, >=32) must react to
        // a flip anywhere in the buffer, including the very last byte, which is
        // the classic place a sloppy tail handler drops data.
        for len in [1usize, 3, 4, 7, 8, 15, 16, 31, 32, 33, 64, 100, 4096] {
            let base = vec![0xABu8; len];
            let reference = digest(7, &base);
            for pos in [0, len / 2, len - 1] {
                let mut flipped = base.clone();
                flipped[pos] ^= 0x01;
                assert_ne!(reference, digest(7, &flipped), "len {len} pos {pos} collided");
            }
        }
    }

    #[test]
    fn length_is_part_of_the_digest() {
        // A run of zeroes is exactly the input a length-blind hash collides on,
        // and fully transparent image rows are runs of zeroes.
        let mut seen = std::collections::HashSet::new();
        for len in 0..200usize {
            assert!(seen.insert(digest(0, &vec![0u8; len])), "zero run of {len} collided");
        }
    }

    #[test]
    fn byte_order_matters() {
        assert_ne!(digest(0, b"abcdefgh"), digest(0, b"abcdefhg"));
        assert_ne!(
            digest(0, b"abcdefghijklmnopqrstuvwxyz01234567"),
            digest(0, b"bacdefghijklmnopqrstuvwxyz01234567")
        );
    }

    #[test]
    fn both_output_halves_carry_information() {
        // A digest whose high half is a constant would silently be a 64-bit hash.
        let mut highs = std::collections::HashSet::new();
        let mut lows = std::collections::HashSet::new();
        for i in 0..256u32 {
            let d = digest(0, &i.to_le_bytes());
            highs.insert((d >> 64) as u64);
            lows.insert(d as u64);
        }
        assert_eq!(highs.len(), 256);
        assert_eq!(lows.len(), 256);
    }

    #[test]
    fn no_collisions_over_a_large_structured_corpus() {
        // Structured, highly similar inputs are the realistic adversary here:
        // sprite sheets that differ in one pixel, or the same image at two sizes.
        let mut seen = std::collections::HashSet::new();
        for w in 1..40u32 {
            for h in 1..40u32 {
                let mut buf = vec![0u8; (w * h) as usize];
                let last = buf.len() - 1;
                buf[0] = w as u8;
                buf[last] = h as u8;
                assert!(seen.insert(digest(0, &buf)), "{w}x{h} collided");
            }
        }
    }
}
