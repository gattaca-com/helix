use helix_common::{ForkKey, api::proposer_api::GetExecutionPayloadBidParams};
use helix_types::{
    ExecutionBlockHash, ExecutionPayloadBid, Slot, convert_kzg_commitments_to_progressive,
    execution_requests_to_gloas,
};
use tokio::sync::oneshot;
use tracing::warn;
use tree_hash::TreeHash;

const WEI_PER_GWEI: u64 = 1_000_000_000;

use crate::{
    api::proposer::{GloasBuilderIdentity, ProposerApiError},
    auctioneer::{
        bid_adjustor::BidAdjustor,
        context::Context,
        types::{BidKey, GetExecutionPayloadBidResult, PayloadEntry, SlotData},
    },
};

impl<B: BidAdjustor> Context<B> {
    pub(super) fn handle_get_execution_payload_bid(
        &self,
        params: GetExecutionPayloadBidParams,
        last_sent: Option<BidKey>,
        slot_data: &SlotData,
        res_tx: oneshot::Sender<GetExecutionPayloadBidResult>,
    ) {
        let result = check_execution_payload_bid_liveness(&params, slot_data).and_then(|()| {
            let fork = ForkKey { parent_hash: params.parent_hash, parent_root: params.parent_root };
            let best_block_hash =
                self.bid_sorter.get_header(&fork).ok_or(ProposerApiError::NoBidPrepared)?;
            let entry =
                self.payloads.get(&best_block_hash).ok_or(ProposerApiError::NoBidPrepared)?;
            build_bid(entry, last_sent, &params, &self.gloas_builder_identity, slot_data)
        });
        let _ = res_tx.send(result);
    }
}

/// Checks `params.parent_hash`/`params.parent_root` against currently-live payload attributes.
pub(super) fn check_execution_payload_bid_liveness(
    params: &GetExecutionPayloadBidParams,
    slot_data: &SlotData,
) -> Result<(), ProposerApiError> {
    if params.slot != slot_data.bid_slot.as_u64() {
        return Err(ProposerApiError::BidRequestSlotMismatch {
            expected: slot_data.bid_slot.as_u64(),
            actual: params.slot,
        });
    }

    let expected_proposer = *slot_data.proposer_pubkey();
    if params.proposer_pubkey != expected_proposer {
        return Err(ProposerApiError::UnexpectedProposerPubkey {
            expected: expected_proposer,
            actual: params.proposer_pubkey,
        });
    }

    let fork = ForkKey { parent_hash: params.parent_hash, parent_root: params.parent_root };
    if !slot_data.payload_attributes_map.contains_key(&fork) {
        warn!(
            req =? fork,
            have =? slot_data.payload_attributes_map.keys(),
            "get execution payload bid for unknown fork"
        );
        return Err(ProposerApiError::NoBidPrepared);
    }

    Ok(())
}

/// Builds the `ExecutionPayloadBid` for the winning submission held in `entry`, under helix's
/// own configured Gloas builder identity, unless it is the bid `last_sent` describes: a stream
/// asks every few ms, so an unchanged bid costs only this comparison.
pub(super) fn build_bid(
    entry: &PayloadEntry,
    last_sent: Option<BidKey>,
    params: &GetExecutionPayloadBidParams,
    identity: &GloasBuilderIdentity,
    slot_data: &SlotData,
) -> Result<ExecutionPayloadBid, ProposerApiError> {
    let execution_payment =
        (entry.value() / alloy_primitives::U256::from(WEI_PER_GWEI)).saturating_to::<u64>();

    if last_sent == Some(BidKey { block_hash: *entry.block_hash(), execution_payment }) {
        return Err(ProposerApiError::BidUnchanged);
    }

    let slot = Slot::new(params.slot);

    let gloas_data = entry.gloas_data();
    let payload = entry.execution_payload();

    let execution_requests = execution_requests_to_gloas(
        entry.bid_data_ref().execution_requests,
        &gloas_data.builder_deposits,
        &gloas_data.builder_exits,
    );
    let execution_requests_root = execution_requests.tree_hash_root();

    Ok(ExecutionPayloadBid {
        parent_block_hash: ExecutionBlockHash(params.parent_hash),
        parent_block_root: params.parent_root,
        block_hash: ExecutionBlockHash(*entry.block_hash()),
        prev_randao: payload.prev_randao,
        // The proposer's, not the payload's: the builder keeps the coinbase and pays
        // the proposer in-block, and consensus pays any enshrined amount to whatever
        // this names. Announcing the coinbase names the builder.
        fee_recipient: slot_data.registration_data.entry.registration.message.fee_recipient,
        gas_limit: payload.gas_limit,
        builder_index: identity.builder_index,
        slot,
        // The proposer is paid in-block, so the enshrined `value` stays 0.
        value: 0,
        execution_payment,
        blob_kzg_commitments: convert_kzg_commitments_to_progressive(
            &entry.payload_and_blobs().blobs_bundle.commitments,
        ),
        execution_requests_root,
        _phantom: std::marker::PhantomData,
    })
}

