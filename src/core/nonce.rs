use fastbloom::BloomFilter;
use std::hash::Hash;
use std::time::{Duration, Instant};

/// Nonce replay protection with in-place rotation instead of runtime tasks.
pub struct NonceFilter {
    current: BloomFilter,
    previous: BloomFilter,
    rotate_every: Duration,
    last_rotate: Instant,
}

impl NonceFilter {
    pub fn new(size: usize, p_false: f64, timeout: Duration) -> Self {
        let create_filter = || BloomFilter::with_false_pos(p_false).expected_items(size);

        NonceFilter {
            current: create_filter(),
            previous: create_filter(),
            rotate_every: timeout,
            last_rotate: Instant::now(),
        }
    }

    pub fn check_and_set<T: Hash + ?Sized>(&mut self, nonce: &T) -> bool {
        self.rotate_if_needed(Instant::now());

        if self.current.contains(nonce) || self.previous.contains(nonce) {
            return false;
        }

        self.current.insert(nonce);
        true
    }

    fn rotate_if_needed(&mut self, now: Instant) {
        if now.duration_since(self.last_rotate) >= self.rotate_every {
            std::mem::swap(&mut self.current, &mut self.previous);
            self.current.clear();
            self.last_rotate = now;
        }
    }
}
