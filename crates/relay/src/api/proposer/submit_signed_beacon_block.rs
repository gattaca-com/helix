use std::{
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use alloy_primitives::B256;
use axum::{Extension, http::HeaderMap};
use helix_common::{
    chain_info::ChainInfo, decoder::Encoding, spawn_tracked, utils::extract_request_id,
};
use helix_types::{
    BlsKeypair, BlsPublicKeyBytes, Domain, EthSpec, ForkName, GloasBidSubmission, MainnetEthSpec,
    Slot,
    gloas::{self, GloasBlock},
    ssz_json,
};
use hyper::StatusCode;
use tracing::{error, info, warn};

use super::{ProposerApi, get_payload::fork_name_from_header, reveal_guard::Registered};
use crate::api::{Api, proposer::error::ProposerApiError};

/// A payload a builder has already handed helix for a proposer's committed bid.
#[derive(Clone)]
pub struct HeldGloasPayload {
    pub submission: GloasBidSubmission,
}

const UNRESOLVED_BUILDER_INDEX: u64 = u64::MAX;

/// Helix's own on-chain Gloas builder identity: `builder_index` plus signing key.
// TODO(gloas): support external builder-signed bids/envelopes; see gattaca-com/helix#489 step 5.
pub struct GloasBuilderIdentity {
    builder_index: AtomicU64,
    pub keypair: BlsKeypair,
}

impl GloasBuilderIdentity {
    pub fn new(keypair: BlsKeypair) -> Self {
        Self { builder_index: AtomicU64::new(UNRESOLVED_BUILDER_INDEX), keypair }
    }

    pub fn builder_index(&self) -> Option<u64> {
        let index = self.builder_index.load(Ordering::Relaxed);
        (index != UNRESOLVED_BUILDER_INDEX).then_some(index)
    }

    pub fn set_builder_index(&self, index: Option<u64>) {
        self.builder_index.store(index.unwrap_or(UNRESOLVED_BUILDER_INDEX), Ordering::Relaxed);
    }

    /// Signs under `DOMAIN_BEACON_BUILDER`, not `ChainInfo::builder_domain`, per
    /// <https://github.com/ethereum/consensus-specs/blob/master/specs/gloas/builder.md#constructing-the-signedexecutionpayloadenvelope>.
    fn sign(
        &self,
        object_root: B256,
        slot: u64,
        message: &[u8],
        chain_info: &ChainInfo,
    ) -> Vec<u8> {
        let domain = domain_at(Domain::BeaconBuilder, slot, chain_info);
        let signature = self.keypair.sk.sign(gloas::signing_root(object_root, domain));
        gloas::signed(message, &signature.serialize())
    }

    pub fn sign_envelope(&self, envelope: &[u8], slot: u64, chain_info: &ChainInfo) -> Vec<u8> {
        self.sign(gloas::envelope_root(envelope), slot, envelope, chain_info)
    }

    pub fn sign_bid(
        &self,
        unsigned_bid: &[u8],
        chain_info: &ChainInfo,
    ) -> Result<Vec<u8>, ProposerApiError> {
        let mut bid = unsigned_bid.to_vec();
        gloas::set_bid_builder_index(
            &mut bid,
            self.builder_index().ok_or(ProposerApiError::NoBidPrepared)?,
        );
        Ok(self.sign(gloas::bid_root(&bid), gloas::bid_slot(&bid), &bid, chain_info))
    }
}

fn domain_at(domain: Domain, slot: u64, chain_info: &ChainInfo) -> B256 {
    let epoch = Slot::new(slot).epoch(MainnetEthSpec::slots_per_epoch());
    let fork = chain_info.spec.fork_at_epoch(epoch);
    chain_info.spec.get_domain(epoch, domain, &fork, chain_info.genesis_validators_root)
}

/// Whether the proposer helix bid to signed this block. The redeeming block is not
/// otherwise authenticated, and the bid's block hash is public once the block is
/// gossiped, so without this anyone could redeem a bid on the proposer's behalf.
fn proposer_signed_block(
    block: &GloasBlock<'_>,
    block_root: B256,
    pubkey: &BlsPublicKeyBytes,
    chain_info: &ChainInfo,
) -> bool {
    let domain = domain_at(Domain::BeaconProposer, block.slot(), chain_info);
    gloas::bls_verify(pubkey.as_slice(), block.signature(), gloas::signing_root(block_root, domain))
        .is_ok()
}

/// Constructs and signs the `SignedExecutionPayloadEnvelope` fulfilling `block`'s committed bid.
/// `held` is the payload the auctioneer has stored for the bid's committed block hash, if any.
pub(super) fn construct_signed_envelope(
    block: &GloasBlock<'_>,
    block_root: B256,
    held: Option<HeldGloasPayload>,
    identity: &GloasBuilderIdentity,
    chain_info: &ChainInfo,
) -> Result<Vec<u8>, ProposerApiError> {
    let bid_block_hash = block.bid_block_hash();
    let bid_builder_index = block.bid_builder_index();

    if identity.builder_index() != Some(bid_builder_index) {
        return Err(ProposerApiError::BuilderIndexMismatch {
            bid: bid_builder_index,
            ours: identity.builder_index(),
        });
    }

    let held = held.ok_or(ProposerApiError::NoHeldPayloadForBlock(bid_block_hash))?;
    let submission = held.submission;

    if !proposer_signed_block(
        block,
        block_root,
        &submission.bid_trace().proposer_pubkey,
        chain_info,
    ) {
        return Err(ProposerApiError::InvalidProposerSignature);
    }

    let held_block_hash = submission.bid_trace().block_hash;
    if held_block_hash != bid_block_hash {
        return Err(ProposerApiError::HeldPayloadBlockHashMismatch {
            held: held_block_hash,
            bid: bid_block_hash,
        });
    }

    let envelope = submission.envelope(bid_builder_index, block_root, block.parent_root());
    let signed_envelope = identity.sign_envelope(&envelope, block.slot(), chain_info);
    Ok(submission.envelope_contents(&signed_envelope))
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
    signed_envelope: bytes::Bytes,
    slot: u64,
) {
    for attempt in 0..PUBLISH_ATTEMPTS {
        match proposer_api
            .multi_beacon_client
            .publish_execution_payload_envelope(signed_envelope.clone(), ForkName::Gloas)
            .await
        {
            Ok(()) => {
                info!(slot, attempt, "revealed the payload");
                return;
            }
            Err(err) => {
                warn!(%err, attempt, "could not reveal the payload yet");
                tokio::time::sleep(PUBLISH_RETRY_INTERVAL).await;
            }
        }
    }

    error!(slot, "gave up revealing the payload after {PUBLISH_ATTEMPTS} attempts");
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

        let body = match Encoding::from_content_type(&headers) {
            Encoding::Json => ssz_json::to_ssz(&ssz_json::SIGNED_BEACON_BLOCK_GLOAS, &body)
                .map_err(|err| {
                    ProposerApiError::SszDecodeError(ssz::DecodeError::BytesInvalid(
                        err.to_string(),
                    ))
                })?
                .into(),
            Encoding::Ssz => body,
        };
        let block = GloasBlock::new(&body).ok_or(ProposerApiError::SszDecodeError(
            ssz::DecodeError::BytesInvalid("signed beacon block layout".to_string()),
        ))?;
        let slot = block.slot();

        info!(slot, "accepted submitSignedBeaconBlock request");

        let bid_block_hash = block.bid_block_hash();
        let Ok(rx) = proposer_api.auctioneer_handle.take_held_gloas_payload(bid_block_hash) else {
            return Err(ProposerApiError::InternalServerError);
        };
        let held = match rx.await {
            Ok(held) => held,
            Err(err) => {
                warn!(%err, "failed to fetch held Gloas payload from auctioneer");
                return Err(ProposerApiError::InternalServerError);
            }
        };

        let block_root = block.block_root();
        let signed_envelope: bytes::Bytes = construct_signed_envelope(
            &block,
            block_root,
            held,
            &proposer_api.gloas_builder_identity,
            &proposer_api.chain_info,
        )?
        .into();

        // Only now is the block known to redeem our bid: this endpoint does not verify
        // the proposer's signature, so an unvalidated block must never be able to
        // withhold a legitimate reveal. A second, different block that still redeems
        // the same bid is the proposer equivocating.
        let registered = proposer_api
            .reveal_guard
            .lock()
            .expect("reveal guard mutex")
            .register(Slot::new(slot), block_root);
        if let Registered::Equivocation { first } = registered {
            let withheld = proposer_api
                .reveal_guard
                .lock()
                .expect("reveal guard mutex")
                .withhold(Slot::new(slot));
            error!(slot, ?first, second = ?block_root, withheld, "the proposer equivocated",);
            return Err(ProposerApiError::ProposerEquivocated { slot, first });
        }

        // The proposer gossips its own block, but handing it to our node directly is
        // what makes the block root known by the time we reveal.
        if let Err(err) = proposer_api.multi_beacon_client.publish_gloas_block(body.clone()).await {
            warn!(%err, "could not publish the proposer's block");
        }

        // Revealing before the attestation deadline hands an equivocating proposer a
        // payload it can build a competing block on. Wait it out, then reveal, and let
        // the proposer have its acknowledgement now rather than seconds from now.
        let delay = proposer_api.chain_info.gloas_reveal_delay(Slot::new(slot), unix_now());
        info!(slot, delay_ms = delay.as_millis() as u64, "holding the payload");
        let guard = proposer_api.reveal_guard.clone();
        let reveal = spawn_tracked!(async move {
            tokio::time::sleep(delay).await;
            reveal_envelope(&proposer_api, signed_envelope, slot).await;
        });
        guard.lock().expect("reveal guard mutex").attach(Slot::new(slot), reveal.abort_handle());

        Ok(StatusCode::ACCEPTED)
    }
}

