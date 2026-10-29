use std::sync::Arc;

use axum::{Extension, extract::Path, http::HeaderMap};
use helix_common::{
    api::proposer_api::ProposerPubkeyParams, chain_info::ChainInfo, decoder::Encoding,
    utils::extract_request_id,
};
use helix_types::{BlsPublicKeyBytes, BuilderPreferencesRequest, Slot};
use hyper::StatusCode;
use ssz::Decode;
use tracing::{info, warn};

use super::ProposerApi;
use crate::api::{Api, proposer::error::ProposerApiError};

/// Admits a preference only for a Gloas slot whose scheduled proposer is `proposer_pubkey`.
/// The request is signed by the path pubkey, so without the schedule anyone could sign with a
/// fresh key and store entries for any (pubkey, slot); with it the store holds at most one
/// entry per slot of the duty lookahead. The fork comes from the relay's own spec, not the
/// client's `Eth-Consensus-Version` header.
fn check_scheduled_proposer(
    chain_info: &ChainInfo,
    scheduled: Option<BlsPublicKeyBytes>,
    head_slot: Slot,
    proposer_pubkey: &BlsPublicKeyBytes,
    slot: Slot,
) -> Result<(), ProposerApiError> {
    if !chain_info.fork_at_slot(slot).gloas_enabled() {
        return Err(ProposerApiError::InvalidFork);
    }
    match scheduled {
        Some(expected) if expected == *proposer_pubkey => Ok(()),
        Some(expected) => {
            Err(ProposerApiError::UnexpectedProposerPubkey { expected, actual: *proposer_pubkey })
        }
        None if slot <= head_slot => {
            Err(ProposerApiError::RequestForPastSlot { request_slot: slot, head_slot })
        }
        None => Err(ProposerApiError::SlotTooNew),
    }
}

impl<A: Api> ProposerApi<A> {
    /// Accepts a proposer's Gloas (ePBS) `BuilderPreferencesRequest`, per
    /// <https://github.com/ethereum/builder-specs/blob/main/specs/gloas/builder.md#builder-preferences>.
    #[tracing::instrument(skip_all, err(level = tracing::Level::TRACE), fields(id =% extract_request_id(&headers)))]
    pub async fn submit_builder_preferences(
        Extension(proposer_api): Extension<Arc<ProposerApi<A>>>,
        headers: HeaderMap,
        Path(params): Path<ProposerPubkeyParams>,
        body: bytes::Bytes,
    ) -> Result<StatusCode, ProposerApiError> {
        let request: BuilderPreferencesRequest = match Encoding::from_content_type(&headers) {
            Encoding::Json => serde_json::from_slice(&body)?,
            Encoding::Ssz => BuilderPreferencesRequest::from_ssz_bytes(&body)?,
        };

        let slot = Slot::new(request.auth.message.slot);
        check_scheduled_proposer(
            &proposer_api.chain_info,
            proposer_api.local_cache.proposer_pubkey(slot),
            proposer_api.curr_slot_info.head_slot(),
            &params.proposer_pubkey,
            slot,
        )?;

        request
            .auth
            .verify_signature(&params.proposer_pubkey, proposer_api.chain_info.request_auth_domain)
            .map_err(|_| ProposerApiError::InvalidRequestAuthSignature)?;

        let (auth_slot, auth_data_len, auth_data) = super::auth_summary(&request.auth);
        info!(
            proposer_pubkey = ?params.proposer_pubkey,
            slot = auth_slot,
            auth_data_len,
            %auth_data,
            max_execution_payment = request.preferences.max_execution_payment,
            "validated submitBuilderPreferences request"
        );

        let Ok(rx) = proposer_api.auctioneer_handle.submit_builder_preferences(
            params.proposer_pubkey,
            request.auth.message.slot,
            request.preferences.max_execution_payment,
        ) else {
            return Err(ProposerApiError::InternalServerError);
        };

        // TODO(gloas): honor max_execution_payment when getExecutionPayloadBid serves a real
        // bid; not consumed anywhere yet.
        match rx.await {
            Ok(res) => res.map(|()| StatusCode::ACCEPTED),
            Err(err) => {
                warn!(%err, "failed to store builder preferences");
                Err(ProposerApiError::InternalServerError)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use helix_types::Epoch;

    use super::*;

    const GLOAS_EPOCH: u64 = 10;

    fn gloas_chain_info() -> ChainInfo {
        let base = ChainInfo::default();
        let mut spec = base.spec;
        spec.gloas_fork_epoch = Some(Epoch::new(GLOAS_EPOCH));
        ChainInfo::new(spec, base.genesis_validators_root, base.genesis_time_in_secs)
    }

    fn check(scheduled: Option<u8>, caller: u8, slot: u64) -> Result<(), ProposerApiError> {
        let head_slot = Slot::new(GLOAS_EPOCH * 32 + 5);
        check_scheduled_proposer(
            &gloas_chain_info(),
            scheduled.map(BlsPublicKeyBytes::repeat_byte),
            head_slot,
            &BlsPublicKeyBytes::repeat_byte(caller),
            Slot::new(slot),
        )
    }

    #[test]
    fn only_the_scheduled_proposer_of_a_gloas_slot_is_admitted() {
        let slot = GLOAS_EPOCH * 32 + 10;
        assert!(check(Some(1), 1, slot).is_ok());
        assert!(matches!(
            check(Some(1), 2, slot),
            Err(ProposerApiError::UnexpectedProposerPubkey { .. })
        ));
        assert!(matches!(check(None, 1, slot), Err(ProposerApiError::SlotTooNew)));
        assert!(matches!(
            check(None, 1, GLOAS_EPOCH * 32 + 1),
            Err(ProposerApiError::RequestForPastSlot { .. })
        ));
    }

    #[test]
    fn a_pre_gloas_slot_is_refused_whatever_the_schedule() {
        let slot = GLOAS_EPOCH * 32 - 1;
        assert!(matches!(check(Some(1), 1, slot), Err(ProposerApiError::InvalidFork)));
    }
}
