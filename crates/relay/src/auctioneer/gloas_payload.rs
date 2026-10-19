use std::{sync::Arc, time::Duration};

use alloy_primitives::B256;
use bytes::Bytes;
use helix_common::{beacon::MultiBeaconClient, chain_info::ChainInfo, spawn_tracked};
use helix_types::{ForkName, GloasBidSubmission, Slot};
use tracing::{error, info, warn};

use crate::auctioneer::{
    GloasBuilderIdentity, bid_adjustor::BidAdjustor, context::Context, seen_blocks::Recorded,
    types::SeenBlock,
};

const PUBLISH_ATTEMPTS: u32 = 12;
const PUBLISH_RETRY_INTERVAL: Duration = Duration::from_millis(100);

impl<B: BidAdjustor> Context<B> {
    pub(super) fn handle_gloas_block(&mut self, slot: Slot, seen: SeenBlock) {
        match self.seen_blocks.record(slot, seen) {
            Recorded::New => info!(%slot, ?seen, "seen a gloas block"),
            Recorded::Repeat => {}
            Recorded::Late => {
                warn!(%slot, ?seen, "gloas block arrived after the attestation deadline")
            }
        }
    }

    /// Reveals for `slot` only if exactly one block was seen, it names our bid, and it is
    /// our beacon node's head. Revealing earlier would hand an equivocating proposer a payload it
    /// can build a competing block on.
    pub(super) fn handle_attestation_deadline(&mut self, slot: Slot, head_root: B256) {
        let seen = self.seen_blocks.decide(slot);
        let ours = self.gloas_builder_identity.builder_index();
        let block = match seen.as_slice() {
            [] => return,
            [block] => *block,
            _ if seen.iter().any(|b| Some(b.bid_builder_index) == ours) => {
                error!(%slot, ?seen, "the proposer equivocated, withholding");
                return;
            }
            _ => {
                warn!(%slot, ?seen, "the proposer equivocated");
                return;
            }
        };
        let Some(builder_index) = ours else {
            return;
        };
        if block.bid_builder_index != builder_index {
            return;
        }
        if block.block_root != head_root {
            warn!(%slot, block_root = %block.block_root, %head_root, "block naming our bid is not our head, withholding");
            return;
        }
        let Some(entry) = self.gloas_payloads.get_for_reveal(&block.bid_block_hash) else {
            error!(%slot, bid_block_hash = %block.bid_block_hash, "block names our bid but we hold no payload");
            return;
        };

        let envelope = signed_envelope_contents(
            &entry.submission,
            builder_index,
            slot,
            block,
            &self.gloas_builder_identity,
            &self.chain_info,
        );
        spawn_tracked!(reveal_envelope(self.beacon_client.clone(), envelope.into(), slot));
    }
}

/// Builds and signs the `SignedExecutionPayloadEnvelope` fulfilling `block`'s bid.
fn signed_envelope_contents(
    submission: &GloasBidSubmission,
    builder_index: u64,
    slot: Slot,
    block: SeenBlock,
    identity: &GloasBuilderIdentity,
    chain_info: &ChainInfo,
) -> Vec<u8> {
    let envelope = submission.envelope(builder_index, block.block_root, block.parent_root);
    let signed_envelope = identity.sign_envelope(&envelope, slot.as_u64(), chain_info);
    submission.envelope_contents(&signed_envelope)
}

/// Offers the envelope to the beacon nodes until one takes it. A node rejects it
/// until it has imported the block it names.
async fn reveal_envelope(beacon_client: Arc<MultiBeaconClient>, envelope: Bytes, slot: Slot) {
    for attempt in 0..PUBLISH_ATTEMPTS {
        match beacon_client
            .publish_execution_payload_envelope(envelope.clone(), ForkName::Gloas)
            .await
        {
            Ok(()) => {
                info!(%slot, attempt, "revealed the payload");
                return;
            }
            Err(err) => {
                warn!(%err, attempt, "could not reveal the payload yet");
                tokio::time::sleep(PUBLISH_RETRY_INTERVAL).await;
            }
        }
    }

    error!(%slot, "gave up revealing the payload after {PUBLISH_ATTEMPTS} attempts");
}