#[cfg(test)]
mod construct_signed_envelope_tests {
    use helix_common::utils::install_default_crypto_provider;
    use helix_types::{
        BeaconBlockGloas, BidTrace, EmptyBlock, ExecutionBlockHash, ExecutionPayloadGloas,
        GloasBidTrace, SignedBeaconBlockGloas, SignedBidSubmissionGloas,
        SignedExecutionPayloadEnvelope, SigningData, TestRandomSeed,
    };
    use ssz::{Decode, Encode};
    use tree_hash::TreeHash;

    use super::*;

    fn held_payload(block_hash: B256, proposer: &BlsKeypair) -> HeldGloasPayload {
        let mut execution_payload = ExecutionPayloadGloas::default();
        execution_payload.block_hash = ExecutionBlockHash(block_hash);
        let mut trace = BidTrace::test_random();
        trace.block_hash = block_hash;
        trace.proposer_pubkey = proposer.pk.compress().serialize().into();
        let submission = SignedBidSubmissionGloas {
            message: GloasBidTrace { trace, parent_block_root: B256::ZERO },
            execution_payload: Arc::new(execution_payload),
            blobs_bundle: Default::default(),
            execution_requests: Default::default(),
            signature: Default::default(),
        };
        HeldGloasPayload {
            submission: GloasBidSubmission::from_ssz_bytes(submission.as_ssz_bytes().into())
                .unwrap(),
        }
    }

