use helix_common::{api::proposer_api::GetExecutionPayloadBidParams, chain_info::ChainInfo};
use helix_types::{
    ExecutionBlockHash, ExecutionPayloadBid, SignedExecutionPayloadBid, Slot,
    convert_kzg_commitments_to_progressive, execution_requests_to_gloas,
};
use tokio::sync::oneshot;
use tracing::warn;
use tree_hash::TreeHash;

use crate::{
    api::proposer::{GloasBuilderIdentity, ProposerApiError},
    auctioneer::{
        bid_adjustor::BidAdjustor,
        context::Context,
        types::{GetExecutionPayloadBidResult, PayloadEntry, SlotData},
    },
};

impl<B: BidAdjustor> Context<B> {
    pub(super) fn handle_get_execution_payload_bid(
        &self,
        params: GetExecutionPayloadBidParams,
        slot_data: &SlotData,
        res_tx: oneshot::Sender<GetExecutionPayloadBidResult>,
    ) {
        let result = check_execution_payload_bid_liveness(&params, slot_data).and_then(|()| {
            let best_block_hash = self
                .bid_sorter
                .get_header(&params.parent_hash)
                .ok_or(ProposerApiError::NoBidPrepared)?;
            let entry =
                self.payloads.get(&best_block_hash).ok_or(ProposerApiError::NoBidPrepared)?;
            build_signed_bid(entry, &params, &self.gloas_builder_identity, &self.chain_info)
        });
        let _ = res_tx.send(result);
    }
}

/// Checks `params.parent_hash`/`params.parent_root` against currently-live payload attributes.
pub(super) fn check_execution_payload_bid_liveness(
    params: &GetExecutionPayloadBidParams,
    slot_data: &SlotData,
) -> Result<(), ProposerApiError> {
    let Some(attrs) = slot_data.payload_attributes_map.get(&params.parent_hash) else {
        warn!(
            req =% params.parent_hash,
            have =? slot_data.payload_attributes_map.keys(),
            "get execution payload bid for unknown parent hash"
        );
        return Err(ProposerApiError::NoBidPrepared);
    };

    if attrs.parent_beacon_block_root != Some(params.parent_root) {
        warn!(
            req =% params.parent_root,
            have =? attrs.parent_beacon_block_root,
            "get execution payload bid for mismatched parent root"
        );
        return Err(ProposerApiError::NoBidPrepared);
    }

    Ok(())
}

/// Builds and signs the `SignedExecutionPayloadBid` for the winning submission held in `entry`,
/// under helix's own configured Gloas builder identity.
pub(super) fn build_signed_bid(
    entry: &PayloadEntry,
    params: &GetExecutionPayloadBidParams,
    identity: &GloasBuilderIdentity,
    chain_info: &ChainInfo,
) -> Result<SignedExecutionPayloadBid, ProposerApiError> {
    let slot = Slot::new(params.slot);

    let payload = entry.execution_payload().to_lighthouse_gloas_payload(slot).map_err(|err| {
        warn!(%err, block_hash =% entry.block_hash(), "failed to convert held payload to Gloas shape for bid");
        ProposerApiError::InternalServerError
    })?;

    let execution_requests = execution_requests_to_gloas(entry.bid_data_ref().execution_requests);
    let execution_requests_root = execution_requests.tree_hash_root();

    // Per gattaca-com/helix#489: no payment-split product need yet, so execution_payment = value.
    let value = entry.value().saturating_to::<u64>();

    let bid = ExecutionPayloadBid {
        parent_block_hash: ExecutionBlockHash(params.parent_hash),
        parent_block_root: params.parent_root,
        block_hash: ExecutionBlockHash(*entry.block_hash()),
        prev_randao: payload.prev_randao,
        fee_recipient: payload.fee_recipient,
        gas_limit: payload.gas_limit,
        builder_index: identity.builder_index,
        slot,
        value,
        execution_payment: value,
        blob_kzg_commitments: convert_kzg_commitments_to_progressive(
            &entry.payload_and_blobs().blobs_bundle.commitments,
        ),
        execution_requests_root,
        _phantom: std::marker::PhantomData,
    };

    Ok(identity.sign_bid(bid, chain_info))
}

#[cfg(test)]
mod tests {
    use alloy_primitives::B256;
    use helix_common::PayloadAttributesUpdate;
    use helix_types::{Domain, EthSpec, ForkName, SignedRoot, TestRandomSeed};
    use rustc_hash::FxHashMap;

    use super::*;

    fn slot_data(payload_attributes_map: FxHashMap<B256, PayloadAttributesUpdate>) -> SlotData {
        SlotData {
            bid_slot: Default::default(),
            registration_data: Default::default(),
            current_fork: ForkName::Gloas,
            payload_attributes_map,
            il: Default::default(),
        }
    }

