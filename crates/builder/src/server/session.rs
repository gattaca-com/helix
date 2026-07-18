use std::time::Instant;

use helix_tcp_types::merging::MERGING_PROTOCOL_VERSION;

/// Per-connection protocol state.
pub struct Session {
    pub state: SessionState,
    pub last_recv: Instant,
    /// Stamped onto outgoing `MergedBlockV1`s, monotonic per connection.
    pub response_id: u32,
}

pub enum SessionState {
    AwaitingRegistration { deadline: Instant },
    Active { zstd: bool },
}

impl Session {
    pub fn awaiting(now: Instant, handshake_timeout: std::time::Duration) -> Self {
        Self {
            state: SessionState::AwaitingRegistration { deadline: now + handshake_timeout },
            last_recv: now,
            response_id: 0,
        }
    }

    pub fn zstd(&self) -> bool {
        matches!(self.state, SessionState::Active { zstd: true, .. })
    }

    pub fn next_response_id(&mut self) -> u32 {
        let id = self.response_id;
        self.response_id += 1;
        id
    }
}

/// Picks the protocol version for a registration, or `None` when the ranges
/// don't overlap. We only speak version 1 today.
pub fn negotiate_version(min_version: u16, max_version: u16) -> Option<u16> {
    (min_version <= MERGING_PROTOCOL_VERSION && MERGING_PROTOCOL_VERSION <= max_version)
        .then_some(MERGING_PROTOCOL_VERSION)
}

/// Constant-time-ish allowlist check: every key is compared in full so the
/// timing doesn't reveal how far a guess matched.
pub fn api_key_allowed(allowlist: &[[u8; 16]], candidate: &[u8; 16]) -> bool {
    let mut allowed = false;
    for key in allowlist {
        let mut diff = 0u8;
        for (a, b) in key.iter().zip(candidate.iter()) {
            diff |= a ^ b;
        }
        allowed |= diff == 0;
    }
    allowed
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_negotiation() {
        assert_eq!(negotiate_version(1, 1), Some(1));
        assert_eq!(negotiate_version(1, 5), Some(1));
        assert_eq!(negotiate_version(0, 1), Some(1));
        assert_eq!(negotiate_version(2, 5), None);
        assert_eq!(negotiate_version(0, 0), None);
    }

    #[test]
    fn api_key_allowlist() {
        let a = [1u8; 16];
        let b = [2u8; 16];
        assert!(api_key_allowed(&[a, b], &a));
        assert!(api_key_allowed(&[a, b], &b));
        assert!(!api_key_allowed(&[a, b], &[3u8; 16]));
        assert!(!api_key_allowed(&[], &a));
    }

    #[test]
    fn response_ids_are_monotonic() {
        let mut session = Session::awaiting(Instant::now(), std::time::Duration::from_secs(1));
        session.state = SessionState::Active { zstd: false };
        assert_eq!(session.next_response_id(), 0);
        assert_eq!(session.next_response_id(), 1);
        assert_eq!(session.next_response_id(), 2);
    }
}
