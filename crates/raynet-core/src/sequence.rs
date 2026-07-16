use std::collections::HashMap;

use crate::wire::{TIME_BITS, TIME_MASK};

pub(crate) const TS_SHIFT: u32 = 10;
pub(crate) const MAX_PAST_TICKS: i32 = 645;
const MAX_FUTURE_TICKS: i32 = 59;
const WINDOW_BITS: u32 = 1 << 20;
const WINDOW_WORDS: usize = WINDOW_BITS as usize / 64;

#[cfg_attr(test, derive(Debug, PartialEq, Eq))]
pub(crate) enum SequenceDrop {
    Freshness,
    Replay,
}

struct SequenceWindow {
    max_seq_no: u64,
    newest_time: u32,
    bitmap: Box<[u64]>,
}

impl SequenceWindow {
    fn new(seq_no: u64, time: u32) -> Self {
        let mut window = Self {
            max_seq_no: seq_no,
            newest_time: time,
            bitmap: vec![0; WINDOW_WORDS].into_boxed_slice(),
        };
        window.set(seq_no);
        window
    }

    fn insert(&mut self, seq_no: u64, time: u32) -> Result<(), SequenceDrop> {
        if seq_no <= self.max_seq_no {
            if self.max_seq_no - seq_no >= u64::from(WINDOW_BITS) {
                return Err(SequenceDrop::Replay);
            }
            if self.is_set(seq_no) {
                return Err(SequenceDrop::Replay);
            }
            self.set(seq_no);
        } else {
            let delta = seq_no - self.max_seq_no;
            if delta >= u64::from(WINDOW_BITS) {
                self.bitmap.fill(0);
            } else {
                self.clear_after(self.max_seq_no, delta as u32);
            }
            self.max_seq_no = seq_no;
            self.set(seq_no);
        }
        if time_diff(time, self.newest_time) > 0 {
            self.newest_time = time;
        }
        Ok(())
    }

    fn clear_after(&mut self, old_max: u64, count: u32) {
        let mut bit = (old_max.wrapping_add(1) as u32) & (WINDOW_BITS - 1);
        let mut remaining = count;
        while remaining != 0 && bit & 63 != 0 {
            self.clear_bit(bit);
            bit = (bit + 1) & (WINDOW_BITS - 1);
            remaining -= 1;
        }
        while remaining >= 64 {
            self.bitmap[(bit as usize) >> 6] = 0;
            bit = (bit + 64) & (WINDOW_BITS - 1);
            remaining -= 64;
        }
        while remaining != 0 {
            self.clear_bit(bit);
            bit = (bit + 1) & (WINDOW_BITS - 1);
            remaining -= 1;
        }
    }

    fn clear_bit(&mut self, bit: u32) {
        let bit = bit as usize;
        self.bitmap[bit >> 6] &= !(1 << (bit & 63));
    }

    fn is_set(&self, seq_no: u64) -> bool {
        let bit = (seq_no as u32 & (WINDOW_BITS - 1)) as usize;
        self.bitmap[bit >> 6] & (1 << (bit & 63)) != 0
    }

    fn set(&mut self, seq_no: u64) {
        let bit = (seq_no as u32 & (WINDOW_BITS - 1)) as usize;
        self.bitmap[bit >> 6] |= 1 << (bit & 63);
    }
}

#[derive(Default)]
pub(crate) struct SequenceFilter {
    windows: HashMap<u64, SequenceWindow>,
}

impl SequenceFilter {
    pub(crate) fn len(&self) -> usize {
        self.windows.len()
    }

    pub(crate) fn check_and_insert(
        &mut self,
        current_time: u32,
        packet_time: u32,
        seq_id: u64,
        seq_no: u64,
    ) -> Result<(), SequenceDrop> {
        let age = time_diff(current_time, packet_time);
        if age > MAX_PAST_TICKS {
            return Err(SequenceDrop::Freshness);
        }
        if age < -MAX_FUTURE_TICKS {
            return Err(SequenceDrop::Freshness);
        }
        if let Some(window) = self.windows.get_mut(&seq_id) {
            return window.insert(seq_no, packet_time);
        }
        self.windows
            .insert(seq_id, SequenceWindow::new(seq_no, packet_time));
        Ok(())
    }

