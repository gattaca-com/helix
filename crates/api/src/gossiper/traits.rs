use async_trait::async_trait;
use crate::builder::SubmitBlockParams;
use crate::gossiper::error::GossipError;

#[async_trait]
#[auto_impl::auto_impl(Arc)]
pub trait GossipClientTrait: Send + Sync + Clone {
    async fn broadcast_block(&self, request: &SubmitBlockParams) -> Result<(), GossipError>;
}