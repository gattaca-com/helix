//! Relay side of the builder block-merging TCP protocol
//! (`helix_tcp_types::merging`). The tile dials configured builders, forwards
//! decoded submissions carrying merging data and streams merged blocks back
//! into the auction.

mod tile;

use std::collections::HashMap;

use alloy_primitives::B256;
use helix_tcp_types::merging::{
    MergingFrameHeader, MergingMsgId,
    builder_to_relay::MergedBlockV1,
    order::{BundleOrderRef, MergeOrderRef, TxOrderRef, bundle_order_hash},
};
use helix_types::{
    BuilderInclusionResult, MergedBlockTrace, Order, payload_from_v3, requests_from_v4,
};
use ssz::Encode;
pub use tile::BlockMergingTile;

use crate::simulator::BlockMergeResponse;

/// Appends `[1B msg_type][1B flags][SSZ body]`; the outer
/// `[u32 len][u64 send-ts]` frame is written by the flux stream.
fn append_frame<T: Encode>(buf: &mut Vec<u8>, msg_id: MergingMsgId, msg: &T) {
    MergingFrameHeader::new(msg_id).append_encoded(buf);
    msg.ssz_append(buf);
}

/// Index-based submission order -> wire ref. `None` if an index exceeds u16.
fn order_to_ref(order: &Order) -> Option<MergeOrderRef> {
    fn idx(i: usize) -> Option<u16> {
        u16::try_from(i).ok()
    }
    Some(match order {
        Order::Tx(tx) => {
            MergeOrderRef::Tx(TxOrderRef { index: idx(tx.index)?, can_revert: tx.can_revert })
        }
        Order::Bundle(bundle) => MergeOrderRef::Bundle(BundleOrderRef {
            txs: bundle.txs.iter().map(|&i| idx(i)).collect::<Option<_>>()?,
            reverting_txs: bundle.reverting_txs.iter().map(|&i| idx(i)).collect::<Option<_>>()?,
            dropping_txs: bundle.dropping_txs.iter().map(|&i| idx(i)).collect::<Option<_>>()?,
        }),
    })
}

/// Distinct-order identity for a wire ref: the tx hash for a solo tx, or a
/// hash of the constituent tx hashes for a bundle (`bundle_order_hash`) —
/// the same formula the merge builder uses for its own pooled `order_hash`
/// (`OrderMeta::order_hash`), so a repeat announcement of the same order —
/// any block, any builder, any resubmission — is recognised as the *same*
/// order here too, instead of the per-connection send budget counting every
/// announcement as a new one. Indices are validated against the
/// submission's own tx count well upstream of this; an out-of-range index
/// here would mean that invariant broke, and falls back to a zero hash
/// rather than panicking.
fn order_ref_hash(order_ref: &MergeOrderRef, tx_hashes: &[B256]) -> B256 {
    match order_ref {
        MergeOrderRef::Tx(tx) => tx_hashes.get(tx.index as usize).copied().unwrap_or_default(),
        MergeOrderRef::Bundle(bundle) => {
            let hashes: Vec<B256> = bundle
                .txs
                .iter()
                .map(|&i| tx_hashes.get(i as usize).copied().unwrap_or_default())
                .collect();
            bundle_order_hash(&hashes)
        }
    }
}

/// Maps the wire message onto the simulator response type so the auctioneer
/// reuses `handle_merge_response` unchanged.
fn merged_block_to_response(m: MergedBlockV1) -> BlockMergeResponse {
    let builder_inclusions: HashMap<_, _> = m
        .builder_inclusions
        .into_iter()
        .map(|i| (i.origin_coinbase, BuilderInclusionResult { revenue: i.revenue, txs: i.txs }))
        .collect();
    BlockMergeResponse {
        base_block_hash: m.base_block_hash,
        execution_payload: payload_from_v3(m.execution_payload),
        execution_requests: requests_from_v4(m.execution_requests),
        appended_blobs: m.appended_blobs,
        proposer_value: m.proposer_value,
        builder_inclusions,
        trace: MergedBlockTrace {
            request_time_ns: m.trace.base_block_recv_ns,
            sim_start_time_ns: m.trace.sim_start_ns,
            sim_end_time_ns: m.trace.sim_end_ns,
            finalize_time_ns: m.trace.finalize_ns,
            header_served_time_ns: None, /* filled in by the auctioneer when it sends the header
                                          * to the proposer */
        },
    }
}

#[cfg(test)]
mod tests {
    use helix_types::{BundleOrder, TransactionOrder, TxIndices};

    use super::*;

    fn indices(v: &[usize]) -> TxIndices {
        v.iter().copied().collect()
    }

    #[test]
    fn order_conversion() {
        let tx = Order::Tx(TransactionOrder { index: 7, can_revert: true });
        assert_eq!(
            order_to_ref(&tx),
            Some(MergeOrderRef::Tx(TxOrderRef { index: 7, can_revert: true }))
        );

        let bundle = Order::Bundle(BundleOrder {
            txs: indices(&[1, 2]),
            reverting_txs: indices(&[0]),
            dropping_txs: indices(&[1]),
        });
        assert_eq!(
            order_to_ref(&bundle),
            Some(MergeOrderRef::Bundle(BundleOrderRef {
                txs: vec![1, 2],
                reverting_txs: vec![0],
                dropping_txs: vec![1],
            }))
        );

        let oob = Order::Tx(TransactionOrder { index: u16::MAX as usize + 1, can_revert: false });
        assert_eq!(order_to_ref(&oob), None);
    }

    #[test]
    fn order_ref_hash_dedups_repeat_announcements() {
        let tx_hashes: Vec<B256> = (0u8..4).map(B256::repeat_byte).collect();

        // Same tx index -> same identity, regardless of how many times (or
        // in how many different messages) it's re-announced.
        let tx_a = MergeOrderRef::Tx(TxOrderRef { index: 1, can_revert: false });
        let tx_a_again = MergeOrderRef::Tx(TxOrderRef { index: 1, can_revert: true });
        assert_eq!(order_ref_hash(&tx_a, &tx_hashes), order_ref_hash(&tx_a_again, &tx_hashes));

        // Different tx index -> different identity.
        let tx_b = MergeOrderRef::Tx(TxOrderRef { index: 2, can_revert: false });
        assert_ne!(order_ref_hash(&tx_a, &tx_hashes), order_ref_hash(&tx_b, &tx_hashes));

        // Bundle identity matches the same formula the merge builder uses to
        // pool its own orders.
        let bundle = MergeOrderRef::Bundle(BundleOrderRef {
            txs: vec![0, 2],
            reverting_txs: vec![],
            dropping_txs: vec![],
        });
        assert_eq!(
            order_ref_hash(&bundle, &tx_hashes),
            bundle_order_hash(&[tx_hashes[0], tx_hashes[2]])
        );

        // Out-of-range index: falls back rather than panicking.
        let oob = MergeOrderRef::Tx(TxOrderRef { index: 99, can_revert: false });
        assert_eq!(order_ref_hash(&oob, &tx_hashes), B256::default());
    }

    #[test]
    fn frame_roundtrip() {
        use helix_tcp_types::merging::control::PingV1;
        use ssz::Decode;

        let mut buf = Vec::new();
        append_frame(&mut buf, MergingMsgId::PingV1, &PingV1 { nonce: 42 });
        let header = MergingFrameHeader::decode(&buf).unwrap();
        assert_eq!(header.msg_id, MergingMsgId::PingV1);
        let ping = PingV1::from_ssz_bytes(&buf[2..]).unwrap();
        assert_eq!(ping.nonce, 42);
    }
}