    pub(crate) fn remove_expired(&mut self, current_time: u32, cap: usize) -> Vec<u64> {
        if self.windows.len() <= cap {
            return Vec::new();
        }
        let mut expired: Vec<_> = self
            .windows
            .iter()
            .filter(|(_, window)| time_diff(current_time, window.newest_time) > MAX_PAST_TICKS)
            .map(|(&seq_id, window)| (seq_id, window.newest_time))
            .collect();
        expired.sort_by_key(|(_, time)| time_diff(current_time, *time));
        expired.reverse();
        let remove_count = expired.len().min(self.windows.len() - cap);
        expired.truncate(remove_count);
        for (seq_id, _) in &expired {
            self.windows.remove(seq_id);
        }
        expired.into_iter().map(|(seq_id, _)| seq_id).collect()
    }

    pub(crate) fn next_expiry_delay_ms(&self, current_time: u32, cap: usize) -> Option<u64> {
        if self.windows.len() <= cap {
            return None;
        }
        self.windows
            .values()
            .map(|window| {
                let age = i64::from(time_diff(current_time, window.newest_time));
                ((i64::from(MAX_PAST_TICKS) + 1 - age).max(0) as u64) << TS_SHIFT
            })
            .min()
    }
}

pub(crate) fn time_diff(later: u32, earlier: u32) -> i32 {
    let difference = later.wrapping_sub(earlier) & TIME_MASK;
    if difference >= 1 << (TIME_BITS - 1) {
        difference as i32 - (1 << TIME_BITS)
    } else {
        difference as i32
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn window_accepts_reordering_and_rejects_duplicates() {
        let mut filter = SequenceFilter::default();
        for seq in [3, 1, 2, 5, 4] {
            assert_eq!(filter.check_and_insert(100, 100, 7, seq), Ok(()));
        }
        assert_eq!(
            filter.check_and_insert(100, 100, 7, 3),
            Err(SequenceDrop::Replay)
        );
        assert_eq!(
            filter.check_and_insert(100, 100, 7, u64::from(3 + WINDOW_BITS)),
            Ok(())
        );
        assert_eq!(
            filter.check_and_insert(100, 100, 7, 3),
            Err(SequenceDrop::Replay)
        );
    }

    #[test]
    fn freshness_handles_boundaries_and_wraparound() {
        let mut filter = SequenceFilter::default();
        assert_eq!(filter.check_and_insert(0, TIME_MASK, 1, 0), Ok(()));
        assert_eq!(
            filter.check_and_insert(1000, 1000 - MAX_PAST_TICKS as u32, 2, 0),
            Ok(())
        );
        assert_eq!(
            filter.check_and_insert(1000, 1000 - MAX_PAST_TICKS as u32 - 1, 3, 0),
            Err(SequenceDrop::Freshness)
        );
        assert_eq!(
            filter.check_and_insert(1000, 1000 + MAX_FUTURE_TICKS as u32 + 1, 4, 0),
            Err(SequenceDrop::Freshness)
        );
    }

    #[test]
    fn inserting_a_new_seq_id_cleans_expired_windows() {
        let mut filter = SequenceFilter::default();
        assert_eq!(filter.check_and_insert(0, 0, 1, 0), Ok(()));
        assert_eq!(filter.len(), 1);
        assert_eq!(
            filter.check_and_insert(MAX_PAST_TICKS as u32 + 1, MAX_PAST_TICKS as u32 + 1, 2, 0),
            Ok(())
        );
        filter.remove_expired(MAX_PAST_TICKS as u32 + 1, 0);
        assert_eq!(filter.len(), 1);
        assert!(filter.windows.contains_key(&2));
    }
}
