//! Relay side of the builder block-merging TCP protocol
//! (`helix_tcp_types::merging`). The tile dials configured builders, forwards
//! decoded submissions carrying merging data and streams merged blocks back
//! into the auction.

mod tile;
mod unbundling;

use std::collections::HashMap;

use alloy_consensus::{Bytes48, Transaction as _, TxEnvelope};
use alloy_primitives::{Address, B256};
use alloy_rlp::Decodable;
use helix_tcp_types::merging::{
    MergingFrameHeader, MergingMsgId,
    builder_to_relay::MergedBlockV1,
    order::{BundleOrderRef, MergeOrderRef, TxOrderRef, bundle_order_hash},
};
use helix_types::{
    BlobWithMetadata, BlobsBundle, BuilderInclusionResult, ExecutionPayload, KzgCommitment,
    MergeOrderFlags, MergedBlockTrace, Order, payload_from_v3, requests_from_v4,
};
use rustc_hash::{FxHashMap, FxHashSet};
use ssz::Encode;
pub use tile::BlockMergingTile;
// Bench-only visibility, see benches/unbundling.rs.
#[cfg(feature = "bench-internals")]
pub use unbundling::{OrderTxs, find_unbundled_txs};

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
            latest_only: false,
        }),
        Order::BundleV2(bundle) => MergeOrderRef::Bundle(BundleOrderRef {
            txs: bundle.txs.iter().map(|&i| idx(i)).collect::<Option<_>>()?,
            reverting_txs: bundle.reverting_txs.iter().map(|&i| idx(i)).collect::<Option<_>>()?,
            dropping_txs: bundle.dropping_txs.iter().map(|&i| idx(i)).collect::<Option<_>>()?,
            latest_only: bundle.flags.contains(MergeOrderFlags::LATEST_ONLY),
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

/// Maps the wire message onto the simulator response type so the auctioneer reuses
/// `handle_merge_response` unchanged. Resolves the merged block's full blob set --
/// the base block's own blob txs as well as any newly appended ones -- from
/// `blob_sidecars` (this tile's own cache of blob sidecars seen in submissions this
/// slot); `None` if any referenced hash can't be resolved or the resolved set is
/// invalid, since a merged block missing a blob sidecar can't be finalized.
fn merged_block_to_response(
    m: MergedBlockV1,
    blob_sidecars: &FxHashMap<B256, BlobWithMetadata>,
    max_blobs_per_block: usize,
) -> Option<BlockMergeResponse> {
    let execution_payload = payload_from_v3(m.execution_payload)?;
    let blobs_bundle =
        resolve_blobs_bundle(&execution_payload, blob_sidecars, max_blobs_per_block)?;

    let builder_inclusions: HashMap<_, _> = m
        .builder_inclusions
        .into_iter()
        .map(|i| {
            (i.origin_coinbase, BuilderInclusionResult {
                contribution: i.contribution,
                revenue: i.revenue,
                txs: i.txs,
            })
        })
        .collect();
    // Base txs are replayed verbatim at the front of a merged block; every appended order tx
    // and the trailing distribution tx come after them, in that order (see
    // `MergeSession::emit` in crates/builder/src/engine/session.rs). So the base payment tx's
    // index is always total-appended-count minus the appended order txs minus the distribution
    // tx. `checked_sub` guards against a malformed/adversarial wire count exceeding the actual
    // tx list, same failure mode as this function's other validity checks.
    let appended_order_txs = appended_tx_hashes(&builder_inclusions).len();
    let base_payment_tx_index =
        execution_payload.transactions.len().checked_sub(appended_order_txs + 2)?;
    Some(BlockMergeResponse {
        base_block_hash: m.base_block_hash,
        execution_payload,
        execution_requests: requests_from_v4(m.execution_requests)?,
        blobs_bundle,
        proposer_value: m.proposer_value,
        base_builder_revenue: m.base_builder_revenue,
        relay_revenue: m.relay_revenue,
        builder_inclusions,
        base_payment_tx_index,
        trace: MergedBlockTrace {
            request_time_ns: m.trace.base_block_recv_ns,
            sim_start_time_ns: m.trace.sim_start_ns,
            sim_end_time_ns: m.trace.sim_end_ns,
            finalize_time_ns: m.trace.finalize_ns,
            header_served_time_ns: None, /* filled in by the auctioneer when it sends the header
                                          * to the proposer */
            was_top_builder: None,
            top_bid: None,
        },
    })
}

/// Every tx hash contributed by a merged-in order, across all builders -- as opposed to the
/// base block's own content or the trailing distribution tx (which isn't attributed to any
/// builder's `revenue.txs`; see `MergeSession::emit`). Used both to filter the unbundling
/// check to genuinely appended content and to locate the base block's own payment tx.
fn appended_tx_hashes(
    builder_inclusions: &HashMap<Address, BuilderInclusionResult>,
) -> FxHashSet<B256> {
    builder_inclusions.values().flat_map(|inclusion| inclusion.txs.iter().copied()).collect()
}

/// Resolves every blob versioned hash referenced by the merged block's own transactions --
/// base txs and newly appended txs alike -- from `blob_sidecars`. Trusting only the wire's
/// `appended_blobs` list would drop the base block's own blob sidecars whenever it already
/// carried blob txs, producing a merged block whose transactions reference blob hashes with
/// no matching `blobs_bundle` entry (surfaces downstream as a blob-versioned-hash mismatch
/// during simulation).
fn resolve_blobs_bundle(
    payload: &ExecutionPayload,
    blob_sidecars: &FxHashMap<B256, BlobWithMetadata>,
    max_blobs_per_block: usize,
) -> Option<BlobsBundle> {
    let mut bundle = BlobsBundle::default();
    for tx in payload.transactions.iter() {
        let envelope = TxEnvelope::decode(&mut tx.0.as_ref()).ok()?;
        for hash in envelope.blob_versioned_hashes().unwrap_or_default() {
            let sidecar = blob_sidecars.get(hash)?;
            bundle
                .push_blob(
                    sidecar.commitment,
                    &sidecar.proofs,
                    sidecar.blob.clone(),
                    max_blobs_per_block,
                )
                .ok()?;
        }
    }
    bundle.validate_ssz_lengths(max_blobs_per_block).ok()?;
    Some(bundle)
}

/// This submission's own blob sidecars, keyed by KZG versioned hash — cached so a later
/// merged block can re-attach one if it appended a blob tx originating from this submission.
fn submission_blob_sidecars(
    bundle: &BlobsBundle,
) -> impl Iterator<Item = (B256, BlobWithMetadata)> + '_ {
    bundle.iter_blobs().map(|(blob, commitment, proofs)| {
        (calculate_versioned_hash(*commitment), BlobWithMetadata {
            commitment: *commitment,
            proofs: proofs.to_vec(),
            blob: blob.clone(),
        })
    })
}

fn calculate_versioned_hash(commitment: Bytes48) -> B256 {
    KzgCommitment(*commitment).calculate_versioned_hash()
}

#[cfg(test)]
mod tests {
    use helix_types::{BundleOrder, BundleOrderV2, MergeOrderFlags, TransactionOrder, TxIndices};

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
                latest_only: false,
            }))
        );

        let bundle_v2 = Order::BundleV2(BundleOrderV2 {
            txs: indices(&[1, 2]),
            reverting_txs: indices(&[0]),
            dropping_txs: indices(&[1]),
            flags: MergeOrderFlags::LATEST_ONLY,
        });
        assert_eq!(
            order_to_ref(&bundle_v2),
            Some(MergeOrderRef::Bundle(BundleOrderRef {
                txs: vec![1, 2],
                reverting_txs: vec![0],
                dropping_txs: vec![1],
                latest_only: true,
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
            latest_only: false,
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

    /// Reproduces the RELAY-FR incident: a merge builder appends no new blob
    /// txs (`appended_blobs` empty on the wire) to a base block that already
    /// carries one. The resolved response must still include the base
    /// block's own blob, or downstream simulation sees a blob tx with no
    /// matching `blobs_bundle` entry ("expected blob versioned hashes do not
    /// match the given transactions").
    #[test]
    fn merged_block_to_response_includes_base_blocks_own_blob_tx() {
        use alloy_consensus::{TxEip4844, TxEnvelope};
        use alloy_primitives::{Address, Bloom, Signature, U256};
        use alloy_rlp::Encodable;
        use alloy_rpc_types::{
            beacon::{BlsPublicKey, requests::ExecutionRequestsV4},
            engine::{ExecutionPayloadV1, ExecutionPayloadV2, ExecutionPayloadV3},
        };
        use helix_tcp_types::merging::builder_to_relay::MergeTraceV1;
        use helix_types::Blob;

        let commitment: Bytes48 = Bytes48::default();
        let hash = calculate_versioned_hash(commitment);

        let tx = TxEip4844 { blob_versioned_hashes: vec![hash], ..Default::default() };
        let envelope = TxEnvelope::new_unhashed(
            tx.into(),
            Signature::new(Default::default(), Default::default(), Default::default()),
        );
        let mut raw = vec![];
        envelope.encode(&mut raw);

        let base_block_hash = B256::repeat_byte(9);
        let execution_payload = ExecutionPayloadV3 {
            payload_inner: ExecutionPayloadV2 {
                payload_inner: ExecutionPayloadV1 {
                    parent_hash: B256::ZERO,
                    fee_recipient: Address::ZERO,
                    state_root: B256::ZERO,
                    receipts_root: B256::ZERO,
                    logs_bloom: Bloom::default(),
                    prev_randao: B256::ZERO,
                    block_number: 1,
                    gas_limit: 30_000_000,
                    gas_used: 0,
                    timestamp: 0,
                    extra_data: Default::default(),
                    base_fee_per_gas: U256::from(1),
                    block_hash: base_block_hash,
                    // The blob tx plus a trailing distribution tx -- every real merged block
                    // has at least these two (see `MergeSession::emit`).
                    transactions: vec![raw.into(), raw_plain_tx()],
                },
                withdrawals: vec![],
            },
            blob_gas_used: 0,
            excess_blob_gas: 0,
        };
        let merged = MergedBlockV1 {
            slot: 5,
            response_id: 0,
            base_block_hash,
            base_builder_pubkey: BlsPublicKey::default(),
            execution_payload,
            execution_requests: ExecutionRequestsV4::default(),
            appended_blobs: vec![],
            proposer_value: U256::from(1),
            base_builder_revenue: U256::ZERO,
            relay_revenue: U256::ZERO,
            builder_inclusions: vec![],
            included_order_ids: vec![],
            trace: MergeTraceV1::default(),
        };

        let mut blob_sidecars = FxHashMap::default();
        blob_sidecars.insert(hash, BlobWithMetadata {
            commitment,
            proofs: vec![Bytes48::default(); 128],
            blob: Blob::default(),
        });

        let response =
            merged_block_to_response(merged, &blob_sidecars, 9).expect("known blob resolves");

        assert_eq!(response.blobs_bundle.blobs.len(), 1);
        assert_eq!(response.blobs_bundle.commitments[0], commitment);
    }

    /// The base block's own blob tx and a merge builder's newly appended
    /// blob tx must both survive resolution, not just the appended one.
    #[test]
    fn merged_block_to_response_includes_both_base_and_appended_blobs() {
        use alloy_consensus::{TxEip4844, TxEnvelope};
        use alloy_primitives::{Address, Bloom, Signature, U256};
        use alloy_rlp::Encodable;
        use alloy_rpc_types::{
            beacon::{BlsPublicKey, requests::ExecutionRequestsV4},
            engine::{ExecutionPayloadV1, ExecutionPayloadV2, ExecutionPayloadV3},
        };
        use helix_tcp_types::merging::builder_to_relay::MergeTraceV1;
        use helix_types::Blob;

        fn raw_blob_tx(hash: B256) -> alloy_primitives::Bytes {
            let tx = TxEip4844 { blob_versioned_hashes: vec![hash], ..Default::default() };
            let envelope = TxEnvelope::new_unhashed(
                tx.into(),
                Signature::new(Default::default(), Default::default(), Default::default()),
            );
            let mut raw = vec![];
            envelope.encode(&mut raw);
            raw.into()
        }

        let base_commitment: Bytes48 = Bytes48::default();
        let base_hash = calculate_versioned_hash(base_commitment);
        let appended_commitment: Bytes48 = Bytes48::repeat_byte(1);
        let appended_hash = calculate_versioned_hash(appended_commitment);

        let base_block_hash = B256::repeat_byte(9);
        let execution_payload = ExecutionPayloadV3 {
            payload_inner: ExecutionPayloadV2 {
                payload_inner: ExecutionPayloadV1 {
                    parent_hash: B256::ZERO,
                    fee_recipient: Address::ZERO,
                    state_root: B256::ZERO,
                    receipts_root: B256::ZERO,
                    logs_bloom: Bloom::default(),
                    prev_randao: B256::ZERO,
                    block_number: 1,
                    gas_limit: 30_000_000,
                    gas_used: 0,
                    timestamp: 0,
                    extra_data: Default::default(),
                    base_fee_per_gas: U256::from(1),
                    block_hash: base_block_hash,
                    transactions: vec![raw_blob_tx(base_hash), raw_blob_tx(appended_hash)],
                },
                withdrawals: vec![],
            },
            blob_gas_used: 0,
            excess_blob_gas: 0,
        };
        let merged = MergedBlockV1 {
            slot: 5,
            response_id: 0,
            base_block_hash,
            base_builder_pubkey: BlsPublicKey::default(),
            execution_payload,
            execution_requests: ExecutionRequestsV4::default(),
            appended_blobs: vec![appended_hash],
            proposer_value: U256::from(1),
            base_builder_revenue: U256::ZERO,
            relay_revenue: U256::ZERO,
            builder_inclusions: vec![],
            included_order_ids: vec![],
            trace: MergeTraceV1::default(),
        };

        let mut blob_sidecars = FxHashMap::default();
        blob_sidecars.insert(base_hash, BlobWithMetadata {
            commitment: base_commitment,
            proofs: vec![Bytes48::default(); 128],
            blob: Blob::default(),
        });
        blob_sidecars.insert(appended_hash, BlobWithMetadata {
            commitment: appended_commitment,
            proofs: vec![Bytes48::default(); 128],
            blob: Blob::default(),
        });

        let response =
            merged_block_to_response(merged, &blob_sidecars, 9).expect("both blobs resolve");

        let commitments: Vec<_> = response.blobs_bundle.commitments.iter().copied().collect();
        assert_eq!(commitments.len(), 2);
        assert!(commitments.contains(&base_commitment));
        assert!(commitments.contains(&appended_commitment));
    }

    fn raw_plain_tx() -> alloy_primitives::Bytes {
        use alloy_consensus::{TxEip1559, TxEnvelope};
        use alloy_primitives::Signature;
        use alloy_rlp::Encodable;

        let envelope = TxEnvelope::new_unhashed(
            TxEip1559::default().into(),
            Signature::new(Default::default(), Default::default(), Default::default()),
        );
        let mut raw = vec![];
        envelope.encode(&mut raw);
        raw.into()
    }

    fn merged_block_with_txs(n_txs: usize, appended: &[B256]) -> MergedBlockV1 {
        use alloy_rpc_types::{
            beacon::{BlsPublicKey, requests::ExecutionRequestsV4},
            engine::{ExecutionPayloadV1, ExecutionPayloadV2, ExecutionPayloadV3},
        };
        use helix_tcp_types::merging::builder_to_relay::{BuilderInclusion, MergeTraceV1};

        let base_block_hash = B256::repeat_byte(9);
        let execution_payload = ExecutionPayloadV3 {
            payload_inner: ExecutionPayloadV2 {
                payload_inner: ExecutionPayloadV1 {
                    parent_hash: B256::ZERO,
                    fee_recipient: Address::ZERO,
                    state_root: B256::ZERO,
                    receipts_root: B256::ZERO,
                    logs_bloom: Default::default(),
                    prev_randao: B256::ZERO,
                    block_number: 1,
                    gas_limit: 30_000_000,
                    gas_used: 0,
                    timestamp: 0,
                    extra_data: Default::default(),
                    base_fee_per_gas: alloy_primitives::U256::from(1),
                    block_hash: base_block_hash,
                    transactions: (0..n_txs).map(|_| raw_plain_tx()).collect(),
                },
                withdrawals: vec![],
            },
            blob_gas_used: 0,
            excess_blob_gas: 0,
        };
        let builder_inclusions = if appended.is_empty() {
            vec![]
        } else {
            vec![BuilderInclusion {
                builder_pubkey: BlsPublicKey::default(),
                origin_coinbase: Address::repeat_byte(0xa),
                contribution: alloy_primitives::U256::ZERO,
                revenue: alloy_primitives::U256::ZERO,
                txs: appended.to_vec(),
            }]
        };
        MergedBlockV1 {
            slot: 5,
            response_id: 0,
            base_block_hash,
            base_builder_pubkey: BlsPublicKey::default(),
            execution_payload,
            execution_requests: ExecutionRequestsV4::default(),
            appended_blobs: vec![],
            proposer_value: alloy_primitives::U256::from(1),
            base_builder_revenue: alloy_primitives::U256::ZERO,
            relay_revenue: alloy_primitives::U256::ZERO,
            builder_inclusions,
            included_order_ids: vec![],
            trace: MergeTraceV1::default(),
        }
    }

    /// No appended orders: the base block's own trailing tx is the only
    /// candidate, so `base_payment_tx_index` is just the second-to-last
    /// position (the distribution tx is always last).
    #[test]
    fn merged_block_to_response_base_payment_tx_index_with_no_appended_orders() {
        let merged = merged_block_with_txs(4, &[]);
        let blob_sidecars = FxHashMap::default();

        let response = merged_block_to_response(merged, &blob_sidecars, 9).unwrap();

        assert_eq!(response.base_payment_tx_index, 2);
    }

    /// With appended order txs in between the base content and the
    /// distribution tx, the base payment tx index must skip over them.
    #[test]
    fn merged_block_to_response_base_payment_tx_index_with_appended_orders() {
        let appended = [B256::repeat_byte(1), B256::repeat_byte(2)];
        let merged = merged_block_with_txs(5, &appended);
        let blob_sidecars = FxHashMap::default();

        let response = merged_block_to_response(merged, &blob_sidecars, 9).unwrap();

        assert_eq!(response.base_payment_tx_index, 1);
    }

    /// A wire count of appended txs that exceeds the actual tx list is
    /// malformed; must be rejected rather than underflow the index.
    #[test]
    fn merged_block_to_response_none_when_appended_count_exceeds_tx_list() {
        let appended = [B256::repeat_byte(1), B256::repeat_byte(2), B256::repeat_byte(3)];
        let merged = merged_block_with_txs(2, &appended);
        let blob_sidecars = FxHashMap::default();

        assert!(merged_block_to_response(merged, &blob_sidecars, 9).is_none());
    }
}
