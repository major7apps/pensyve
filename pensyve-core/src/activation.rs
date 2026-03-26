//! ACT-R base-level activation for memory retrieval scoring.
//!
//! Implements `B(m,t) = ln(Σ (t - t_k)^(-d))` from Anderson (2007),
//! which combines recency and frequency of memory access into a single score.

use serde::{Deserialize, Serialize};

/// Computes ACT-R base-level activation for a memory item.
///
/// Formula: `B(m,t) = ln(Σ (t - t_k)^(-d))`
///
/// - `access_times`: UNIX timestamps (seconds) of prior accesses to this memory.
/// - `now`: current UNIX timestamp (seconds).
/// - `decay`: decay parameter `d` (Anderson 2007 recommends 0.5; lower = slower decay).
///
/// Returns `-20.0` when `access_times` is empty (no access history).
pub fn base_level_activation(access_times: &[f64], now: f64, decay: f32) -> f32 {
    if access_times.is_empty() {
        return -20.0;
    }

    let d = f64::from(decay);
    let sum: f64 = access_times
        .iter()
        .filter_map(|&t_k| {
            let elapsed = now - t_k;
            if elapsed > 0.0 {
                Some(elapsed.powf(-d))
            } else {
                // Access in the future or at exactly now — skip to avoid NaN/inf.
                None
            }
        })
        .sum();

    if sum <= 0.0 {
        return -20.0;
    }

    sum.ln() as f32
}

/// Fixed-capacity ring buffer for memory access timestamps.
///
/// Stores the last `capacity` access timestamps per memory item so that
/// base-level activation can be computed at retrieval time without unbounded
/// growth. Oldest entries are evicted automatically when the buffer is full.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccessRingBuffer {
    /// Circular storage for timestamps.
    buf: Vec<f64>,
    /// Index of the next write slot.
    head: usize,
    /// Number of valid entries currently in the buffer (≤ capacity).
    len: usize,
}

impl AccessRingBuffer {
    /// Creates an empty ring buffer with the given capacity.
    pub fn new(capacity: usize) -> Self {
        assert!(capacity > 0, "capacity must be > 0");
        Self {
            buf: vec![0.0; capacity],
            head: 0,
            len: 0,
        }
    }

    /// Returns the maximum number of timestamps this buffer can hold.
    pub fn capacity(&self) -> usize {
        self.buf.len()
    }

    /// Returns the number of valid timestamps currently stored.
    pub fn len(&self) -> usize {
        self.len
    }

    /// Returns `true` if no timestamps are stored.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Adds a timestamp, evicting the oldest entry if the buffer is full.
    pub fn push(&mut self, timestamp: f64) {
        self.buf[self.head] = timestamp;
        self.head = (self.head + 1) % self.buf.len();
        if self.len < self.buf.len() {
            self.len += 1;
        }
    }

    /// Returns timestamps in chronological order (oldest first).
    pub fn timestamps(&self) -> Vec<f64> {
        if self.len == 0 {
            return Vec::new();
        }

        let cap = self.buf.len();
        let mut out = Vec::with_capacity(self.len);

        if self.len < cap {
            // Buffer not yet full — valid entries occupy [0..len) in insertion order.
            // head == len in this case.
            out.extend_from_slice(&self.buf[..self.len]);
        } else {
            // Buffer is full. `head` points to the oldest entry.
            out.extend_from_slice(&self.buf[self.head..]);
            out.extend_from_slice(&self.buf[..self.head]);
        }

        out
    }

    /// Computes ACT-R base-level activation using the stored timestamps.
    pub fn activation(&self, now: f64, decay: f32) -> f32 {
        let ts = self.timestamps();
        base_level_activation(&ts, now, decay)
    }

    /// Creates a synthetic evenly-spaced access history for migration from
    /// legacy data that only has `last_accessed` and `access_count`.
    ///
    /// Distributes `access_count` synthetic accesses over the interval
    /// `[last_accessed - access_count, last_accessed]` with 1-second spacing,
    /// capped at `capacity`.
    pub fn bootstrap(last_accessed: f64, access_count: u32, capacity: usize) -> Self {
        let mut buf = Self::new(capacity);
        if access_count == 0 {
            return buf;
        }

        // Generate `access_count` synthetic timestamps spaced 1 second apart,
        // ending at `last_accessed`.  We only keep the most recent `capacity`
        // of them (the ring buffer evicts the rest automatically).
        let count = f64::from(access_count);
        for i in 0..access_count {
            let t = last_accessed - (count - 1.0 - f64::from(i));
            buf.push(t);
        }

        buf
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_single_access_returns_zero() {
        // elapsed = 1.0, d = 0.5 → sum = 1.0^(-0.5) = 1.0 → ln(1.0) = 0.0
        let now = 1000.0_f64;
        let access_times = [now - 1.0];
        let result = base_level_activation(&access_times, now, 0.5);
        assert!((result - 0.0).abs() < 1e-5, "expected ≈0.0, got {result}");
    }

    #[test]
    fn test_more_accesses_increase_activation() {
        let now = 1000.0_f64;
        let one_access = [now - 10.0];
        let three_accesses = [now - 10.0, now - 20.0, now - 30.0];

        let b1 = base_level_activation(&one_access, now, 0.5);
        let b3 = base_level_activation(&three_accesses, now, 0.5);

        assert!(
            b3 > b1,
            "3 accesses ({b3}) should yield higher activation than 1 access ({b1})"
        );
    }

    #[test]
    fn test_recent_access_higher_than_old() {
        let now = 1000.0_f64;
        let recent = [now - 5.0];
        let old = [now - 90.0];

        let b_recent = base_level_activation(&recent, now, 0.5);
        let b_old = base_level_activation(&old, now, 0.5);

        assert!(
            b_recent > b_old,
            "recent access ({b_recent}) should be higher than old ({b_old})"
        );
    }

    #[test]
    fn test_decay_parameter_affects_rate() {
        // Elapsed = 100s.  Higher decay → (100)^(-d) is smaller → lower activation.
        let now = 1000.0_f64;
        let access_times = [now - 100.0];

        let b_slow = base_level_activation(&access_times, now, 0.3);
        let b_fast = base_level_activation(&access_times, now, 0.7);

        assert!(
            b_slow > b_fast,
            "d=0.3 should yield higher activation ({b_slow}) than d=0.7 ({b_fast}) for old memory"
        );
    }

    #[test]
    fn test_empty_access_history() {
        let result = base_level_activation(&[], 1000.0, 0.5);
        assert!(
            result < -10.0,
            "empty history should return < -10.0, got {result}"
        );
    }

    #[test]
    fn test_access_ring_buffer() {
        let mut buf = AccessRingBuffer::new(5);

        // Push 8 items; only the last 5 should be retained.
        for i in 1u32..=8 {
            buf.push(i as f64 * 10.0);
        }

        assert_eq!(buf.len(), 5, "buffer should hold exactly 5 items");

        let ts = buf.timestamps();
        assert_eq!(ts.len(), 5);

        // Oldest retained item is the 4th pushed (40.0 → wait, items 4..=8 kept)
        // Items pushed: 10, 20, 30, 40, 50, 60, 70, 80.
        // After 8 pushes with cap=5, retained are: 40, 50, 60, 70, 80.
        assert_eq!(
            ts,
            vec![40.0, 50.0, 60.0, 70.0, 80.0],
            "timestamps() should return oldest-first: {ts:?}"
        );
    }
}
