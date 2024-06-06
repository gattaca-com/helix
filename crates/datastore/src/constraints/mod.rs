use async_trait::async_trait;
use ethereum_consensus::altair::BlsPublicKey;
use helix_common::api::constraints_api::{ConstraintsMessage, SignedGatewayElection};
use crate::error::AuctioneerError;

#[async_trait]
pub trait ConstraintsAuctioneer: Send + Sync + Clone {
    async fn save_new_gateway_election(&self, signed_election: &SignedGatewayElection, slot: u64) -> Result<(), AuctioneerError>;

    /// Returns the elected gateway for a slot. None if there is no elected gateway for the slot.
    async fn get_elected_gateway(&self, slot: u64) -> Result<Option<SignedGatewayElection>, AuctioneerError>;

    /// Save the constraints for a specific slot.
    async fn save_constraints(&self, constraints: &ConstraintsMessage) -> Result<(), AuctioneerError>;

    /// Get the constraints for a specific slot.
    async fn get_constraints(&self, slot: u64) -> Result<Option<ConstraintsMessage>, AuctioneerError>;
}