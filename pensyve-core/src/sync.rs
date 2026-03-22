//! Cross-device memory synchronization.
//!
//! Uses vector clocks for conflict-free merging of memories across
//! devices. Each device maintains a monotonic sequence number; during
//! sync, the higher-numbered version wins for the same memory ID.

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// A vector clock entry for a single device.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct VectorClock {
    /// Device ID → sequence number mapping.
    pub clocks: HashMap<String, u64>,
}

impl VectorClock {
    /// Create a new empty vector clock.
    pub fn new() -> Self {
        Self {
            clocks: HashMap::new(),
        }
    }

    /// Increment the clock for a device.
    pub fn tick(&mut self, device_id: &str) {
        let counter = self.clocks.entry(device_id.to_string()).or_insert(0);
        *counter += 1;
    }

    /// Get the sequence number for a device.
    pub fn get(&self, device_id: &str) -> u64 {
        self.clocks.get(device_id).copied().unwrap_or(0)
    }

    /// Merge two vector clocks, taking the max of each entry.
    pub fn merge(&mut self, other: &VectorClock) {
        for (device, &seq) in &other.clocks {
            let current = self.clocks.entry(device.clone()).or_insert(0);
            *current = (*current).max(seq);
        }
    }

    /// Returns true if self dominates other (all entries >= other).
    pub fn dominates(&self, other: &VectorClock) -> bool {
        for (device, &seq) in &other.clocks {
            if self.get(device) < seq {
                return false;
            }
        }
        true
    }

    /// Returns true if the clocks are concurrent (neither dominates).
    pub fn is_concurrent(&self, other: &VectorClock) -> bool {
        !self.dominates(other) && !other.dominates(self)
    }
}

impl Default for VectorClock {
    fn default() -> Self {
        Self::new()
    }
}

/// A memory change to be synced between devices.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyncEntry {
    /// Memory ID.
    pub memory_id: Uuid,
    /// Type of change.
    pub operation: SyncOperation,
    /// Vector clock at the time of the change.
    pub clock: VectorClock,
    /// Timestamp of the change.
    pub timestamp: DateTime<Utc>,
    /// Device that made the change.
    pub device_id: String,
}

/// Type of sync operation.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum SyncOperation {
    /// Memory was created or updated.
    Upsert,
    /// Memory was deleted.
    Delete,
}

/// Resolve a conflict between two sync entries for the same memory.
///
/// Resolution strategy:
/// 1. If one clock dominates, the dominating entry wins.
/// 2. If concurrent, the later timestamp wins (last-writer-wins).
pub fn resolve_conflict(local: &SyncEntry, remote: &SyncEntry) -> ConflictResolution {
    if local.clock.dominates(&remote.clock) {
        ConflictResolution::KeepLocal
    } else if remote.clock.dominates(&local.clock) {
        ConflictResolution::KeepRemote
    } else {
        // Concurrent — last writer wins
        if local.timestamp >= remote.timestamp {
            ConflictResolution::KeepLocal
        } else {
            ConflictResolution::KeepRemote
        }
    }
}

/// Result of a conflict resolution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConflictResolution {
    KeepLocal,
    KeepRemote,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_vector_clock_tick() {
        let mut clock = VectorClock::new();
        clock.tick("device-a");
        clock.tick("device-a");
        clock.tick("device-b");
        assert_eq!(clock.get("device-a"), 2);
        assert_eq!(clock.get("device-b"), 1);
        assert_eq!(clock.get("device-c"), 0);
    }

    #[test]
    fn test_vector_clock_merge() {
        let mut a = VectorClock::new();
        a.tick("d1");
        a.tick("d1");
        a.tick("d2");

        let mut b = VectorClock::new();
        b.tick("d1");
        b.tick("d2");
        b.tick("d2");
        b.tick("d3");

        a.merge(&b);
        assert_eq!(a.get("d1"), 2); // max(2, 1)
        assert_eq!(a.get("d2"), 2); // max(1, 2)
        assert_eq!(a.get("d3"), 1); // max(0, 1)
    }

    #[test]
    fn test_dominates() {
        let mut a = VectorClock::new();
        a.tick("d1");
        a.tick("d1");

        let mut b = VectorClock::new();
        b.tick("d1");

        assert!(a.dominates(&b));
        assert!(!b.dominates(&a));
    }

    #[test]
    fn test_concurrent() {
        let mut a = VectorClock::new();
        a.tick("d1");

        let mut b = VectorClock::new();
        b.tick("d2");

        assert!(a.is_concurrent(&b));
        assert!(!a.dominates(&b));
        assert!(!b.dominates(&a));
    }

    #[test]
    fn test_resolve_dominating() {
        let mut clock_a = VectorClock::new();
        clock_a.tick("d1");
        clock_a.tick("d1");

        let mut clock_b = VectorClock::new();
        clock_b.tick("d1");

        let local = SyncEntry {
            memory_id: Uuid::new_v4(),
            operation: SyncOperation::Upsert,
            clock: clock_a,
            timestamp: Utc::now(),
            device_id: "d1".to_string(),
        };
        let remote = SyncEntry {
            memory_id: local.memory_id,
            operation: SyncOperation::Upsert,
            clock: clock_b,
            timestamp: Utc::now(),
            device_id: "d2".to_string(),
        };

        assert_eq!(
            resolve_conflict(&local, &remote),
            ConflictResolution::KeepLocal
        );
    }

    #[test]
    fn test_resolve_concurrent_last_writer_wins() {
        let mut clock_a = VectorClock::new();
        clock_a.tick("d1");

        let mut clock_b = VectorClock::new();
        clock_b.tick("d2");

        let earlier = Utc::now() - chrono::Duration::seconds(10);
        let later = Utc::now();

        let local = SyncEntry {
            memory_id: Uuid::new_v4(),
            operation: SyncOperation::Upsert,
            clock: clock_a,
            timestamp: earlier,
            device_id: "d1".to_string(),
        };
        let remote = SyncEntry {
            memory_id: local.memory_id,
            operation: SyncOperation::Upsert,
            clock: clock_b,
            timestamp: later,
            device_id: "d2".to_string(),
        };

        assert_eq!(
            resolve_conflict(&local, &remote),
            ConflictResolution::KeepRemote
        );
    }
}
