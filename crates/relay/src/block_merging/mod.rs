//! Relay side of the builder block-merging TCP protocol
//! (`helix_tcp_types::merging`). The tile dials configured builders, forwards
//! decoded submissions carrying merging data and streams merged blocks back
//! into the auction.

mod tile;

use std::collections::HashMap;

use helix_tcp_types::merging::{
    MergingFrameHeader, MergingMsgId,
    builder_to_relay::MergedBlockV1,
    order::{BundleOrderRef, MergeOrderRef, TxOrderRef},
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
