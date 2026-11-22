//! Hashing of 64-bit keys and a small open-addressing map.

use std::hash::{BuildHasherDefault, Hasher};

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

/// A fast hasher for std maps keyed by u64.
#[derive(Default)]
pub struct MixHasher(u64);

impl Hasher for MixHasher {
    fn finish(&self) -> u64 {
        mix(self.0)
    }
    fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.0 = (self.0.rotate_left(8) ^ b as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15);
        }
    }
    fn write_u64(&mut self, n: u64) {
        self.0 ^= n;
    }
}

pub type MixBuild = BuildHasherDefault<MixHasher>;
pub type FastMap<K, V> = std::collections::HashMap<K, V, MixBuild>;
