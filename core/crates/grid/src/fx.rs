//! Fast deterministic hashing for integer keys and internal caches.
//!
//! Standard `HashMap` uses `SipHash` seeded from the OS on each run: safe against chosen keys,
//! slow on millions of `u32` cell indices, and its random seed varies iteration order from run to
//! run. Inside the grid crate the keys are board coordinates and cell indices of our own making,
//! so a multiply-xorshift word mixer gives reproducible tables at a fraction of the cost.

use std::collections::{HashMap, HashSet};
use std::hash::{BuildHasherDefault, Hasher};

const SEED1: u64 = 0x517c_c1b7_2722_0a95;
const SEED2: u64 = 0x9e37_79b9_7f4a_7c15;

#[derive(Clone, Copy)]
pub(crate) struct FxHasher {
    hash: u64,
}

impl Default for FxHasher {
    #[inline]
    fn default() -> FxHasher {
        FxHasher { hash: 0 }
    }
}

impl FxHasher {
    #[inline]
    pub(crate) fn with_seed(seed: u64) -> FxHasher {
        FxHasher { hash: seed }
    }

    #[inline]
    fn word(&mut self, word: u64) {
        self.hash = (self.hash.rotate_left(5) ^ word).wrapping_mul(SEED1);
    }
}

impl Hasher for FxHasher {
    #[inline]
    fn finish(&self) -> u64 {
        let h = self.hash;
        (h ^ (h >> 32)).wrapping_mul(SEED2)
    }

    #[inline]
    fn write(&mut self, mut bytes: &[u8]) {
        while bytes.len() >= 8 {
            let (head, rest) = bytes.split_at(8);
            self.word(u64::from_le_bytes(head.try_into().unwrap()));
            bytes = rest;
        }
        if bytes.len() >= 4 {
            let (head, rest) = bytes.split_at(4);
            self.word(u32::from_le_bytes(head.try_into().unwrap()) as u64);
            bytes = rest;
        }
        for &b in bytes {
            self.word(b as u64);
        }
    }

    #[inline]
    fn write_u8(&mut self, i: u8) {
        self.word(i as u64);
    }

    #[inline]
    fn write_u16(&mut self, i: u16) {
        self.word(i as u64);
    }

    #[inline]
    fn write_u32(&mut self, i: u32) {
        self.word(i as u64);
    }

    #[inline]
    fn write_u64(&mut self, i: u64) {
        self.word(i);
    }

    #[inline]
    fn write_usize(&mut self, i: usize) {
        self.word(i as u64);
    }

    #[inline]
    fn write_i32(&mut self, i: i32) {
        self.word(i as u32 as u64);
    }

    #[inline]
    fn write_i64(&mut self, i: i64) {
        self.word(i as u64);
    }
}

pub(crate) type FxHashMap<K, V> = HashMap<K, V, BuildHasherDefault<FxHasher>>;
#[allow(dead_code)]
pub(crate) type FxHashSet<T> = HashSet<T, BuildHasherDefault<FxHasher>>;

/// Two independent 64-bit passes over `feed` packed into 128 bits, for cache keys that stand in
/// for whole fixed-copper lists.
pub(crate) fn hash128(mut feed: impl FnMut(&mut FxHasher)) -> u128 {
    let mut h1 = FxHasher::with_seed(SEED1);
    let mut h2 = FxHasher::with_seed(SEED2);
    feed(&mut h1);
    feed(&mut h2);
    (u128::from(h1.finish()) << 64) | u128::from(h2.finish())
}
