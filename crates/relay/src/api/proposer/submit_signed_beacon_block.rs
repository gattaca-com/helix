use std::{sync::Arc, time::Duration};

use alloy_primitives::B256;
use axum::{Extension, http::HeaderMap};
use helix_common::{
    chain_info::ChainInfo, decoder::Encoding, spawn_tracked, utils::extract_request_id,
};
use helix_types::{
    BlsKeypair, Domain, EthSpec, ExecutionPayloadEnvelope, ExecutionPayloadGloas,
    ExecutionRequestsGloas, ForkName, MainnetEthSpec, SignedBeaconBlockGloas,
    SignedExecutionPayloadEnvelope, SignedRoot,
};
use hyper::StatusCode;
use ssz::Decode;
use tracing::{error, info, warn};
use tree_hash::TreeHash;

use super::{ProposerApi, get_payload::fork_name_from_header, reveal_guard::Registered};
use crate::api::{Api, proposer::error::ProposerApiError};

/// A payload a builder has already handed helix for a proposer's committed bid.
pub struct HeldGloasPayload {
    pub payload: ExecutionPayloadGloas,
    pub execution_requests: ExecutionRequestsGloas,
    /// Revealed with the envelope: the beacon node never saw this block, so it
    /// has no blobs cached to attach itself.
    pub blobs: helix_types::BlobsBundle,
}

/// Helix's own on-chain Gloas builder identity: `builder_index` plus signing key.
// TODO(gloas): support external builder-signed bids/envelopes; see gattaca-com/helix#489 step 5.
pub struct GloasBuilderIdentity {
    pub builder_index: u64,
    pub keypair: BlsKeypair,
}

impl GloasBuilderIdentity {
    /// Signs under `DOMAIN_BEACON_BUILDER`, not `ChainInfo::builder_domain`, per
    /// <https://github.com/ethereum/consensus-specs/blob/master/specs/gloas/builder.md#constructing-the-signedexecutionpayloadenvelope>.
    pub fn sign_envelope(
        &self,
        message: ExecutionPayloadEnvelope,
        chain_info: &ChainInfo,
    ) -> SignedExecutionPayloadEnvelope {
        let epoch = message.slot().epoch(MainnetEthSpec::slots_per_epoch());
        let fork = chain_info.spec.fork_at_epoch(epoch);
        let domain = chain_info.spec.get_domain(
            epoch,
            Domain::BeaconBuilder,
            &fork,
            chain_info.genesis_validators_root,
        );
        let signature = self.keypair.sk.sign(message.signing_root(domain));
        SignedExecutionPayloadEnvelope { message, signature }
    }

    /// Signs a `SignedExecutionPayloadBid` under the same domain as `sign_envelope`.
    pub fn sign_bid(
        &self,
        message: helix_types::ExecutionPayloadBid,
        chain_info: &ChainInfo,
    ) -> helix_types::SignedExecutionPayloadBid {
        let epoch = message.slot.epoch(MainnetEthSpec::slots_per_epoch());
        let fork = chain_info.spec.fork_at_epoch(epoch);
        let domain = chain_info.spec.get_domain(
            epoch,
            Domain::BeaconBuilder,
            &fork,
            chain_info.genesis_validators_root,
        );
        let signature = self.keypair.sk.sign(message.signing_root(domain));
        helix_types::SignedExecutionPayloadBid { message, signature }
    }
}

/// Constructs and signs the `SignedExecutionPayloadEnvelope` fulfilling `block`'s committed bid.
/// `held` is the payload the auctioneer has stored for the bid's committed block hash, if any.
pub(super) fn construct_signed_envelope(
    block: &SignedBeaconBlockGloas,
    held: Option<HeldGloasPayload>,
    identity: &GloasBuilderIdentity,
    chain_info: &ChainInfo,
) -> Result<helix_types::SignedExecutionPayloadEnvelopeContents, ProposerApiError> {
    let bid = &block.message.body.signed_execution_payload_bid.message;
    let bid_block_hash: B256 = bid.block_hash.0;

    if bid.builder_index != identity.builder_index {
        return Err(ProposerApiError::BuilderIndexMismatch {
            bid: bid.builder_index,
            configured: identity.builder_index,
        });
    }

    let held = held.ok_or(ProposerApiError::NoHeldPayloadForBlock(bid_block_hash))?;

    let held_block_hash: B256 = held.payload.block_hash.0;
    if held_block_hash != bid_block_hash {
        return Err(ProposerApiError::HeldPayloadBlockHashMismatch {
            held: held_block_hash,
            bid: bid_block_hash,
        });
    }

    let envelope = ExecutionPayloadEnvelope {
        payload: held.payload,
        execution_requests: held.execution_requests,
        builder_index: bid.builder_index,
        beacon_block_root: block.message.tree_hash_root(),
        parent_beacon_block_root: block.message.parent_root,
    };

    helix_types::envelope_with_blobs(identity.sign_envelope(envelope, chain_info), &held.blobs)
        .map_err(|err| {
            warn!(%err, "could not attach the blobs to the envelope");
            ProposerApiError::InternalServerError
        })
}