    fn attrs_update(
        parent_hash: B256,
        parent_beacon_block_root: Option<B256>,
    ) -> PayloadAttributesUpdate {
        let mut update = PayloadAttributesUpdate {
            slot: Default::default(),
            parent_hash,
            withdrawals_root: Default::default(),
            payload_attributes: Default::default(),
        };
        update.payload_attributes.parent_beacon_block_root = parent_beacon_block_root;
        update
    }

    fn params(parent_hash: B256, parent_root: B256) -> GetExecutionPayloadBidParams {
        GetExecutionPayloadBidParams {
            slot: 1,
            parent_hash,
            parent_root,
            proposer_pubkey: Default::default(),
        }
    }

    #[test]
    fn unknown_parent_hash_is_no_bid() {
        let parent_hash = B256::repeat_byte(0x11);
        let parent_root = B256::repeat_byte(0x22);
        let data = slot_data(FxHashMap::default());

        let result = check_execution_payload_bid_liveness(&params(parent_hash, parent_root), &data);

        assert!(matches!(result, Err(ProposerApiError::NoBidPrepared)));
    }

    #[test]
    fn parent_root_mismatch_is_no_bid() {
        let parent_hash = B256::repeat_byte(0x11);
        let live_root = B256::repeat_byte(0x22);
        let requested_root = B256::repeat_byte(0x33);
        let mut map = FxHashMap::default();
        map.insert(parent_hash, attrs_update(parent_hash, Some(live_root)));
        let data = slot_data(map);

        let result =
            check_execution_payload_bid_liveness(&params(parent_hash, requested_root), &data);

        assert!(matches!(result, Err(ProposerApiError::NoBidPrepared)));
    }

    #[test]
    fn missing_parent_beacon_block_root_is_no_bid() {
        let parent_hash = B256::repeat_byte(0x11);
        let requested_root = B256::repeat_byte(0x33);
        let mut map = FxHashMap::default();
        map.insert(parent_hash, attrs_update(parent_hash, None));
        let data = slot_data(map);

        let result =
            check_execution_payload_bid_liveness(&params(parent_hash, requested_root), &data);

        assert!(matches!(result, Err(ProposerApiError::NoBidPrepared)));
    }

    #[test]
    fn matching_parent_passes_liveness_check() {
        let parent_hash = B256::repeat_byte(0x11);
        let parent_root = B256::repeat_byte(0x22);
        let mut map = FxHashMap::default();
        map.insert(parent_hash, attrs_update(parent_hash, Some(parent_root)));
        let data = slot_data(map);

        let result = check_execution_payload_bid_liveness(&params(parent_hash, parent_root), &data);

        assert!(result.is_ok());
    }

    fn payload_entry(block_hash: B256, value: u64) -> PayloadEntry {
        use std::sync::Arc;

        use alloy_primitives::U256;
        use helix_types::{BlobsBundle, ExecutionPayload, ExecutionRequests, PayloadAndBlobs};

        let mut payload = ExecutionPayload::test_random();
        payload.block_hash = block_hash;

        PayloadEntry::new_gossip(
            PayloadAndBlobs {
                execution_payload: Arc::new(payload),
                blobs_bundle: Arc::new(BlobsBundle::default()),
            },
            helix_types::PayloadBidData {
                withdrawals_root: B256::ZERO,
                tx_root: None,
                execution_requests: Arc::new(ExecutionRequests::default()),
                value: U256::from(value),
                builder_pubkey: Default::default(),
            },
        )
    }

    fn bid_identity(builder_index: u64) -> GloasBuilderIdentity {
        helix_common::utils::install_default_crypto_provider();
        GloasBuilderIdentity { builder_index, keypair: helix_types::BlsKeypair::random() }
    }

    #[test]
    fn build_signed_bid_uses_the_entrys_data_and_configured_identity() {
        let chain_info = ChainInfo::default();
        let block_hash = B256::repeat_byte(0x99);
        let parent_hash = B256::repeat_byte(0x11);
        let parent_root = B256::repeat_byte(0x22);
        let entry = payload_entry(block_hash, 42);
        let identity = bid_identity(7);
        let params = params(parent_hash, parent_root);

        let signed_bid = build_signed_bid(&entry, &params, &identity, &chain_info).unwrap();

        assert_eq!(signed_bid.message.block_hash.0, block_hash);
        assert_eq!(signed_bid.message.parent_block_hash.0, parent_hash);
        assert_eq!(signed_bid.message.parent_block_root, parent_root);
        assert_eq!(signed_bid.message.builder_index, 7);
        assert_eq!(signed_bid.message.value, 42);
        assert_eq!(signed_bid.message.execution_payment, 42);

        let epoch = signed_bid.message.slot.epoch(helix_types::MainnetEthSpec::slots_per_epoch());
        let fork = chain_info.spec.fork_at_epoch(epoch);
        let domain = chain_info.spec.get_domain(
            epoch,
            Domain::BeaconBuilder,
            &fork,
            chain_info.genesis_validators_root,
        );
        assert!(
            signed_bid
                .signature
                .verify(&identity.keypair.pk, signed_bid.message.signing_root(domain))
        );
    }
}