#[cfg(test)]
mod tests {
    use helix_common::utils::install_default_crypto_provider;
    use helix_types::{
        BeaconBlockGloas, BidTrace, BlsKeypair, EmptyBlock, EthSpec, ExecutionBlockHash,
        ExecutionPayloadGloas, GloasBidTrace, MainnetEthSpec, SignedBeaconBlockGloas,
        SignedBidSubmissionGloas, SignedExecutionPayloadEnvelope, TestRandomSeed,
        gloas::GloasBlock,
    };
    use ssz::{Decode, Encode};
    use tree_hash::TreeHash;

    use super::*;

    fn submission(block_hash: B256) -> GloasBidSubmission {
        let mut execution_payload = ExecutionPayloadGloas::default();
        execution_payload.block_hash = ExecutionBlockHash(block_hash);
        let mut trace = BidTrace::test_random();
        trace.block_hash = block_hash;
        let submission = SignedBidSubmissionGloas {
            message: GloasBidTrace { trace, parent_block_root: B256::ZERO },
            execution_payload: Arc::new(execution_payload),
            blobs_bundle: Default::default(),
            execution_requests: Default::default(),
            signature: Default::default(),
        };
        GloasBidSubmission::from_ssz_bytes(submission.as_ssz_bytes().into()).unwrap()
    }

    fn block(block_hash: B256, builder_index: u64, parent_root: B256) -> Vec<u8> {
        let mut message = BeaconBlockGloas::empty(&ChainInfo::default().spec);
        message.parent_root = parent_root;
        message.body.signed_execution_payload_bid.message.block_hash =
            ExecutionBlockHash(block_hash);
        message.body.signed_execution_payload_bid.message.builder_index = builder_index;
        SignedBeaconBlockGloas { message, signature: helix_types::BlsSignature::empty() }
            .as_ssz_bytes()
    }

    fn identity() -> GloasBuilderIdentity {
        install_default_crypto_provider();
        GloasBuilderIdentity::new(BlsKeypair::random())
    }

    fn construct(block: &[u8], identity: &GloasBuilderIdentity) -> SignedExecutionPayloadEnvelope {
        let view = GloasBlock::new(block).expect("a well-formed block");
        let seen = SeenBlock::new(&view, view.block_root());
        let contents = signed_envelope_contents(
            &submission(seen.bid_block_hash),
            view.bid_builder_index(),
            Slot::new(view.slot()),
            seen,
            identity,
            &ChainInfo::default(),
        );
        let proofs_offset = u32::from_le_bytes(contents[4..8].try_into().unwrap()) as usize;
        SignedExecutionPayloadEnvelope::from_ssz_bytes(&contents[12..proofs_offset])
            .expect("the envelope decodes as the consensus type")
    }

    #[test]
    fn the_envelope_matches_the_block_and_payload() {
        let block_hash = B256::repeat_byte(0x11);
        let parent_root = B256::repeat_byte(0x22);
        let block = block(block_hash, 7, parent_root);

        let signed_envelope = construct(&block, &identity());

        let message = &signed_envelope.message;
        assert_eq!(message.builder_index, 7);
        assert_eq!(
            message.beacon_block_root,
            SignedBeaconBlockGloas::from_ssz_bytes(&block).unwrap().message.tree_hash_root()
        );
        assert_eq!(message.parent_beacon_block_root, parent_root);
        assert_eq!(message.payload.block_hash.0, block_hash);
    }

    #[test]
    fn the_envelope_signature_verifies_against_our_identity() {
        let chain_info = ChainInfo::default();
        let identity = identity();

        let signed_envelope = construct(&block(B256::repeat_byte(0x33), 3, B256::ZERO), &identity);

        let epoch = signed_envelope.message.slot().epoch(MainnetEthSpec::slots_per_epoch());
        let fork = chain_info.spec.fork_at_epoch(epoch);
        assert!(signed_envelope.verify_signature(
            &identity.keypair.pk,
            &fork,
            chain_info.genesis_validators_root,
            &chain_info.spec,
        ));
    }
}
