use async_trait::async_trait;
use helix_common::api::constraints_api::{ConstraintsMessage, SignedPreconferElection};

use crate::error::AuctioneerError;

#[async_trait]
pub trait ConstraintsAuctioneer: Send + Sync + Clone {
    async fn save_new_gateway_election(&self, signed_election: &SignedPreconferElection, slot: u64) -> Result<(), AuctioneerError>;

    /// Returns the elected gateway for a slot. None if there is no elected gateway for the slot.
    async fn get_elected_gateway(&self, slot: u64) -> Result<Option<SignedPreconferElection>, AuctioneerError>;

    /// Save the constraints for a specific slot.
    async fn save_constraints(&self, constraints: &ConstraintsMessage) -> Result<(), AuctioneerError>;

    /// Get the constraints for a specific slot.
    async fn get_constraints(&self, slot: u64) -> Result<Option<ConstraintsMessage>, AuctioneerError>;
}
