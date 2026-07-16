use std::time::{Instant, SystemTime, UNIX_EPOCH};

#[derive(Debug)]
pub struct CoreClock {
    boot_time_ms: u64,
    started_at: Instant,
}

impl CoreClock {
    pub fn new() -> Self {
        Self {
            boot_time_ms: now_millis(),
            started_at: Instant::now(),
        }
    }

    pub fn boot_time_ms(&self) -> u64 {
        self.boot_time_ms
    }
    pub fn elapsed_ms(&self) -> u64 {
        self.started_at.elapsed().as_millis() as u64
    }
}

#[inline]
pub fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("time went backwards")
        .as_millis() as u64
}