const PUBLISH_ATTEMPTS: u32 = 12;
const PUBLISH_RETRY_INTERVAL: Duration = Duration::from_millis(100);

fn unix_now() -> Duration {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("the clock is after the unix epoch")
}

/// Offers the envelope to the beacon nodes until one takes it. The block reaches a
/// node over gossip a few hundred milliseconds after the proposer hands it to us.
async fn reveal_envelope<A: Api>(
    proposer_api: &ProposerApi<A>,
    signed_envelope: Arc<helix_types::SignedExecutionPayloadEnvelopeContents>,
    slot: helix_types::Slot,
) {
    for attempt in 0..PUBLISH_ATTEMPTS {
        match proposer_api
            .multi_beacon_client
            .publish_execution_payload_envelope(signed_envelope.clone(), ForkName::Gloas)
            .await
        {
            Ok(()) => {
                info!(slot = slot.as_u64(), attempt, "revealed the payload");
                return;
            }
            Err(err) => {
                warn!(%err, attempt, "could not reveal the payload yet");
                tokio::time::sleep(PUBLISH_RETRY_INTERVAL).await;
            }
        }
    }

    error!(slot = slot.as_u64(), "gave up revealing the payload after {PUBLISH_ATTEMPTS} attempts");
}

impl<A: Api> ProposerApi<A> {
    /// Accepts a Gloas `SignedBeaconBlock`. Replaces `submitBlindedBlock`/`getPayload`; per
    /// <https://github.com/ethereum/builder-specs/blob/main/specs/gloas/validator.md#block-proposal>,
    /// Gloas has no blinded-block variant.
    #[tracing::instrument(skip_all, err(level = tracing::Level::TRACE), fields(id =% extract_request_id(&headers)))]
    pub async fn submit_signed_beacon_block(
        Extension(proposer_api): Extension<Arc<ProposerApi<A>>>,
        headers: HeaderMap,
        body: bytes::Bytes,
    ) -> Result<StatusCode, ProposerApiError> {
        let fork = fork_name_from_header(&headers).ok().flatten();
        if fork != Some(ForkName::Gloas) {
            return Err(ProposerApiError::InvalidFork);
        }

        let block: SignedBeaconBlockGloas = match Encoding::from_content_type(&headers) {
            Encoding::Json => serde_json::from_slice(&body)?,
            Encoding::Ssz => SignedBeaconBlockGloas::from_ssz_bytes(&body)?,
        };

        info!(slot = block.message.slot.as_u64(), "accepted submitSignedBeaconBlock request");

        let bid_block_hash: B256 =
            block.message.body.signed_execution_payload_bid.message.block_hash.0;
        let Ok(rx) = proposer_api
            .auctioneer_handle
            .take_held_gloas_payload(bid_block_hash, block.message.slot)
        else {
            return Err(ProposerApiError::InternalServerError);
        };
        let held = match rx.await {
            Ok(held) => held,
            Err(err) => {
                warn!(%err, "failed to fetch held Gloas payload from auctioneer");
                return Err(ProposerApiError::InternalServerError);
            }
        };

        let signed_envelope = Arc::new(construct_signed_envelope(
            &block,
            held,
            &proposer_api.gloas_builder_identity,
            &proposer_api.chain_info,
        )?);

        // Only now is the block known to redeem our bid: this endpoint does not verify
        // the proposer's signature, so an unvalidated block must never be able to
        // withhold a legitimate reveal. A second, different block that still redeems
        // the same bid is the proposer equivocating.
        let slot = block.message.slot;
        let block_root = block.message.tree_hash_root();
        let registered = proposer_api
            .reveal_guard
            .lock()
            .expect("reveal guard mutex")
            .register(slot, block_root);
        if let Registered::Equivocation { first } = registered {
            let withheld =
                proposer_api.reveal_guard.lock().expect("reveal guard mutex").withhold(slot);
            error!(
                slot = slot.as_u64(),
                ?first,
                second = ?block_root,
                withheld,
                "the proposer equivocated",
            );
            return Err(ProposerApiError::ProposerEquivocated { slot: slot.as_u64(), first });
        }

        let block = Arc::new(block);

        // The proposer gossips its own block, but handing it to our node directly is
        // what makes the block root known by the time we reveal.
        if let Err(err) = proposer_api.multi_beacon_client.publish_gloas_block(block.clone()).await
        {
            warn!(%err, "could not publish the proposer's block");
        }

        // Revealing before the attestation deadline hands an equivocating proposer a
        // payload it can build a competing block on. Wait it out, then reveal, and let
        // the proposer have its acknowledgement now rather than seconds from now.
        let delay = proposer_api.chain_info.gloas_reveal_delay(slot, unix_now());
        info!(slot = slot.as_u64(), delay_ms = delay.as_millis() as u64, "holding the payload");
        let guard = proposer_api.reveal_guard.clone();
        let reveal = spawn_tracked!(async move {
            tokio::time::sleep(delay).await;
            reveal_envelope(&proposer_api, signed_envelope, slot).await;
        });
        guard.lock().expect("reveal guard mutex").attach(slot, reveal.abort_handle());

        Ok(StatusCode::ACCEPTED)
    }
}

