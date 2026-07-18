use std::time::{SystemTime, UNIX_EPOCH};

/// Wall-clock nanoseconds since the unix epoch (`MergeTraceV1` timestamps).
pub fn utcnow_ns() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_nanos() as u64
}
