use fastbloom::BloomFilter;
use std::hash::Hash;

/// Nonce replay protection with in-place rotation instead of runtime tasks.
#[derive(Debug)]
pub struct NonceFilter {
    current: BloomFilter,
    previous: BloomFilter,
    rotate_every_ms: u64,
    last_rotate_ms: u64,
}

impl NonceFilter {
    pub fn new(size: usize, p_false: f64, rotate_every_ms: u64, time_ms: u64) -> Self {
        let create_filter = || BloomFilter::with_false_pos(p_false).expected_items(size);

        NonceFilter {
            current: create_filter(),
            previous: create_filter(),
            rotate_every_ms,
            last_rotate_ms: time_ms,
        }
    }

    pub fn check_and_set<T: Hash + ?Sized>(&mut self, time_ms: u64, nonce: &T) -> bool {
        self.rotate_if_needed(time_ms);

        if self.current.contains(nonce) || self.previous.contains(nonce) {
            return false;
        }

        self.current.insert(nonce);
        true
    }

    fn rotate_if_needed(&mut self, time_ms: u64) {
        if time_ms.saturating_sub(self.last_rotate_ms) >= self.rotate_every_ms {
            std::mem::swap(&mut self.current, &mut self.previous);
            self.current.clear();
            self.last_rotate_ms = time_ms;
        }
    }
}
