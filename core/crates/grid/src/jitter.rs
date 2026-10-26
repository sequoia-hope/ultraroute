//! A seeded shuffle, so a search can run the same router many ways: seed 0 leaves the order as
//! it was, any other seed reorders it the same way every time.

/// The next value of a xorshift generator.
fn next(state: &mut u64) -> u64 {
    *state ^= *state << 13;
    *state ^= *state >> 7;
    *state ^= *state << 17;
    *state
}

/// Shuffles `items` by `seed` (Fisher-Yates); seed 0 leaves them be.
pub fn shuffle<T>(items: &mut [T], seed: u64) {
    if seed == 0 {
        return;
    }
    let mut state = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    for i in (1..items.len()).rev() {
        let j = (next(&mut state) % (i as u64 + 1)) as usize;
        items.swap(i, j);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seed_zero_keeps_the_order_and_others_reorder_it_the_same_way_each_time() {
        let mut a: Vec<u32> = (0..20).collect();
        shuffle(&mut a, 0);
        assert_eq!(a, (0..20).collect::<Vec<_>>());
        let (mut b, mut c) = (a.clone(), a.clone());
        shuffle(&mut b, 7);
        shuffle(&mut c, 7);
        assert_eq!(b, c);
        assert_ne!(b, a);
        b.sort_unstable();
        assert_eq!(b, a);
    }
}
