use std::sync::Arc;

use axum::{Extension, extract::Path, http::HeaderMap};
use helix_common::{
    api::{
        HEADER_START_TIME_UNIX_MS, HEADER_TIMEOUT_MS, proposer_api::GetExecutionPayloadBidParams,
    },
    api_provider::header_u64,
    decoder::Encoding,
    utils::extract_request_id,
};
use helix_types::{ForkName, SignedRequestAuth};
use hyper::StatusCode;
use ssz::Decode;
use tracing::info;

use super::{ProposerApi, get_payload::fork_name_from_header};
use crate::api::{Api, proposer::error::ProposerApiError};

impl<A: Api> ProposerApi<A> {
    /// Serves a `SignedExecutionPayloadBid` for the given slot/parent_hash/parent_root to a
    /// Gloas (ePBS) proposer.
    ///
    /// Not yet wired to the auctioneer: this only validates the request per
    /// <https://github.com/ethereum/builder-specs/pull/165> and always answers with the
    /// spec's "no bid available" response.
    #[tracing::instrument(skip_all, err(level = tracing::Level::TRACE), fields(id =% extract_request_id(&headers), slot = params.slot))]
    pub async fn get_execution_payload_bid(
        Extension(proposer_api): Extension<Arc<ProposerApi<A>>>,
        headers: HeaderMap,
        Path(params): Path<GetExecutionPayloadBidParams>,
        body: bytes::Bytes,
    ) -> Result<StatusCode, ProposerApiError> {
        let fork = fork_name_from_header(&headers).ok().flatten();
        if fork != Some(ForkName::Gloas) {
            return Err(ProposerApiError::InvalidFork);
        }

        if header_u64(&headers, HEADER_START_TIME_UNIX_MS).is_none() ||
            header_u64(&headers, HEADER_TIMEOUT_MS).is_none()
        {
            return Err(ProposerApiError::MissingTimingHeaders);
        }

        let signed_request_auth: SignedRequestAuth = match Encoding::from_content_type(&headers) {
            Encoding::Json => serde_json::from_slice(&body)?,
            Encoding::Ssz => SignedRequestAuth::from_ssz_bytes(&body)?,
        };

        if signed_request_auth.message.slot != params.slot {
            return Err(ProposerApiError::RequestAuthSlotMismatch {
                auth_slot: signed_request_auth.message.slot,
                request_slot: params.slot,
            });
        }

        signed_request_auth
            .verify_signature(&params.proposer_pubkey, proposer_api.chain_info.request_auth_domain)
            .map_err(|_| ProposerApiError::InvalidRequestAuthSignature)?;

        info!(
            slot = params.slot,
            parent_hash = ?params.parent_hash,
            parent_root = ?params.parent_root,
            proposer_pubkey = ?params.proposer_pubkey,
            "validated getExecutionPayloadBid request (not yet wired to the auctioneer)"
        );

        // TODO(gloas): fetch/build the SignedExecutionPayloadBid from the auctioneer, honoring
        // any stored max_execution_payment preference. Not wired in yet -- always reports "no
        // bid available", which is a valid response per spec.
        Ok(StatusCode::NO_CONTENT)
    }
}
