use helix_common::api::proposer_api::GetExecutionPayloadBidParams;
use tokio::sync::oneshot;
use tracing::warn;

use crate::{
    api::proposer::ProposerApiError,
    auctioneer::{
        bid_adjustor::BidAdjustor,
        context::Context,
        types::{GetExecutionPayloadBidResult, SlotData},
    },
};

impl<B: BidAdjustor> Context<B> {
    pub(super) fn handle_get_execution_payload_bid(
        &self,
        params: GetExecutionPayloadBidParams,
        slot_data: &SlotData,
        res_tx: oneshot::Sender<GetExecutionPayloadBidResult>,
    ) {
        let _ = res_tx.send(get_execution_payload_bid(&params, slot_data));
    }
}

/// Checks `params.parent_hash`/`params.parent_root` against currently-live payload attributes,
/// then reports "no bid available" -- serving a real Gloas bid needs step 5's builder->relay
/// submission wire format, not landed yet.
pub(super) fn get_execution_payload_bid(
    params: &GetExecutionPayloadBidParams,
    slot_data: &SlotData,
) -> GetExecutionPayloadBidResult {
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

    Err(ProposerApiError::NoBidPrepared)
}

#[cfg(test)]
mod tests {
    use alloy_primitives::B256;
    use helix_common::PayloadAttributesUpdate;
    use helix_types::ForkName;
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

        let result = get_execution_payload_bid(&params(parent_hash, parent_root), &data);

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

        let result = get_execution_payload_bid(&params(parent_hash, requested_root), &data);

        assert!(matches!(result, Err(ProposerApiError::NoBidPrepared)));
    }

    #[test]
    fn missing_parent_beacon_block_root_is_no_bid() {
        let parent_hash = B256::repeat_byte(0x11);
        let requested_root = B256::repeat_byte(0x33);
        let mut map = FxHashMap::default();
        map.insert(parent_hash, attrs_update(parent_hash, None));
        let data = slot_data(map);

        let result = get_execution_payload_bid(&params(parent_hash, requested_root), &data);

        assert!(matches!(result, Err(ProposerApiError::NoBidPrepared)));
    }

    #[test]
    fn matching_parent_still_reports_no_bid_until_step_5() {
        let parent_hash = B256::repeat_byte(0x11);
        let parent_root = B256::repeat_byte(0x22);
        let mut map = FxHashMap::default();
        map.insert(parent_hash, attrs_update(parent_hash, Some(parent_root)));
        let data = slot_data(map);

        let result = get_execution_payload_bid(&params(parent_hash, parent_root), &data);

        assert!(matches!(result, Err(ProposerApiError::NoBidPrepared)));
    }
}
