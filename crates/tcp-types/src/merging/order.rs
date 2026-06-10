use alloy_primitives::{Address, B256, keccak256};
use alloy_rpc_types::beacon::BlsPublicKey;
use ssz_derive::{Decode, Encode};

/// Validation limits, enforced on decode (`ethereum_ssz` `Vec<T>` carries no
/// type-level bound). Violation => reject + disconnect.
pub const MAX_TX_BYTES: usize = 131_072;
pub const MAX_BUNDLE_TXS: usize = 32;
pub const MAX_ORDERS_PER_FRAME: usize = 1024;

/// Identity of a mergeable order, traceable to the contributing builder and
/// the block the order was extracted from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Encode, Decode)]
pub struct OrderMeta {
    /// Single tx: `keccak(tx RLP)`. Bundle: `keccak(concat ordered tx hashes)`.
    pub order_hash: B256,
    /// Contributing builder identity.
    pub builder_pubkey: BlsPublicKey,
    /// Credited for inclusion revenue.
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

#[derive(Debug, Clone, PartialEq, Eq, Encode, Decode)]
#[ssz(enum_behaviour = "union")]
pub enum MergeableOrder {
    Tx(MergeableTx),
    Bundle(MergeableBundle),
}

#[derive(Debug, Clone, PartialEq, Eq, Encode, Decode)]
pub struct MergeableTx {
    /// EIP-2718 encoded signed transaction, no blob sidecar.
    pub transaction: Vec<u8>,
    pub can_revert: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Encode, Decode)]
pub struct MergeableBundle {
    /// EIP-2718 encoded signed transactions, no blob sidecars.
    pub transactions: Vec<Vec<u8>>,
    /// Indices into `transactions` allowed to revert.
    pub reverting_txs: Vec<u16>,
    /// Indices into `transactions` allowed to be omitted, but not revert.
    pub dropping_txs: Vec<u16>,
}

#[derive(Debug, Clone, Copy)]
pub struct InvalidMergeableOrder;

impl MergeableOrder {
    pub fn validate(&self) -> Result<(), InvalidMergeableOrder> {
        match self {
            Self::Tx(tx) => {
                if tx.transaction.is_empty() || tx.transaction.len() > MAX_TX_BYTES {
                    return Err(InvalidMergeableOrder);
                }
            }
            Self::Bundle(bundle) => {
                if bundle.transactions.is_empty() ||
                    bundle.transactions.len() > MAX_BUNDLE_TXS ||
                    bundle.transactions.iter().any(|tx| tx.is_empty() || tx.len() > MAX_TX_BYTES) ||
                    bundle.reverting_txs.iter().any(|&i| i as usize >= bundle.transactions.len()) ||
                    bundle.dropping_txs.iter().any(|&i| i as usize >= bundle.transactions.len())
                {
                    return Err(InvalidMergeableOrder);
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
    fn order_union_roundtrip() {
        use ssz::{Decode, Encode};

        let order = MergeableOrder::Bundle(MergeableBundle {
            transactions: vec![vec![1, 2, 3], vec![4, 5]],
            reverting_txs: vec![0],
            dropping_txs: vec![1],
        });
        let bytes = order.as_ssz_bytes();
        assert_eq!(MergeableOrder::from_ssz_bytes(&bytes).unwrap(), order);
        order.validate().unwrap();
    }

    #[test]
    fn bundle_index_validation() {
        let bundle = MergeableOrder::Bundle(MergeableBundle {
            transactions: vec![vec![1]],
            reverting_txs: vec![1],
            dropping_txs: vec![],
        });
        assert!(bundle.validate().is_err());
    }
}
