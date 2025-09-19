use std::time::Instant;

use alloy_primitives::B256;
use helix_types::{
    mock_public_key_bytes, BuilderBid, ForkName, SignedBidSubmission, SignedBuilderBid,
    SignedBuilderBidInner,
};
use tracing::debug;

use crate::{
    bid_submission::v2::header_submission::SignedHeaderSubmission, metrics::BID_SIGNING_LATENCY,
    signing::RelaySigningContext,
};

/// Signs the builder bid with the relay key. This is necessary because the relay is the "builder"
/// from the proposer point of view
pub fn resign_builder_bid(
    mut message: BuilderBid,
    signing_ctx: &RelaySigningContext,
    fork: ForkName,
) -> SignedBuilderBid {
    let start = Instant::now();

    message.pubkey = *signing_ctx.pubkey();
    let sig = signing_ctx.sign_builder_message(&message).serialize().into();

    let bid = SignedBuilderBid {
        version: fork,
        metadata: Default::default(),
        data: SignedBuilderBidInner { message, signature: sig },
    };

    BID_SIGNING_LATENCY.observe(start.elapsed().as_micros() as f64);
    debug!("re-signing builder bid took {:?}", start.elapsed());

    bid
}

pub fn bid_submission_to_builder_bid_unsigned(
    submission: &SignedBidSubmission,
    withdrawals_root: B256,
) -> BuilderBid {
    match submission {
        SignedBidSubmission::Electra(bid) => {
            let header = bid.execution_payload.to_header(Some(withdrawals_root));
            let execution_requests = bid.execution_requests.clone();

            BuilderBid {
                header,
                blob_kzg_commitments: bid.blobs_bundle.commitments.clone(),
                value: bid.message.value,
                execution_requests,
                pubkey: mock_public_key_bytes(), // this will be replaced when signing the header
            }
        }
    }
}

pub fn header_submission_to_builder_bid_unsigned(
    submission: &SignedHeaderSubmission,
) -> BuilderBid {
    match submission {
        SignedHeaderSubmission::Electra(bid) => {
            let header = bid.message.execution_payload_header.clone();
            BuilderBid {
                header,
                blob_kzg_commitments: bid.message.commitments.clone(),
                value: bid.message.bid_trace.value,
                execution_requests: bid.message.execution_requests.clone(),
                pubkey: mock_public_key_bytes(), // this will replaced when signing the header
            }
        }
    }
}
