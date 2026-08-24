use std::sync::Arc;

use alloy_primitives::B256;
use axum::{Extension, http::HeaderMap};
use helix_common::{chain_info::ChainInfo, decoder::Encoding, utils::extract_request_id};
use helix_types::{
    BlsKeypair, Domain, EthSpec, ExecutionPayloadEnvelope, ExecutionPayloadGloas,
    ExecutionRequestsGloas, ForkName, MainnetEthSpec, SignedBeaconBlock, SignedBeaconBlockGloas,
    SignedExecutionPayloadEnvelope, SignedRoot,
};
use hyper::StatusCode;
use ssz::Decode;
use tracing::info;
use tree_hash::TreeHash;

use super::{ProposerApi, get_payload::fork_name_from_header};
use crate::api::{Api, proposer::error::ProposerApiError};

/// A payload a builder has already handed helix for a proposer's committed bid.
// TODO(gloas): wire into ProposerApi's shared state and call from the handler below.
#[allow(dead_code)]
pub struct HeldGloasPayload {
    pub payload: ExecutionPayloadGloas,
    pub execution_requests: ExecutionRequestsGloas,
}

/// Looks up and consumes the payload held for a bid's committed block hash. Must not return
/// the same payload twice.
// TODO(gloas): implement against the auctioneer; see gattaca-com/helix#489 step 3.
#[allow(dead_code)]
pub trait GloasPayloadStore: Send + Sync {
    fn take_held_payload(&self, block_hash: B256) -> Option<HeldGloasPayload>;
}

/// Helix's own on-chain Gloas builder identity: `builder_index` plus signing key.
// TODO(gloas): support external builder-signed bids/envelopes; see gattaca-com/helix#489 step 5.
#[allow(dead_code)]
pub struct GloasBuilderIdentity {
    pub builder_index: u64,
    pub keypair: BlsKeypair,
}

impl GloasBuilderIdentity {
    /// Signs under `DOMAIN_BEACON_BUILDER`, not `ChainInfo::builder_domain`, per
    /// <https://github.com/ethereum/consensus-specs/blob/master/specs/gloas/builder.md#constructing-the-signedexecutionpayloadenvelope>.
    #[allow(dead_code)]
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
}

/// Constructs and signs the `SignedExecutionPayloadEnvelope` fulfilling `block`'s committed bid.
#[allow(dead_code)]
pub(super) fn construct_signed_envelope(
    block: &SignedBeaconBlockGloas,
    store: &dyn GloasPayloadStore,
    identity: &GloasBuilderIdentity,
    chain_info: &ChainInfo,
) -> Result<SignedExecutionPayloadEnvelope, ProposerApiError> {
    let bid = &block.message.body.signed_execution_payload_bid.message;
    let bid_block_hash: B256 = bid.block_hash.0;

    if bid.builder_index != identity.builder_index {
        return Err(ProposerApiError::BuilderIndexMismatch {
            bid: bid.builder_index,
            configured: identity.builder_index,
        });
    }

    let held = store
        .take_held_payload(bid_block_hash)
        .ok_or(ProposerApiError::NoHeldPayloadForBlock(bid_block_hash))?;

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

    Ok(identity.sign_envelope(envelope, chain_info))
}

impl<A: Api> ProposerApi<A> {
    /// Accepts a Gloas `SignedBeaconBlock`. Replaces `submitBlindedBlock`/`getPayload`; per
    /// <https://github.com/ethereum/builder-specs/blob/main/specs/gloas/validator.md#block-proposal>,
    /// Gloas has no blinded-block variant.
    #[tracing::instrument(skip_all, err(level = tracing::Level::TRACE), fields(id =% extract_request_id(&headers)))]
    pub async fn submit_signed_beacon_block(
        Extension(_proposer_api): Extension<Arc<ProposerApi<A>>>,
        headers: HeaderMap,
        body: bytes::Bytes,
    ) -> Result<StatusCode, ProposerApiError> {
        let fork = fork_name_from_header(&headers).ok().flatten();
        if fork != Some(ForkName::Gloas) {
            return Err(ProposerApiError::InvalidFork);
        }

        let signed_block: SignedBeaconBlock = match Encoding::from_content_type(&headers) {
            Encoding::Json => {
                let block: SignedBeaconBlockGloas = serde_json::from_slice(&body)?;
                block.into()
            }
            Encoding::Ssz => {
                let block = SignedBeaconBlockGloas::from_ssz_bytes(&body)?;
                block.into()
            }
        };

        info!(
            slot = signed_block.slot().as_u64(),
            "accepted submitSignedBeaconBlock request (not yet wired to the auctioneer)"
        );

        // TODO(gloas): call construct_signed_envelope and broadcast via MultiBeaconClient.
        Ok(StatusCode::ACCEPTED)
    }
}

#[cfg(test)]
mod construct_signed_envelope_tests {
    use std::sync::Mutex;

    use helix_common::utils::install_default_crypto_provider;
    use helix_types::{BeaconBlockGloas, BlsSignature, EmptyBlock, ExecutionBlockHash};

    use super::*;

    struct StubStore(Mutex<Option<HeldGloasPayload>>);

    impl StubStore {
        fn holding(payload: HeldGloasPayload) -> Self {
            Self(Mutex::new(Some(payload)))
        }

        fn empty() -> Self {
            Self(Mutex::new(None))
        }
    }

    impl GloasPayloadStore for StubStore {
        fn take_held_payload(&self, _block_hash: B256) -> Option<HeldGloasPayload> {
            self.0.lock().unwrap().take()
        }
    }

    fn held_payload(block_hash: B256) -> HeldGloasPayload {
        let mut payload = ExecutionPayloadGloas::default();
        payload.block_hash = ExecutionBlockHash(block_hash);
        HeldGloasPayload { payload, execution_requests: ExecutionRequestsGloas::default() }
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
        let store = StubStore::holding(held_payload(block_hash));
        let identity = identity(7);

        let signed_envelope =
            construct_signed_envelope(&block, &store, &identity, &chain_info).unwrap();

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
        let store = StubStore::holding(held_payload(block_hash));
        let identity = identity(3);

        let signed_envelope =
            construct_signed_envelope(&block, &store, &identity, &chain_info).unwrap();

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
        let store = StubStore::empty();
        let identity = identity(1);

        let result = construct_signed_envelope(&block, &store, &identity, &chain_info);

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
        let store = StubStore::holding(held_payload(wrong_held_hash));
        let identity = identity(1);

        let result = construct_signed_envelope(&block, &store, &identity, &chain_info);

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
        let store = StubStore::holding(held_payload(block_hash));
        let identity = identity(1);

        let result = construct_signed_envelope(&block, &store, &identity, &chain_info);

        assert!(matches!(
            result,
            Err(ProposerApiError::BuilderIndexMismatch { bid: 9, configured: 1 })
        ));
    }
}
