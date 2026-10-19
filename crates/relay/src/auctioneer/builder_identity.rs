use std::sync::atomic::{AtomicU64, Ordering};

use alloy_primitives::B256;
use helix_common::chain_info::ChainInfo;
use helix_types::{BlsKeypair, Domain, Slot, gloas};

use crate::api::proposer::ProposerApiError;

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
        let domain = chain_info.domain_at(Domain::BeaconBuilder, Slot::new(slot));
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