#[cfg(test)]
mod construct_signed_envelope_tests {
    use helix_common::utils::install_default_crypto_provider;
    use helix_types::{BeaconBlockGloas, BlsSignature, EmptyBlock, ExecutionBlockHash, TestRandom};
    use rand::SeedableRng;

    use super::*;

    fn held_payload(block_hash: B256) -> HeldGloasPayload {
        let mut payload = ExecutionPayloadGloas::default();
        payload.block_hash = ExecutionBlockHash(block_hash);
        HeldGloasPayload {
            payload,
            execution_requests: ExecutionRequestsGloas::default(),
            blobs: Default::default(),
        }
    }

    fn test_block(
        block_hash: B256,
        builder_index: u64,
        parent_root: B256,
    ) -> SignedBeaconBlockGloas {
        let chain_info = ChainInfo::default();
        let mut message = BeaconBlockGloas::empty(&chain_info.spec);
        message.parent_root = parent_root;
        message.body.signed_execution_payload_bid.message.block_hash =
            ExecutionBlockHash(block_hash);
        message.body.signed_execution_payload_bid.message.builder_index = builder_index;
        SignedBeaconBlockGloas { message, signature: BlsSignature::empty() }
    }

    fn identity(builder_index: u64) -> GloasBuilderIdentity {
        install_default_crypto_provider();
        GloasBuilderIdentity { builder_index, keypair: BlsKeypair::random() }
    }

    #[test]
    fn constructs_and_signs_envelope_matching_the_block_and_held_payload() {
        let chain_info = ChainInfo::default();
        let block_hash = B256::repeat_byte(0x11);
        let parent_root = B256::repeat_byte(0x22);
        let block = test_block(block_hash, 7, parent_root);
        let held = Some(held_payload(block_hash));
        let identity = identity(7);

        let contents = construct_signed_envelope(&block, held, &identity, &chain_info).unwrap();
        let signed_envelope = contents.signed_execution_payload_envelope;

        assert_eq!(signed_envelope.message.builder_index, 7);
        assert_eq!(signed_envelope.message.beacon_block_root, block.message.tree_hash_root());
        assert_eq!(signed_envelope.message.parent_beacon_block_root, parent_root);
        assert_eq!(signed_envelope.message.payload.block_hash.0, block_hash);
    }

    #[test]
    fn signature_verifies_against_the_configured_identity() {
        let chain_info = ChainInfo::default();
        let block_hash = B256::repeat_byte(0x33);
        let block = test_block(block_hash, 3, B256::ZERO);
        let held = Some(held_payload(block_hash));
        let identity = identity(3);

        let signed_envelope = construct_signed_envelope(&block, held, &identity, &chain_info)
            .unwrap()
            .signed_execution_payload_envelope;

        let epoch = signed_envelope.message.slot().epoch(MainnetEthSpec::slots_per_epoch());
        let fork = chain_info.spec.fork_at_epoch(epoch);
        assert!(signed_envelope.verify_signature(
            &identity.keypair.pk,
            &fork,
            chain_info.genesis_validators_root,
            &chain_info.spec,
        ));
    }

