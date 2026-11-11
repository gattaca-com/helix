use std::{net::SocketAddr, sync::Arc};

use axum::{
    Extension,
    extract::{ConnectInfo, Path},
    http::HeaderMap,
    response::IntoResponse,
};
use helix_common::{
    GetHeaderTrace, RequestTimings,
    api::{
        HEADER_START_TIME_UNIX_MS, HEADER_TIMEOUT_MS,
        proposer_api::{GetExecutionPayloadBidParams, GetHeaderParams},
    },
    api_provider::{ApiProvider, header_ip_addr, header_u64},
    decoder::{Encoding, HEADER_SSZ},
    utils::{extract_request_id, utcnow_ns},
};
use helix_types::{ForkName, SignedBuilderRequestAuth, Slot, ssz_json};
use http::{HeaderValue, header::CONTENT_TYPE};
use ssz::Decode;
use tracing::{info, warn};

use super::{ProposerApi, get_payload::fork_name_from_header};
use crate::{
    api::{
        Api,
        proposer::{CONSENSUS_VERSION_HEADER, error::ProposerApiError},
    },
    spine::messages::GetHeaderServed,
};

impl<A: Api> ProposerApi<A> {
    /// Serves a `SignedExecutionPayloadBid` for the given slot/parent_hash/parent_root to a
    /// Gloas (ePBS) proposer, per
    /// <https://github.com/ethereum/builder-specs/blob/main/specs/gloas/builder.md#per-request-validator-inputs>.
    #[tracing::instrument(skip_all, err(level = tracing::Level::TRACE), fields(id =% extract_request_id(&headers), slot = params.slot))]
    pub async fn get_execution_payload_bid(
        Extension(proposer_api): Extension<Arc<ProposerApi<A>>>,
        Extension(timings): Extension<RequestTimings>,
        ConnectInfo(peer): ConnectInfo<SocketAddr>,
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
        super::check_request_auth_data(
            &proposer_api.relay_config.request_auth_data,
            &signed_request_auth,
        )?;

        proposer_api
            .local_cache
            .proposer_key(Slot::new(params.slot), &params.proposer_pubkey)
            .and_then(|key| {
                signed_request_auth
                    .verify_signature(&key, proposer_api.chain_info.request_auth_domain)
            })
            .map_err(|_| ProposerApiError::InvalidRequestAuthSignature)?;
        let mut trace = GetHeaderTrace {
            receive: timings.on_receive_ns,
            validation_complete: utcnow_ns(),
            ..Default::default()
        };

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

        let bid = match rx.await {
            Ok(res) => res?,
            Err(err) => {
                warn!(%err, "failed to get execution payload bid from auctioneer");
                return Err(ProposerApiError::InternalServerError);
            }
        };
        trace.best_bid_fetched = utcnow_ns();

        let served = GetHeaderServed {
            slot: params.slot,
            receive_ns: trace.receive,
            best_bid_fetched_ns: trace.best_bid_fetched,
            block_hash: bid.trace.block_hash,
            value: bid.trace.value,
            builder_pubkey: bid.trace.builder_pubkey,
            proposer_ip: GetHeaderServed::ip_octets(header_ip_addr(&headers)),
            is_mev_boost: false,
            streamed: false,
            proposer_pubkey: params.proposer_pubkey,
            _pad: [0; 6],
        };
        proposer_api.api_provider.on_header_served(served, peer, &proposer_api.header_served);
        proposer_api.db.save_get_header_call(
            GetHeaderParams {
                slot: params.slot,
                parent_hash: params.parent_hash,
                pubkey: params.proposer_pubkey,
            },
            bid.trace.block_hash,
            bid.trace.value,
            trace,
            false,
            proposer_api.api_provider.get_metadata(&headers),
            bid.trace.builder_pubkey,
            bid.trace.proposer_fee_recipient,
            bid.block_number,
            bid.extra_data,
        );
        info!(block_hash =% bid.trace.block_hash, value =? bid.trace.value, "delivering bid");
        let signed_bid = bid.signed_bid;

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
