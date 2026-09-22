//! # Inclusion Lists Sharing
//!
//! The inclusion list consensus algorithm computes a new inclusion list
//! at the start of the slot (t_0), and after each cutoff point (t_1, t_2).
//!
//! In a timeline, it would look like this:
//!
//! ```text
//! t_0 ---- t_1 ---- t_2 ---- slot end
//! ^        ^        ^
//! |        |        +-- final inclusion list computed here
//! |        +-- shared inclusion list computed here
//! +-- local inclusion list computed here
//! ```
//!
//! The local inclusion list is fetched from a trusted node and passed to
//! [crate::RelayNetworkManager::share_inclusion_list]. Candidate
//! and final shared inclusion lists are computed based on the algorithms
//! detailed in [`consensus::compute_shared_inclusion_list`] and
//! [`consensus::compute_final_inclusion_list`], respectively.
//!
//! After the local and shared inclusion lists are computed, they are
//! broadcasted to peers, which use it to compute the inclusion list
//! at the next cutoff point.
mod consensus;
pub(crate) mod service;
