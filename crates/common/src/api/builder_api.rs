use alloy_consensus::{Transaction, TxEnvelope};
use alloy_primitives::{Address, Bytes, SignatureError, B256, U256};
use alloy_rlp::Decodable;
use helix_types::{BlsPublicKey, SignedValidatorRegistration, Slot, TestRandom};
use serde::{Deserialize, Serialize};
use ssz_derive::{Decode, Encode};

use crate::{api::proposer_api::ValidatorRegistrationInfo, BuilderValidatorPreferences};

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
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

#[derive(Clone, Debug, Encode, Decode, TestRandom)]
pub struct TopBidUpdate {
    pub timestamp: u64,
    pub slot: u64,
    pub block_number: u64,
    pub block_hash: B256,
    pub parent_hash: B256,
    pub builder_pubkey: BlsPublicKey,
    pub fee_recipient: Address,
    pub value: U256,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct InclusionListTxWithMetadata {
    pub hash: B256,
    pub nonce: u64,
    pub sender: Address,
    pub gas_priority_fee: u128,
    pub bytes: Bytes,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct InclusionListWithMetadata {
    pub txs: Vec<InclusionListTxWithMetadata>,
}

impl TryFrom<InclusionList> for InclusionListWithMetadata {
    type Error = InclusionListError;

    fn try_from(inclusion_list: InclusionList) -> Result<Self, Self::Error> {
        let mut txs = Vec::with_capacity(inclusion_list.txs.len());

        for encoded_tx in inclusion_list.txs {
            let decoded_tx = TxEnvelope::decode(&mut encoded_tx.iter().as_slice())?;
            let tx_with_md = InclusionListTxWithMetadata {
                hash: *decoded_tx.hash(),
                nonce: decoded_tx.nonce(),
                sender: decoded_tx.recover_signer()?,
                gas_priority_fee: decoded_tx
                    .max_priority_fee_per_gas()
                    .ok_or(InclusionListError::MissingPriorityFee)?,
                bytes: encoded_tx,
            };

            txs.push(tx_with_md);
        }

        Ok(Self { txs })
    }
}

#[derive(Debug, thiserror::Error)]
pub enum InclusionListError {
    #[error("Failed to decode transaction: {0}")]
    TransactionDecode(#[from] alloy_rlp::Error),
    #[error("Failed to recover signer: {0}")]
    TransactionError(#[from] SignatureError),
    #[error("Transaction missing max priority fee")]
    MissingPriorityFee,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct InclusionList {
    pub txs: Vec<Bytes>,
}

impl From<&InclusionListWithMetadata> for InclusionList {
    fn from(value: &InclusionListWithMetadata) -> Self {
        InclusionList { txs: value.txs.iter().map(|tx| tx.bytes.clone()).collect() }
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{Address, Bytes, B256};

    use super::{InclusionList, InclusionListWithMetadata};
    use crate::api::builder_api::InclusionListTxWithMetadata;

    #[test]
    fn derive_rlp_bytes_to_inclusion_list_with_metadata() {
        let il: InclusionList = InclusionList { txs: vec![
                "0x02f87582426801850221646a70850221646a7082520894acabf6c2d38973a5f2ebab6b5e85623db1005a4e880ddf2f839aa3d97080c080a0088ae2635655e314949dae343ac296c3fb6ac56802e1024639f9603c61e253669f2bf33fe18ce70520abc6a662794c1ef5bb310248b7b7c4acc6be93e7885d62".parse::<Bytes>().unwrap(),
                "0x02f8b5824268820130850239465d16850239465d1682728a9494373a4919b3240d86ea41593d5eba789fef384880b844095ea7b30000000000000000000000005fbe74a283f7954f10aa04c2edf55578811aeb03000000000000000000000000000000000000000000000000000009184e72a000c080a0a6d0c20df1f0582c0dbf62a125fc1874868106d845c3916d666441973fb29ff0a04f4ce8879bd85510c82246de9a58a5a705dc672b6d89cc8183544f2db8649ea9".parse::<Bytes>().unwrap(),
                "0x02f8b48242688193850232306c41850232306c4182739294685ce6742351ae9b618f383883d6d1e0c5a31b4b80b844095ea7b30000000000000000000000005fbe74a283f7954f10aa04c2edf55578811aeb030000000000000000000000000000000000000000000000000de0b6b3a7640000c001a0bda9b5171b2e0d3ceceebfa4de504485bd6a8fd5041251fcd2d4aed24a65ab4ca0369e2d4bcc7ef790e64195578005c8a6c3313dd0b0bc105856d0202a4e230de9".parse::<Bytes>().unwrap(),
            ] };
        let il_len = il.txs.len();
        let il_w_md = InclusionListWithMetadata::try_from(il).unwrap();

        assert_eq!(il_len, il_w_md.txs.len());
        assert_eq!(il_w_md.txs[0], InclusionListTxWithMetadata {
            hash: "0x80e000dd49cd0c518e8e426cf00daeebbae81ee79e0bb669601a329d1cafa6c2"
                .parse::<B256>()
                .unwrap(),
            nonce: 1,
            sender: "0x384b9b5024f0f40b9f595284b14746e1b271e181".parse::<Address>().unwrap(),
            gas_priority_fee: 9150163568,
            bytes: "0x02f87582426801850221646a70850221646a7082520894acabf6c2d38973a5f2ebab6b5e85623db1005a4e880ddf2f839aa3d97080c080a0088ae2635655e314949dae343ac296c3fb6ac56802e1024639f9603c61e253669f2bf33fe18ce70520abc6a662794c1ef5bb310248b7b7c4acc6be93e7885d62".parse::<Bytes>().unwrap()
        });
    }
}
