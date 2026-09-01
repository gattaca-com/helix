use std::sync::Arc;

use alloy_eips::eip7594::CELLS_PER_EXT_BLOB;
use helix_common::{
    api::{PATH_BUILDER_API, PATH_SUBMIT_BLOCK},
    signing::RelaySigningContext,
};
use helix_types::{
    BidTrace, BlobsBundle, KzgCommitments, SignedBidSubmission, payload_from_v3, requests_from_v4,
};
use ssz::Encode;
use thiserror::Error;

use crate::{
    building::{assemble::BuiltBlock, slot::SlotContext},
    engine::convert::{block_to_payload_v3, requests_to_v4},
};

#[derive(Debug, Error)]
pub enum SubmitError {
    #[error("blobs bundle: {0}")]
    Blobs(String),
    #[error("payload exceeds the consensus limits")]
    OversizedPayload,
    #[error("requests: {0}")]
    Requests(String),
    #[error("relay rejected the submission ({status}): {body}")]
    Rejected { status: u16, body: String },
    #[error("relay request failed: {0}")]
    Transport(String),
}

pub struct Submitter {
    http: reqwest::Client,
    url: String,
    api_key: String,
    signing: RelaySigningContext,
}

impl Submitter {
    pub fn new(relay_url: &str, api_key: String, signing: RelaySigningContext) -> Self {
        Self {
            http: reqwest::Client::new(),
            url: format!(
                "{}{PATH_BUILDER_API}{PATH_SUBMIT_BLOCK}",
                relay_url.trim_end_matches('/')
            ),
            api_key,
            signing,
        }
    }

    /// Builds the `BidTrace` and signs it under the builder domain.
    pub fn sign(
        &self,
        built: &BuiltBlock,
        slot: &SlotContext,
    ) -> Result<SignedBidSubmission, SubmitError> {
        let payload_v3 = block_to_payload_v3(&built.block);
        let payload = payload_from_v3(payload_v3).ok_or(SubmitError::OversizedPayload)?;

        let requests_v4 = requests_to_v4(&built.requests).map_err(SubmitError::Requests)?;
        let requests = requests_from_v4(requests_v4)
            .ok_or_else(|| SubmitError::Requests("exceeds the consensus limits".into()))?;

        let blobs = wire_blobs(&built.blobs_bundle)?;

        // Every field the relay cross-checks against the payload comes from the
        // payload itself, not from the build.
        let message = BidTrace {
            slot: slot.slot,
            parent_hash: payload.parent_hash,
            block_hash: payload.block_hash,
            builder_pubkey: *self.signing.pubkey(),
            proposer_pubkey: slot.proposer_pubkey,
            proposer_fee_recipient: slot.proposer_fee_recipient,
            gas_limit: payload.gas_limit,
            gas_used: payload.gas_used,
            value: built.value,
        };
        let signature = self.signing.sign_builder_message(&message);

        Ok(SignedBidSubmission {
            message,
            execution_payload: Arc::new(payload),
            blobs_bundle: Arc::new(blobs),
            execution_requests: Arc::new(requests),
            signature: signature.serialize().into(),
        })
    }

    pub async fn submit(&self, submission: &SignedBidSubmission) -> Result<(), SubmitError> {
        let response = self
            .http
            .post(&self.url)
            .header("content-type", "application/octet-stream")
            .header("x-api-key", &self.api_key)
            .body(submission.as_ssz_bytes())
            .send()
            .await
            .map_err(|e| SubmitError::Transport(e.to_string()))?;

        let status = response.status();
        if status.is_success() {
            return Ok(());
        }
        // The relay's message is how an operator learns the blocks are bad.
        let body = response.text().await.unwrap_or_default();
        Err(SubmitError::Rejected { status: status.as_u16(), body })
    }
}

/// Inverse of [`crate::engine::convert::eblobs`].
///
/// ethrex's `AddAssign` for `BlobsBundle` does not propagate `version`, so an
/// aggregated bundle always claims version 0. The proof count is the only
/// trustworthy signal that these are EIP-7594 cell proofs.
fn wire_blobs(bundle: &ethrex_common::types::BlobsBundle) -> Result<BlobsBundle, SubmitError> {
    let expected = bundle.blobs.len() * CELLS_PER_EXT_BLOB;
    if bundle.proofs.len() != expected {
        return Err(SubmitError::Blobs(format!(
            "expected {expected} cell proofs for {} blobs, got {}",
            bundle.blobs.len(),
            bundle.proofs.len()
        )));
    }
    if bundle.commitments.len() != bundle.blobs.len() {
        return Err(SubmitError::Blobs(format!(
            "expected {} commitments, got {}",
            bundle.blobs.len(),
            bundle.commitments.len()
        )));
    }

    // A blob is 128 KiB. Write into the allocation rather than returning one
    // by value, which would move it across the stack.
    let mut blobs = Vec::with_capacity(bundle.blobs.len());
    for blob in &bundle.blobs {
        let mut out = Box::new(alloy_consensus::Blob::ZERO);
        out.0.copy_from_slice(blob.as_slice());
        blobs.push(Arc::from(out));
    }

    // `helix_types::fields::Kzg*` are `Bytes48`, not lighthouse's same-named types.
    let commitments: Vec<alloy_consensus::Bytes48> =
        bundle.commitments.iter().map(|c| alloy_consensus::Bytes48::from(*c)).collect();
    Ok(BlobsBundle {
        commitments: KzgCommitments::new(commitments)
            .map_err(|e| SubmitError::Blobs(format!("too many commitments: {e:?}")))?,
        proofs: bundle.proofs.iter().map(|p| alloy_consensus::Bytes48::from(*p)).collect(),
        blobs,
    })
}

#[cfg(test)]
mod tests;