    #[test]
    fn no_held_payload_is_an_error_not_a_panic() {
        let chain_info = ChainInfo::default();
        let block_hash = B256::repeat_byte(0x44);
        let block = test_block(block_hash, 1, B256::ZERO);
        let identity = identity(1);

        let result = construct_signed_envelope(&block, None, &identity, &chain_info);

        assert!(
            matches!(result, Err(ProposerApiError::NoHeldPayloadForBlock(hash)) if hash == block_hash)
        );
    }

    #[test]
    fn held_payload_block_hash_mismatch_is_rejected() {
        let chain_info = ChainInfo::default();
        let bid_block_hash = B256::repeat_byte(0x55);
        let wrong_held_hash = B256::repeat_byte(0x66);
        let block = test_block(bid_block_hash, 1, B256::ZERO);
        let held = Some(held_payload(wrong_held_hash));
        let identity = identity(1);

        let result = construct_signed_envelope(&block, held, &identity, &chain_info);

        assert!(matches!(
            result,
            Err(ProposerApiError::HeldPayloadBlockHashMismatch { held, bid })
                if held == wrong_held_hash && bid == bid_block_hash
        ));
    }

    #[test]
    fn bid_builder_index_not_matching_configured_identity_is_rejected() {
        let chain_info = ChainInfo::default();
        let block_hash = B256::repeat_byte(0x77);
        let block = test_block(block_hash, 9, B256::ZERO);
        let held = Some(held_payload(block_hash));
        let identity = identity(1);

        let result = construct_signed_envelope(&block, held, &identity, &chain_info);

        assert!(matches!(
            result,
            Err(ProposerApiError::BuilderIndexMismatch { bid: 9, configured: 1 })
        ));
    }
}

#[cfg(test)]
mod envelope_blob_tests {
    use helix_common::utils::install_default_crypto_provider;
    use helix_types::{BeaconBlockGloas, BlsSignature, EmptyBlock, ExecutionBlockHash, TestRandom};
    use rand::SeedableRng;

    use super::*;

    /// Seeded so the bundle is never empty: `random_for_test` picks a length in 0..=6.
    fn blobs_bundle() -> helix_types::BlobsBundle {
        let mut rng = rand::rngs::StdRng::seed_from_u64(7);
        helix_types::BlobsBundle::random_for_test(&mut rng)
    }

    /// The beacon node never saw this block, so it has nothing cached to attach and
    /// refuses a bare envelope that commits to blobs. The reveal has to carry them.
    #[test]
    fn the_reveal_carries_the_blocks_blobs() {
        install_default_crypto_provider();
        let chain_info = ChainInfo::default();
        let block_hash = B256::repeat_byte(0xaa);
        let blobs = blobs_bundle();
        let expected_blobs = blobs.blobs.len();
        let expected_proofs = blobs.proofs.len();
        assert!(expected_blobs > 0, "the fixture must have blobs to be worth testing");

        let mut payload = ExecutionPayloadGloas::default();
        payload.block_hash = ExecutionBlockHash(block_hash);
        let held = Some(HeldGloasPayload {
            payload,
            execution_requests: ExecutionRequestsGloas::default(),
            blobs,
        });

        let mut message = BeaconBlockGloas::empty(&chain_info.spec);
        message.body.signed_execution_payload_bid.message.block_hash =
            ExecutionBlockHash(block_hash);
        message.body.signed_execution_payload_bid.message.builder_index = 3;
        let block = SignedBeaconBlockGloas { message, signature: BlsSignature::empty() };

        let contents =
            construct_signed_envelope(&block, held, &identity_for(3), &chain_info).unwrap();

        assert_eq!(contents.blobs.len(), expected_blobs, "every blob must be revealed");
        assert_eq!(contents.kzg_proofs.len(), expected_proofs, "and every proof with them");
    }

    fn identity_for(builder_index: u64) -> GloasBuilderIdentity {
        GloasBuilderIdentity { builder_index, keypair: helix_types::BlsKeypair::random() }
    }
}