    pub(super) fn proposer() -> BlsKeypair {
        install_default_crypto_provider();
        BlsKeypair::random()
    }

    /// Signs `message` the way the proposer's validator client would.
    pub(super) fn sign_block(message: BeaconBlockGloas, proposer: &BlsKeypair) -> Vec<u8> {
        let chain_info = ChainInfo::default();
        let epoch = message.slot.epoch(MainnetEthSpec::slots_per_epoch());
        let fork = chain_info.spec.fork_at_epoch(epoch);
        let domain = chain_info.spec.get_domain(
            epoch,
            Domain::BeaconProposer,
            &fork,
            chain_info.genesis_validators_root,
        );
        let signing_root =
            SigningData { object_root: message.tree_hash_root(), domain }.tree_hash_root();
        let signature = proposer.sk.sign(signing_root);
        SignedBeaconBlockGloas { message, signature }.as_ssz_bytes()
    }

    fn block_message(block_hash: B256, builder_index: u64, parent_root: B256) -> BeaconBlockGloas {
        let chain_info = ChainInfo::default();
        let mut message = BeaconBlockGloas::empty(&chain_info.spec);
        message.parent_root = parent_root;
        message.body.signed_execution_payload_bid.message.block_hash =
            ExecutionBlockHash(block_hash);
        message.body.signed_execution_payload_bid.message.builder_index = builder_index;
        message
    }

    fn test_block(
        block_hash: B256,
        builder_index: u64,
        parent_root: B256,
        proposer: &BlsKeypair,
    ) -> Vec<u8> {
        sign_block(block_message(block_hash, builder_index, parent_root), proposer)
    }

    fn identity(builder_index: u64) -> GloasBuilderIdentity {
        install_default_crypto_provider();
        let identity = GloasBuilderIdentity::new(BlsKeypair::random());
        identity.set_builder_index(Some(builder_index));
        identity
    }

