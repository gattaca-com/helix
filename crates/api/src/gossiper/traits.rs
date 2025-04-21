use async_trait::async_trait;

use crate::gossiper::types::{
    BroadcastGetPayloadParams, BroadcastHeaderParams, BroadcastPayloadParams, RequestPayloadParams,
};

#[async_trait]
#[auto_impl::auto_impl(Arc)]
pub trait GossipClientTrait: Send + Sync + Clone {
    /// Broadcast a header. The header will be saved if it is the best header for the receiving
    /// relay. Only validated Headers are gossiped.
    async fn broadcast_header(&self, request: BroadcastHeaderParams);

    /// Broadcast a payload. This payload will always be saved to the receiving relay's Autcioneer.
    /// This is because, the local relay has saved the payload's header and may have served it for
    /// get_header. Only validated Payloads are gossiped.
    async fn broadcast_payload(&self, request: BroadcastPayloadParams);

    /// Broadcast a request for a payload. If the receiving relay has the payload, it will be able
    /// to broadcast the block to the network. This is fallback mechanism for when the relay that
    /// get_header was called on does not have the payload yet.
    async fn broadcast_get_payload(&self, request: BroadcastGetPayloadParams);

    async fn request_payload(&self, _request: RequestPayloadParams);
}
