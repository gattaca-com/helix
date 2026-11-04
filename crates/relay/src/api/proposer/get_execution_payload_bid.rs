use std::sync::Arc;

use alloy_primitives::B256;
use axum::{Extension, extract::Path, http::HeaderMap, response::IntoResponse};
use helix_common::{
    api::{
        HEADER_START_TIME_UNIX_MS, HEADER_TIMEOUT_MS, proposer_api::GetExecutionPayloadBidParams,
    },
    api_provider::header_u64,
    decoder::{Encoding, HEADER_SSZ},
    utils::extract_request_id,
};
use helix_types::{ForkName, SignedBuilderRequestAuth, ssz_json};
use http::{HeaderValue, header::CONTENT_TYPE};
use ssz::Decode;
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

        verify_request_auth(
            &signed_request_auth,
            &params,
            proposer_api.chain_info.request_auth_domain,
        )?;

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

        let Ok(rx) = proposer_api.auctioneer_handle.get_execution_payload_bid(params, None) else {
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
                let data = ssz_json::to_json(&ssz_json::SIGNED_EXECUTION_PAYLOAD_BID, &signed_bid)
                    .map_err(|err| {
                        warn!(%err, "could not render the signed bid as json");
                        ProposerApiError::InternalServerError
                    })?;
                Ok(axum::Json(serde_json::json!({ "version": ForkName::Gloas, "data": data }))
                    .into_response())
            }
            Encoding::Ssz => {
                let mut response = signed_bid.into_response();
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

/// The auth must be for the requested slot, signed by the requesting proposer.
pub(super) fn verify_request_auth(
    auth: &SignedBuilderRequestAuth,
    params: &GetExecutionPayloadBidParams,
    request_auth_domain: B256,
) -> Result<(), ProposerApiError> {
    if auth.message.slot != params.slot {
        return Err(ProposerApiError::RequestAuthSlotMismatch {
            auth_slot: auth.message.slot,
            request_slot: params.slot,
        });
    }

    auth.verify_signature(&params.proposer_pubkey, request_auth_domain)
        .map_err(|_| ProposerApiError::InvalidRequestAuthSignature)
}

#[cfg(test)]
mod tests {
    use helix_types::{BlsKeypair, BuilderRequestAuth, RequestAuthData};
    use tree_hash::TreeHash;

    use super::*;

    #[test]
    fn request_auth_for_another_slot_is_refused() {
        let proposer = BlsKeypair::random();
        let domain = B256::repeat_byte(7);
        let message = BuilderRequestAuth { data: RequestAuthData(vec![1, 2, 3].into()), slot: 4 };
        let signing_root = helix_types::gloas::signing_root(message.tree_hash_root(), domain);
        let signature = proposer.sk.sign(signing_root).serialize().into();
        let params = GetExecutionPayloadBidParams {
            slot: 5,
            parent_hash: B256::ZERO,
            parent_root: B256::ZERO,
            proposer_pubkey: proposer.pk.serialize().into(),
        };

        let result =
            verify_request_auth(&SignedBuilderRequestAuth { message, signature }, &params, domain);

        assert!(matches!(
            result,
            Err(ProposerApiError::RequestAuthSlotMismatch { auth_slot: 4, request_slot: 5 })
        ));
    }
}
