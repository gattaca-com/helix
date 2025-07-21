pub mod blob_sidecars;

use alloy_primitives::B256;
use helix_types::{
    mock_public_key_bytes, BlsPublicKey, BuilderBid, BuilderBidElectra,
    ExecutionPayloadHeaderElectra, ForkName, SignedBidSubmission, SignedBuilderBid,
    SignedBuilderBidInner, Slot,
};

use crate::{
    bid_submission::v2::header_submission::SignedHeaderSubmission, signing::RelaySigningContext,
};

#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct BidRequest {
    pub slot: Slot,
    pub parent_hash: B256,
    pub pubkey: BlsPublicKey,
}

/// Signs the builder bid with the relay key. This is necessary because the relay is the "builder"
/// from the proposer point of view
///
/// TODO!!: add some timings/metric here
pub fn resign_builder_bid(
    mut message: BuilderBid,
    signing_ctx: &RelaySigningContext,
    fork: ForkName,
) -> SignedBuilderBid {
    *message.pubkey_mut() = signing_ctx.pubkey().clone().into();
    let sig = signing_ctx.sign_builder_message(&message);
    SignedBuilderBid::new_no_metadata(Some(fork), SignedBuilderBidInner { message, signature: sig })
}

pub fn bid_submission_to_builder_bid_unsigned(submission: &SignedBidSubmission) -> BuilderBid {
    match submission {
        SignedBidSubmission::Electra(bid) => {
            let header: ExecutionPayloadHeaderElectra = (&bid.execution_payload).into();
            let blobs_bundle = &bid.blobs_bundle;
            let message = BuilderBidElectra {
                header,
                blob_kzg_commitments: blobs_bundle.commitments.clone(),
                value: bid.message.value,
                execution_requests: bid.execution_requests.clone(),
                pubkey: mock_public_key_bytes(), // this will replaced when signing the header
            }
            .into();

            BuilderBid::Electra(message)
        }
    }
}

pub fn header_submission_to_builder_bid_unsigned(
    submission: &SignedHeaderSubmission,
) -> BuilderBid {
    match submission {
        SignedHeaderSubmission::Electra(bid) => {
            let header = bid.message.execution_payload_header.clone();
            let message = BuilderBidElectra {
                header,
                blob_kzg_commitments: bid.message.commitments.clone(),
                value: bid.message.bid_trace.value,
                execution_requests: bid.message.execution_requests.clone(),
                pubkey: mock_public_key_bytes(), // this will replaced when signing the header
            };

            BuilderBid::Electra(message)
        }
    }
}