    fn construct(
        block: &[u8],
        held: Option<HeldGloasPayload>,
        identity: &GloasBuilderIdentity,
    ) -> Result<SignedExecutionPayloadEnvelope, ProposerApiError> {
        let block = GloasBlock::new(block).expect("a well-formed block");
        let contents = construct_signed_envelope(
            &block,
            block.block_root(),
            held,
            identity,
            &ChainInfo::default(),
        )?;
        let proofs_offset = u32::from_le_bytes(contents[4..8].try_into().unwrap()) as usize;
        Ok(SignedExecutionPayloadEnvelope::from_ssz_bytes(&contents[12..proofs_offset])
            .expect("the envelope decodes as the consensus type"))
    }

    #[test]
    fn constructs_and_signs_envelope_matching_the_block_and_held_payload() {
        let block_hash = B256::repeat_byte(0x11);
        let parent_root = B256::repeat_byte(0x22);
        let proposer = proposer();
        let block = test_block(block_hash, 7, parent_root, &proposer);
        let held = Some(held_payload(block_hash, &proposer));

        let signed_envelope = construct(&block, held, &identity(7)).unwrap();

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
    fn signature_verifies_against_the_configured_identity() {
        let chain_info = ChainInfo::default();
        let block_hash = B256::repeat_byte(0x33);
        let proposer = proposer();
        let block = test_block(block_hash, 3, B256::ZERO, &proposer);
        let held = Some(held_payload(block_hash, &proposer));
        let identity = identity(3);

        let signed_envelope = construct(&block, held, &identity).unwrap();

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
        let block_hash = B256::repeat_byte(0x44);
        let proposer = proposer();
        let block = test_block(block_hash, 1, B256::ZERO, &proposer);

        let result = construct(&block, None, &identity(1));

        assert!(
            matches!(result, Err(ProposerApiError::NoHeldPayloadForBlock(hash)) if hash == block_hash)
        );
    }

    #[test]
    fn held_payload_block_hash_mismatch_is_rejected() {
        let bid_block_hash = B256::repeat_byte(0x55);
        let wrong_held_hash = B256::repeat_byte(0x66);
        let proposer = proposer();
        let block = test_block(bid_block_hash, 1, B256::ZERO, &proposer);
        let held = Some(held_payload(wrong_held_hash, &proposer));

        let result = construct(&block, held, &identity(1));

        assert!(matches!(
            result,
            Err(ProposerApiError::HeldPayloadBlockHashMismatch { held, bid })
                if held == wrong_held_hash && bid == bid_block_hash
        ));
    }

    /// The bid's block hash is public once the proposer gossips its block, so without
    /// this check anyone could redeem the bid and, with the equivocation guard, make
    /// helix withhold a legitimate reveal.
    #[test]
    fn a_block_signed_by_anyone_else_is_rejected() {
        let block_hash = B256::repeat_byte(0x33);
        let attacker = proposer();
        let proposer = proposer();
        // The attacker replays the real bid commitment under its own signature.
        let block = test_block(block_hash, 4, B256::ZERO, &attacker);
        let held = Some(held_payload(block_hash, &proposer));

        let result = construct(&block, held, &identity(4));

        assert!(matches!(result, Err(ProposerApiError::InvalidProposerSignature)));
    }

    #[test]
    fn an_unsigned_block_is_rejected() {
        let block_hash = B256::repeat_byte(0x44);
        let proposer = proposer();
        let block = SignedBeaconBlockGloas {
            message: block_message(block_hash, 4, B256::ZERO),
            signature: helix_types::BlsSignature::empty(),
        }
        .as_ssz_bytes();
        let held = Some(held_payload(block_hash, &proposer));

        let result = construct(&block, held, &identity(4));

        assert!(matches!(result, Err(ProposerApiError::InvalidProposerSignature)));
    }

    #[test]
    fn bid_builder_index_not_matching_configured_identity_is_rejected() {
        let block_hash = B256::repeat_byte(0x77);
        let proposer = proposer();
        let block = test_block(block_hash, 9, B256::ZERO, &proposer);
        let held = Some(held_payload(block_hash, &proposer));

        let result = construct(&block, held, &identity(1));

        assert!(matches!(
            result,
            Err(ProposerApiError::BuilderIndexMismatch { bid: 9, ours: Some(1) })
        ));
    }
}
