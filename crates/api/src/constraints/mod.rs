pub mod api;
pub mod error;
pub mod tests;
pub mod types;

/// We don't accept any new block constraints that are more than 8 seconds into the prev slot.
pub const SET_CONSTRAINTS_CUTOFF_NS: i64 = 8_000_000_000;
