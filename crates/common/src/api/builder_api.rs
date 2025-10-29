use alloy_consensus::TxEnvelope;
use alloy_primitives::{Address, B256, U256};
use alloy_rlp::Decodable;
use helix_types::{
    BlsPublicKeyBytes, SignedValidatorRegistration, Slot, Transaction, Transactions,
};
use serde::{Deserialize, Serialize};
use ssz::Encode;
use ssz_derive::{Decode, Encode};
use tree_hash_derive::TreeHash;

use crate::{BuilderValidatorPreferences, api::proposer_api::ValidatorRegistrationInfo};

#[derive(Debug, Default, Clone, serde::Serialize, serde::Deserialize)]
pub struct BuilderGetValidatorsResponseEntry {
    pub slot: Slot,
    #[serde(with = "serde_utils::quoted_u64")]
    pub validator_index: u64,
    pub entry: ValidatorRegistrationInfo,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct BuilderGetValidatorsResponse {
    pub slot: Slot,
    #[serde(with = "serde_utils::quoted_u64")]
    pub validator_index: u64,
    pub entry: SignedValidatorRegistration,
    pub preferences: BuilderValidatorPreferences,
}

impl From<BuilderGetValidatorsResponseEntry> for BuilderGetValidatorsResponse {
    fn from(entry: BuilderGetValidatorsResponseEntry) -> Self {
        Self {
            slot: entry.slot,
            validator_index: entry.validator_index,
            entry: entry.entry.registration,
            preferences: entry.entry.preferences.into(),
        }
    }
}

#[derive(Clone, Debug, Encode, Decode)]
pub struct TopBidUpdate {
    pub timestamp: u64,
    pub slot: u64,
    pub block_number: u64,
    pub block_hash: B256,
    pub parent_hash: B256,
    pub builder_pubkey: BlsPublicKeyBytes,
    pub fee_recipient: Address,
    pub value: U256,
}

impl TopBidUpdate {
    const SSZ_SIZE: usize = 188;

    pub fn as_ssz_bytes_fast(&self) -> Vec<u8> {
        let mut vec = Vec::with_capacity(Self::SSZ_SIZE);
        self.ssz_append(&mut vec);
        vec
    }
}

pub type SlotCoordinate = (u64, BlsPublicKeyBytes, B256);

#[derive(Clone, Debug)]
pub struct InclusionListWithKey {
    pub key: SlotCoordinate,
    pub inclusion_list: InclusionListWithMetadata,
}

impl<'a> From<&'a InclusionListWithKey> for (&'a InclusionListWithMetadata, &'a SlotCoordinate) {
    fn from(value: &'a InclusionListWithKey) -> Self {
        (&value.inclusion_list, &value.key)
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct InclusionListTxWithMetadata {
    pub hash: B256,
    pub bytes: Transaction,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct InclusionListWithMetadata {
    pub txs: Vec<InclusionListTxWithMetadata>,
}

impl TryFrom<InclusionList> for InclusionListWithMetadata {
    type Error = String;

    fn try_from(inclusion_list: InclusionList) -> Result<Self, Self::Error> {
        let mut txs = Vec::with_capacity(inclusion_list.txs.len());

        for encoded_tx in inclusion_list.txs {
            let decoded_tx = TxEnvelope::decode(&mut encoded_tx.as_ref())
                .map_err(|err| format!("Failed to decode transaction: {err}"))?;
            let tx_with_md =
                InclusionListTxWithMetadata { hash: *decoded_tx.hash(), bytes: encoded_tx };

            txs.push(tx_with_md);
        }

        Ok(Self { txs })
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, Encode, Decode, TreeHash)]
pub struct InclusionList {
    pub txs: Transactions,
}

impl From<&InclusionListWithMetadata> for InclusionList {
    fn from(value: &InclusionListWithMetadata) -> Self {
        let txs: Vec<_> = value.txs.iter().map(|tx| tx.bytes.clone()).collect();
        InclusionList { txs: txs.into() }
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{B256, Bytes, U256};
    use helix_types::Transaction;

    use super::*;
    use crate::api::builder_api::InclusionListTxWithMetadata;

    #[test]
    fn derive_rlp_bytes_to_inclusion_list_with_metadata() {
        let il: InclusionList = InclusionList { txs: vec![
                Transaction("0x02f87582426801850221646a70850221646a7082520894acabf6c2d38973a5f2ebab6b5e85623db1005a4e880ddf2f839aa3d97080c080a0088ae2635655e314949dae343ac296c3fb6ac56802e1024639f9603c61e253669f2bf33fe18ce70520abc6a662794c1ef5bb310248b7b7c4acc6be93e7885d62".parse::<Bytes>().unwrap()),
                Transaction("0x02f8b5824268820130850239465d16850239465d1682728a9494373a4919b3240d86ea41593d5eba789fef384880b844095ea7b30000000000000000000000005fbe74a283f7954f10aa04c2edf55578811aeb03000000000000000000000000000000000000000000000000000009184e72a000c080a0a6d0c20df1f0582c0dbf62a125fc1874868106d845c3916d666441973fb29ff0a04f4ce8879bd85510c82246de9a58a5a705dc672b6d89cc8183544f2db8649ea9".parse::<Bytes>().unwrap()),
                Transaction("0x02f8b48242688193850232306c41850232306c4182739294685ce6742351ae9b618f383883d6d1e0c5a31b4b80b844095ea7b30000000000000000000000005fbe74a283f7954f10aa04c2edf55578811aeb030000000000000000000000000000000000000000000000000de0b6b3a7640000c001a0bda9b5171b2e0d3ceceebfa4de504485bd6a8fd5041251fcd2d4aed24a65ab4ca0369e2d4bcc7ef790e64195578005c8a6c3313dd0b0bc105856d0202a4e230de9".parse::<Bytes>().unwrap()),
            ].into() };
        let il_len = il.txs.len();
        let il_w_md = InclusionListWithMetadata::try_from(il).unwrap();

        assert_eq!(il_len, il_w_md.txs.len());
        assert_eq!(il_w_md.txs[0], InclusionListTxWithMetadata {
            hash: "0x80e000dd49cd0c518e8e426cf00daeebbae81ee79e0bb669601a329d1cafa6c2"
                .parse::<B256>()
                .unwrap(),
            bytes: Transaction("0x02f87582426801850221646a70850221646a7082520894acabf6c2d38973a5f2ebab6b5e85623db1005a4e880ddf2f839aa3d97080c080a0088ae2635655e314949dae343ac296c3fb6ac56802e1024639f9603c61e253669f2bf33fe18ce70520abc6a662794c1ef5bb310248b7b7c4acc6be93e7885d62".parse::<Bytes>().unwrap())
        });
    }

    #[test]
    fn top_bid_ssz_fast_path() {
        let x = TopBidUpdate {
            timestamp: u64::MAX,
            slot: u64::MAX,
            block_number: u64::MAX,
            block_hash: B256::random(),
            parent_hash: B256::random(),
            builder_pubkey: BlsPublicKeyBytes::random(),
            fee_recipient: Address::random(),
            value: U256::ZERO,
        };

        let ssz = x.as_ssz_bytes();
        let ssz_check = x.as_ssz_bytes_fast();

        assert_eq!(ssz, ssz_check);
        assert_eq!(ssz.len(), TopBidUpdate::SSZ_SIZE);
    }
}