#[cfg(test)]
mod tests {
    use alloy_primitives::B256;
    use helix_common::PayloadAttributesUpdate;
    use helix_types::{BlsPublicKeyBytes, ForkName, TestRandomSeed};

    use super::*;

    const BID_SLOT: u64 = 1;

    fn slot_data(attrs: Vec<PayloadAttributesUpdate>) -> SlotData {
        SlotData {
            bid_slot: Slot::new(BID_SLOT),
            registration_data: Default::default(),
            current_fork: ForkName::Gloas,
            payload_attributes_map: attrs.into_iter().map(|a| (a.fork(), a)).collect(),
            il: Default::default(),
        }
    }

    fn live_slot_data(parent_hash: B256, parent_root: B256) -> SlotData {
        slot_data(vec![attrs_update(parent_hash, Some(parent_root))])
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
        let data = slot_data(vec![]);

        let result = check_execution_payload_bid_liveness(&params(parent_hash, parent_root), &data);

        assert!(matches!(result, Err(ProposerApiError::NoBidPrepared)));
    }

    #[test]
    fn parent_root_mismatch_is_no_bid() {
        let parent_hash = B256::repeat_byte(0x11);
        let live_root = B256::repeat_byte(0x22);
        let requested_root = B256::repeat_byte(0x33);
        let data = slot_data(vec![attrs_update(parent_hash, Some(live_root))]);

        let result =
            check_execution_payload_bid_liveness(&params(parent_hash, requested_root), &data);

        assert!(matches!(result, Err(ProposerApiError::NoBidPrepared)));
    }

    #[test]
    fn missing_parent_beacon_block_root_is_no_bid() {
        let parent_hash = B256::repeat_byte(0x11);
        let requested_root = B256::repeat_byte(0x33);
        let data = slot_data(vec![attrs_update(parent_hash, None)]);

        let result =
            check_execution_payload_bid_liveness(&params(parent_hash, requested_root), &data);

        assert!(matches!(result, Err(ProposerApiError::NoBidPrepared)));
    }

    #[test]
    fn matching_parent_passes_liveness_check() {
        let parent_hash = B256::repeat_byte(0x11);
        let parent_root = B256::repeat_byte(0x22);
        let data = slot_data(vec![attrs_update(parent_hash, Some(parent_root))]);

        let result = check_execution_payload_bid_liveness(&params(parent_hash, parent_root), &data);

        assert!(result.is_ok());
    }

