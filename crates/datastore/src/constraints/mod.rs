use async_trait::async_trait;
use crate::error::AuctioneerError;

#[async_trait]
#[auto_impl::auto_impl(Arc)]
pub trait ConstraintsAuctioneer: Send + Sync + Clone {
    async fn get_last_slot_delivered(&self) -> Result<Option<u64>, AuctioneerError>;
}