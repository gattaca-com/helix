use helix_common::{
    ForkKey, api::proposer_api::GetExecutionPayloadBidParams, chain_info::ChainInfo,
};
use tokio::sync::oneshot;
use tracing::warn;

use crate::{
    api::proposer::{GloasBuilderIdentity, ProposerApiError},
    auctioneer::{
        bid_adjustor::BidAdjustor,
        context::Context,
        types::{GetExecutionPayloadBidResult, GloasPayload, SlotData},
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
            let fork = ForkKey { parent_hash: params.parent_hash, parent_root: params.parent_root };
            let best_block_hash =
                self.bid_sorter.get_header(&fork).ok_or(ProposerApiError::NoBidPrepared)?;
            let entry =
                self.gloas_payloads.get(&best_block_hash).ok_or(ProposerApiError::NoBidPrepared)?;
            build_signed_bid(entry, &self.gloas_builder_identity, &self.chain_info)
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

pub(super) fn build_signed_bid(
    entry: &GloasPayload,
    identity: &GloasBuilderIdentity,
    chain_info: &ChainInfo,
) -> Result<Vec<u8>, ProposerApiError> {
    identity.sign_bid(&entry.bid, chain_info)
}

#[cfg(test)]
mod tests {
    use alloy_primitives::B256;
    use helix_common::PayloadAttributesUpdate;
    use helix_types::{
        BlsPublicKeyBytes, Domain, EthSpec, ForkName, SignedRoot, Slot, TestRandomSeed,
    };

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

    fn bid_identity(builder_index: u64) -> GloasBuilderIdentity {
        helix_common::utils::install_default_crypto_provider();
        let identity = GloasBuilderIdentity::new(helix_types::BlsKeypair::random());
        identity.set_builder_index(Some(builder_index));
        identity
    }

    fn gloas_payload(
        block_hash: B256,
        parent_hash: B256,
        parent_root: B256,
        value: u64,
        fee_recipient: alloy_primitives::Address,
    ) -> GloasPayload {
        use std::sync::Arc;

        use helix_types::{
            BidTrace, ExecutionBlockHash, ExecutionPayloadGloas, GloasBidTrace,
            SignedBidSubmissionGloas,
        };

        let mut payload = ExecutionPayloadGloas::default();
        payload.block_hash = ExecutionBlockHash(block_hash);
        payload.parent_hash = ExecutionBlockHash(parent_hash);
        payload.slot_number = Slot::new(BID_SLOT);
        let mut trace = BidTrace::test_random();
        trace.value = alloy_primitives::U256::from(value);
        let submission = SignedBidSubmissionGloas {
            message: GloasBidTrace { trace, parent_block_root: parent_root },
            execution_payload: Arc::new(payload),
            blobs_bundle: Default::default(),
            execution_requests: Default::default(),
            signature: Default::default(),
        };
        GloasPayload::new(
            helix_types::GloasBidSubmission::from_ssz_bytes(
                ssz::Encode::as_ssz_bytes(&submission).into(),
            )
            .unwrap(),
            fee_recipient,
        )
    }

    #[test]
    fn build_signed_bid_uses_the_entrys_data_and_configured_identity() {
        let chain_info = ChainInfo::default();
        let block_hash = B256::repeat_byte(0x99);
        let parent_hash = B256::repeat_byte(0x11);
        let parent_root = B256::repeat_byte(0x22);
        let fee_recipient = alloy_primitives::Address::repeat_byte(0x44);
        let entry =
            gloas_payload(block_hash, parent_hash, parent_root, 42_000_000_000, fee_recipient);
        let identity = bid_identity(7);

        let signed_bid = <helix_types::SignedExecutionPayloadBid as ssz::Decode>::from_ssz_bytes(
            &build_signed_bid(&entry, &identity, &chain_info).unwrap(),
        )
        .expect("the signed bid decodes as the consensus type");

        assert_eq!(signed_bid.message.block_hash.0, block_hash);
        assert_eq!(signed_bid.message.parent_block_hash.0, parent_hash);
        assert_eq!(signed_bid.message.parent_block_root, parent_root);
        assert_eq!(signed_bid.message.builder_index, 7);
        assert_eq!(signed_bid.message.value, 0, "the proposer is paid in-block");
        assert_eq!(signed_bid.message.execution_payment, 42, "wei converts to gwei");
        assert_eq!(
            signed_bid.message.fee_recipient, fee_recipient,
            "consensus pays any enshrined amount here, so it must be the proposer's",
        );

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
