//! Hashing of 64-bit keys and a small open-addressing map.

#[inline]
pub fn mix(mut x: u64) -> u64 {
    x ^= x >> 33;
    x = x.wrapping_mul(0xff51_afd7_ed55_8ccd);
    x ^= x >> 33;
    x = x.wrapping_mul(0xc4ce_b9fe_1a85_ec53);
    x ^= x >> 33;
    x
}

/// Maps a hash to `0..n` with its high 32 bits, leaving the low bits for tables.
#[inline]
pub fn bucket(hash: u64, n: usize) -> usize {
    (((hash >> 32) * n as u64) >> 32) as usize
}
