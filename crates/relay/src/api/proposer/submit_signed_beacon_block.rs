use std::sync::Arc;

use axum::{Extension, http::HeaderMap};
use helix_common::{decoder::Encoding, utils::extract_request_id};
use helix_types::{ForkName, SignedBeaconBlock, SignedBeaconBlockGloas};
use hyper::StatusCode;
use ssz::Decode;
use tracing::info;

use super::{ProposerApi, get_payload::fork_name_from_header};
use crate::api::{Api, proposer::error::ProposerApiError};

impl<A: Api> ProposerApi<A> {
    /// Accepts a Gloas (ePBS) `SignedBeaconBlock`, replacing `submitBlindedBlock`/`getPayload`:
    /// post-Gloas there is no blinded-block variant, and the payload is no longer returned
    /// synchronously -- the builder reveals it later via a `SignedExecutionPayloadEnvelope`
    /// broadcast to the PTC over gossip.
    ///
    /// Not yet wired in: this only decodes and accepts the block per
    /// <https://github.com/ethereum/builder-specs/pull/165>. No validation against a held
    /// bid, and no envelope construction/broadcast, happens yet.
    #[tracing::instrument(skip_all, err(level = tracing::Level::TRACE), fields(id =% extract_request_id(&headers)))]
    pub async fn submit_signed_beacon_block(
        Extension(_proposer_api): Extension<Arc<ProposerApi<A>>>,
        headers: HeaderMap,
        body: bytes::Bytes,
    ) -> Result<StatusCode, ProposerApiError> {
        let fork = fork_name_from_header(&headers).ok().flatten();
        if fork != Some(ForkName::Gloas) {
            return Err(ProposerApiError::InvalidFork);
        }

        let signed_block: SignedBeaconBlock = match Encoding::from_content_type(&headers) {
            Encoding::Json => {
                let block: SignedBeaconBlockGloas = serde_json::from_slice(&body)?;
                block.into()
            }
            Encoding::Ssz => {
                let block = SignedBeaconBlockGloas::from_ssz_bytes(&body)?;
                block.into()
            }
        };

        info!(
            slot = signed_block.slot().as_u64(),
            "accepted submitSignedBeaconBlock request (not yet wired to the auctioneer)"
        );

        // TODO(gloas): validate against a held SignedExecutionPayloadBid, then construct and
        // broadcast the SignedExecutionPayloadEnvelope to the PTC. Not wired in yet.
        Ok(StatusCode::ACCEPTED)
    }
}
