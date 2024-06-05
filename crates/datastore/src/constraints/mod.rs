use async_trait::async_trait;
use ethereum_consensus::altair::BlsPublicKey;
use crate::error::AuctioneerError;

#[async_trait]
pub trait ConstraintsAuctioneer: Send + Sync + Clone {
    async fn save_new_gateway_election(&self, gateway_public_key: &BlsPublicKey, slot: u64) -> Result<(), AuctioneerError>;

    /// Returns the elected gateway public key for a slot. None if there is no elected gateway for the slot.
    async fn get_gateway(&self, slot: u64) -> Result<Option<BlsPublicKey>, AuctioneerError>;
}