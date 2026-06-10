use alloy_primitives::{Address, B256, keccak256};
use alloy_rpc_types::beacon::BlsPublicKey;
use ssz_derive::{Decode, Encode};

/// Validation limits, enforced on decode (`ethereum_ssz` `Vec<T>` carries no
/// type-level bound). Violation => reject.
pub const MAX_TX_BYTES: usize = 131_072;
pub const MAX_BUNDLE_TXS: usize = 32;
pub const MAX_ORDERS_PER_BLOCK: usize = 1024;
pub const MAX_BLOCK_TXS: usize = u16::MAX as usize;

/// Identity of a mergeable order, derived by both sides from a forwarded
/// `MergeableBlockV1`; traceable to the contributing builder and the block
/// the order came from. Not itself a wire type — the canonical id spec.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Encode, Decode)]
pub struct OrderMeta {
    /// Single tx: `keccak(tx bytes)`. Bundle: `keccak(concat tx hashes)`.
    pub order_hash: B256,
    /// Contributing builder identity.
    pub builder_pubkey: BlsPublicKey,
    /// Credited for inclusion revenue (`builder_address` of the block's
    /// merging data).
    pub origin_coinbase: Address,
    /// Block the order was extracted from.
    pub source_block_hash: B256,
}

impl OrderMeta {
    /// Canonical order id, computed identically on both sides.
    pub fn order_id(&self) -> B256 {
        let mut buf = [0u8; 32 + 48];
        buf[..32].copy_from_slice(self.order_hash.as_slice());
        buf[32..].copy_from_slice(self.builder_pubkey.as_slice());
        keccak256(buf)
    }
}

pub fn bundle_order_hash(tx_hashes: &[B256]) -> B256 {
    let mut buf = Vec::with_capacity(tx_hashes.len() * 32);
    for hash in tx_hashes {
        buf.extend_from_slice(hash.as_slice());
    }
    keccak256(buf)
}

/// Index-based order reference into a forwarded block's transaction list —
/// the same shape builders attach to their submissions as `BlockMergingData`.
#[derive(Debug, Clone, PartialEq, Eq, Encode, Decode)]
#[ssz(enum_behaviour = "union")]
pub enum MergeOrderRef {
    Tx(TxOrderRef),
    Bundle(BundleOrderRef),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Encode, Decode)]
pub struct TxOrderRef {
    /// Index into the block's transactions.
    pub index: u16,
    pub can_revert: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Encode, Decode)]
pub struct BundleOrderRef {
    /// Indices into the block's transactions, in bundle order.
    pub txs: Vec<u16>,
    /// Indices into `txs` allowed to revert.
    pub reverting_txs: Vec<u16>,
    /// Indices into `txs` allowed to be omitted, but not revert.
    pub dropping_txs: Vec<u16>,
}

#[derive(Debug, Clone, Copy)]
pub struct InvalidMergeOrderRef;

impl MergeOrderRef {
    pub fn validate(&self, block_tx_count: usize) -> Result<(), InvalidMergeOrderRef> {
        match self {
            Self::Tx(tx) => {
                if tx.index as usize >= block_tx_count {
                    return Err(InvalidMergeOrderRef);
                }
            }
            Self::Bundle(bundle) => {
                if bundle.txs.is_empty() ||
                    bundle.txs.len() > MAX_BUNDLE_TXS ||
                    bundle.txs.iter().any(|&i| i as usize >= block_tx_count) ||
                    bundle.reverting_txs.iter().any(|&i| i as usize >= bundle.txs.len()) ||
                    bundle.dropping_txs.iter().any(|&i| i as usize >= bundle.txs.len())
                {
                    return Err(InvalidMergeOrderRef);
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn order_ref_union_roundtrip() {
        use ssz::{Decode, Encode};

        let order = MergeOrderRef::Bundle(BundleOrderRef {
            txs: vec![3, 5],
            reverting_txs: vec![0],
            dropping_txs: vec![1],
        });
        let bytes = order.as_ssz_bytes();
        assert_eq!(MergeOrderRef::from_ssz_bytes(&bytes).unwrap(), order);
        order.validate(6).unwrap();
    }

    #[test]
    fn order_ref_index_validation() {
        let tx = MergeOrderRef::Tx(TxOrderRef { index: 4, can_revert: false });
        assert!(tx.validate(4).is_err());
        assert!(tx.validate(5).is_ok());

        // Bundle-internal indices are into `txs`, not the block.
        let bundle = MergeOrderRef::Bundle(BundleOrderRef {
            txs: vec![0],
            reverting_txs: vec![1],
            dropping_txs: vec![],
        });
        assert!(bundle.validate(4).is_err());
    }
}
