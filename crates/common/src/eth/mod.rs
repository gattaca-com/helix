use std::time::Instant;

use helix_types::{BuilderBid, ForkName, SignedBuilderBid, SignedBuilderBidInner};
use tracing::debug;

use crate::{metrics::BID_SIGNING_LATENCY, signing::RelaySigningContext};

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