    #[test]
    fn each_parent_root_on_a_shared_parent_hash_passes_liveness_check() {
        let parent_hash = B256::repeat_byte(0x11);
        let missed_root = B256::repeat_byte(0x22);
        let empty_root = B256::repeat_byte(0x33);
        let data = slot_data(vec![
            attrs_update(parent_hash, Some(missed_root)),
            attrs_update(parent_hash, Some(empty_root)),
        ]);

        for root in [missed_root, empty_root] {
            let result = check_execution_payload_bid_liveness(&params(parent_hash, root), &data);
            assert!(result.is_ok());
        }
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

    /// A submission entry carrying a block access list, as Gloas requires.
    fn gloas_submission_entry(block_access_list: Vec<u8>) -> PayloadEntry {
        use helix_types::{
            BlockAccessListBytes, GloasSubmissionData, SignedBidSubmission, TestRandomSeed,
        };

        let mut submission = SignedBidSubmission::test_random();
        submission.blobs_bundle = Default::default();

        PayloadEntry::new_submission(
            submission,
            B256::ZERO,
            None,
            None,
            Some(GloasSubmissionData {
                block_access_list: BlockAccessListBytes(block_access_list.into()),
                ..Default::default()
            }),
            helix_types::SubmissionVersion::new(0, None),
            Default::default(),
            None,
        )
    }

    #[test]
    fn the_stored_payload_keeps_the_block_access_list_for_the_bid() {
        let entry = gloas_submission_entry(vec![5u8; 96]);

        let bal = entry.gloas_data().block_access_list;

        assert_eq!(bal.to_vec(), vec![5u8; 96], "the bid's payload would be invalid without it");

        // And it reaches the converted Gloas payload.
        let payload = entry
            .execution_payload()
            .to_lighthouse_gloas_payload(Slot::new(1), &bal)
            .expect("conversion must succeed");
        assert_eq!(payload.block_access_list.len(), 96);
    }

    #[test]
    fn a_gossip_entry_has_no_block_access_list() {
        let entry = payload_entry(B256::repeat_byte(0x11), 1);

        assert!(
            entry.gloas_data().block_access_list.is_empty(),
            "a gossiped payload carries none, so Gloas cannot be served from one",
        );
    }

    #[test]
    fn build_bid_uses_the_entrys_data_and_configured_identity() {
        let block_hash = B256::repeat_byte(0x99);
        let parent_hash = B256::repeat_byte(0x11);
        let parent_root = B256::repeat_byte(0x22);
        let entry = payload_entry(block_hash, 42 * WEI_PER_GWEI);
        let identity = bid_identity(7);
        let params = params(parent_hash, parent_root);
        let data = live_slot_data(parent_hash, parent_root);

        let bid = build_bid(&entry, None, &params, &identity, &data).unwrap();

        assert_eq!(bid.block_hash.0, block_hash);
        assert_eq!(bid.prev_randao, entry.execution_payload().prev_randao);
        assert_eq!(bid.gas_limit, entry.execution_payload().gas_limit);
        assert_eq!(bid.parent_block_hash.0, parent_hash);
        assert_eq!(bid.parent_block_root, parent_root);
        assert_eq!(bid.builder_index, 7);
        assert_eq!(bid.value, 0, "the proposer is paid in-block");
        assert_eq!(bid.execution_payment, 42, "wei converts to gwei");
        assert_eq!(
            bid.fee_recipient, data.registration_data.entry.registration.message.fee_recipient,
            "consensus pays any enshrined amount here, so it must be the proposer's",
        );
    }

    fn build(
        entry: &PayloadEntry,
        last_sent: Option<BidKey>,
    ) -> Result<ExecutionPayloadBid, ProposerApiError> {
        let parent_hash = B256::repeat_byte(0x11);
        let parent_root = B256::repeat_byte(0x22);
        build_bid(
            entry,
            last_sent,
            &params(parent_hash, parent_root),
            &bid_identity(7),
            &live_slot_data(parent_hash, parent_root),
        )
    }

    #[test]
    fn the_bid_a_stream_last_sent_is_not_built_again() {
        let entry = payload_entry(B256::repeat_byte(0x99), 42 * WEI_PER_GWEI);
        let sent = build(&entry, None).unwrap();

        let result = build(&entry, Some(BidKey::of(&sent)));

        assert!(matches!(result, Err(ProposerApiError::BidUnchanged)));
    }

    #[test]
    fn a_new_block_paying_the_same_is_built() {
        let last_sent = BidKey { block_hash: B256::repeat_byte(0x99), execution_payment: 42 };
        let entry = payload_entry(B256::repeat_byte(0xaa), 42 * WEI_PER_GWEI);

        let bid = build(&entry, Some(last_sent)).unwrap();

        assert_eq!(bid.block_hash.0, B256::repeat_byte(0xaa));
    }

    #[test]
    fn the_same_block_paying_more_is_built() {
        let block_hash = B256::repeat_byte(0x99);
        let last_sent = BidKey { block_hash, execution_payment: 42 };
        let entry = payload_entry(block_hash, 43 * WEI_PER_GWEI);

        let bid = build(&entry, Some(last_sent)).unwrap();

        assert_eq!(bid.execution_payment, 43);
    }

    #[test]
    fn refuses_a_bid_request_for_another_slot() {
        let parent_hash = B256::repeat_byte(0x11);
        let parent_root = B256::repeat_byte(0x22);
        let data = live_slot_data(parent_hash, parent_root);
        let mut params = params(parent_hash, parent_root);
        params.slot = BID_SLOT + 1;

        let result = check_execution_payload_bid_liveness(&params, &data);

        assert!(
            matches!(
                result,
                Err(ProposerApiError::BidRequestSlotMismatch { expected, actual })
                    if expected == BID_SLOT && actual == BID_SLOT + 1
            ),
            "the relay must not sign a bid for a slot it is not bidding for",
        );
    }

    #[test]
    fn refuses_a_bid_request_from_another_proposer() {
        let parent_hash = B256::repeat_byte(0x11);
        let parent_root = B256::repeat_byte(0x22);
        let data = live_slot_data(parent_hash, parent_root);
        let mut params = params(parent_hash, parent_root);
        params.proposer_pubkey = BlsPublicKeyBytes::from([7u8; 48]);

        let result = check_execution_payload_bid_liveness(&params, &data);

        assert!(
            matches!(result, Err(ProposerApiError::UnexpectedProposerPubkey { .. })),
            "a valid request auth proves the key, not the right to this slot",
        );
    }

    #[test]
    fn accepts_the_slots_own_proposer() {
        let parent_hash = B256::repeat_byte(0x11);
        let parent_root = B256::repeat_byte(0x22);
        let data = live_slot_data(parent_hash, parent_root);

        let result = check_execution_payload_bid_liveness(&params(parent_hash, parent_root), &data);

        assert!(result.is_ok(), "the expected slot and proposer must pass");
    }
}
