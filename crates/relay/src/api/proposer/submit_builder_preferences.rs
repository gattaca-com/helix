use std::sync::Arc;

use axum::{Extension, extract::Path, http::HeaderMap};
use helix_common::{
    api::proposer_api::ProposerPubkeyParams, decoder::Encoding, utils::extract_request_id,
};
use helix_types::{BuilderPreferencesRequest, ForkName};
use hyper::StatusCode;
use ssz::Decode;
use tracing::info;

use super::{ProposerApi, get_payload::fork_name_from_header};
use crate::api::{Api, proposer::error::ProposerApiError};

impl<A: Api> ProposerApi<A> {
    /// Accepts a proposer's Gloas (ePBS) `BuilderPreferencesRequest`.
    ///
    /// Not yet wired in: this only validates the request per
    /// <https://github.com/ethereum/builder-specs/pull/165>; preferences are not yet stored
    /// or enforced when serving bids.
    #[tracing::instrument(skip_all, err(level = tracing::Level::TRACE), fields(id =% extract_request_id(&headers)))]
    pub async fn submit_builder_preferences(
        Extension(proposer_api): Extension<Arc<ProposerApi<A>>>,
        headers: HeaderMap,
        Path(params): Path<ProposerPubkeyParams>,
        body: bytes::Bytes,
    ) -> Result<StatusCode, ProposerApiError> {
        let fork = fork_name_from_header(&headers).ok().flatten();
        if fork != Some(ForkName::Gloas) {
            return Err(ProposerApiError::InvalidFork);
        }

        let request: BuilderPreferencesRequest = match Encoding::from_content_type(&headers) {
            Encoding::Json => serde_json::from_slice(&body)?,
            Encoding::Ssz => BuilderPreferencesRequest::from_ssz_bytes(&body)?,
        };

        request
            .auth
            .verify_signature(&params.proposer_pubkey, proposer_api.chain_info.request_auth_domain)
            .map_err(|_| ProposerApiError::InvalidRequestAuthSignature)?;

        info!(
            proposer_pubkey = ?params.proposer_pubkey,
            slot = request.auth.message.slot,
            max_execution_payment = request.preferences.max_execution_payment,
            "validated submitBuilderPreferences request (not yet persisted -- storage not wired in)"
        );

        // TODO(gloas): reject stale slots, store preferences per proposer per slot, and honor
        // max_execution_payment when serving bids. Not wired in yet.
        Ok(StatusCode::ACCEPTED)
    }
}
