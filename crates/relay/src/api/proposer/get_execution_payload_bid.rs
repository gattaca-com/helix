use std::sync::Arc;

use axum::{Extension, extract::Path, http::HeaderMap, response::IntoResponse};
use helix_common::{
    api::{
        HEADER_START_TIME_UNIX_MS, HEADER_TIMEOUT_MS, proposer_api::GetExecutionPayloadBidParams,
    },
    api_provider::header_u64,
    decoder::{Encoding, HEADER_SSZ},
    utils::extract_request_id,
};
use helix_types::{
    ForkName, GetExecutionPayloadBidResponse, SignedBuilderRequestAuth, SignedExecutionPayloadBid,
};
use http::{HeaderValue, header::CONTENT_TYPE};
use ssz::{Decode, Encode};
use tracing::{info, warn};

use super::{ProposerApi, get_payload::fork_name_from_header};
use crate::api::{
    Api,
    proposer::{CONSENSUS_VERSION_HEADER, error::ProposerApiError},
};

impl<A: Api> ProposerApi<A> {
    /// Serves a `SignedExecutionPayloadBid` for the given slot/parent_hash/parent_root to a
    /// Gloas (ePBS) proposer, per
    /// <https://github.com/ethereum/builder-specs/blob/main/specs/gloas/builder.md#per-request-validator-inputs>.
    #[tracing::instrument(skip_all, err(level = tracing::Level::TRACE), fields(id =% extract_request_id(&headers), slot = params.slot))]
    pub async fn get_execution_payload_bid(
        Extension(proposer_api): Extension<Arc<ProposerApi<A>>>,
        headers: HeaderMap,
        Path(params): Path<GetExecutionPayloadBidParams>,
        body: bytes::Bytes,
    ) -> Result<impl IntoResponse, ProposerApiError> {
        let fork = fork_name_from_header(&headers).ok().flatten();
        if fork != Some(ForkName::Gloas) {
            return Err(ProposerApiError::InvalidFork);
        }

        if header_u64(&headers, HEADER_START_TIME_UNIX_MS).is_none() ||
            header_u64(&headers, HEADER_TIMEOUT_MS).is_none()
        {
            return Err(ProposerApiError::MissingTimingHeaders);
        }

        let signed_request_auth: SignedBuilderRequestAuth =
            match Encoding::from_content_type(&headers) {
                Encoding::Json => serde_json::from_slice(&body)?,
                Encoding::Ssz => SignedBuilderRequestAuth::from_ssz_bytes(&body)?,
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

        let (auth_slot, auth_data_len, auth_data) = super::auth_summary(&signed_request_auth);
        info!(
            slot = params.slot,
            auth_slot,
            auth_data_len,
            %auth_data,
            parent_hash = ?params.parent_hash,
            parent_root = ?params.parent_root,
            proposer_pubkey = ?params.proposer_pubkey,
            "validated getExecutionPayloadBid request"
        );

        let Ok(rx) = proposer_api.auctioneer_handle.get_execution_payload_bid(params) else {
            return Err(ProposerApiError::InternalServerError);
        };

        let signed_bid = match rx.await {
            Ok(res) => res?,
            Err(err) => {
                warn!(%err, "failed to get execution payload bid from auctioneer");
                return Err(ProposerApiError::InternalServerError);
            }
        };

        match Encoding::from_accept(&headers) {
            Encoding::Json => {
                let versioned = GetExecutionPayloadBidResponse {
                    version: ForkName::Gloas,
                    metadata: Default::default(),
                    data: signed_bid,
                };
                Ok(axum::Json(serde_json::to_value(&versioned)?).into_response())
            }
            Encoding::Ssz => {
                let mut response = signed_bid.as_ssz_bytes().into_response();
                let headers = response.headers_mut();
                headers.insert(CONTENT_TYPE, HeaderValue::from_str(HEADER_SSZ).unwrap());
                headers.insert(
                    CONSENSUS_VERSION_HEADER,
                    HeaderValue::from_str(&ForkName::Gloas.to_string()).unwrap(),
                );
                Ok(response)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use helix_types::{BlsSignature, ExecutionPayloadBid};

    use super::*;

    #[test]
    fn json_bid_carries_the_version_key() {
        let versioned = GetExecutionPayloadBidResponse {
            version: ForkName::Gloas,
            metadata: Default::default(),
            data: SignedExecutionPayloadBid {
                message: ExecutionPayloadBid::default(),
                signature: BlsSignature::empty(),
            },
        };

        let json = serde_json::to_value(&versioned).unwrap();

        assert_eq!(json["version"], "gloas");
        assert!(json["data"]["message"].is_object());
    }
}
