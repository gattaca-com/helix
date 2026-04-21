use alloy_primitives::{B256, hex};
use helix_types::{Slot, Withdrawals};
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Clone, Debug)]
pub enum StateId {
    Head,
    Genesis,
    Finalized,
    Justified,
    Slot(Slot),
    Root(B256),
}

impl std::fmt::Display for StateId {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        let printable = match *self {
            StateId::Finalized => "finalized",
            StateId::Justified => "justified",
            StateId::Head => "head",
            StateId::Genesis => "genesis",
            StateId::Slot(slot) => return write!(f, "{slot}"),
            StateId::Root(root) => return write!(f, "{root}"),
        };
        write!(f, "{printable}")
    }
}

impl std::str::FromStr for StateId {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "finalized" => Ok(StateId::Finalized),
            "justified" => Ok(StateId::Justified),
            "head" => Ok(StateId::Head),
            "genesis" => Ok(StateId::Genesis),
            _ => match s.parse::<Slot>() {
                Ok(slot) => Ok(Self::Slot(slot)),
                Err(_) => match hex::decode(s) {
                    Ok(root_data) => {
                        let root = B256::try_from(root_data.as_slice()).map_err(|err| {
                            format!(
                                "could not parse state identifier by root from the provided argument {s}: {err}"
                            )
                        })?;
                        Ok(Self::Root(root))
                    }
                    Err(err) => Err(format!(
                        "could not parse state identifier by root from the provided argument {s}: {err}"
                    )),
                },
            },
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct SyncStatus {
    pub head_slot: Slot,
    #[serde(with = "serde_utils::quoted_u64")]
    pub sync_distance: u64,
    pub is_syncing: bool,
}

#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct HeadEventData {
    pub slot: Slot,
    pub block: B256,
    pub state: String,
}

#[derive(Debug, Serialize, Deserialize, Clone, Default)]
pub struct PayloadAttributesEvent {
    pub version: String,
    pub data: PayloadAttributesEventData,
}

#[derive(Debug, Serialize, Deserialize, Clone, Default)]
pub struct PayloadAttributesEventData {
    #[serde(with = "serde_utils::quoted_u64")]
    pub proposer_index: u64,
    pub proposal_slot: Slot,
    #[serde(with = "serde_utils::quoted_u64")]
    pub parent_block_number: u64,
    pub parent_block_root: String,
    pub parent_block_hash: B256,
    pub payload_attributes: PayloadAttributes,
}

#[derive(Debug, Serialize, Deserialize, Clone, Default)]
pub struct PayloadAttributes {
    #[serde(with = "serde_utils::quoted_u64")]
    pub timestamp: u64,
    pub prev_randao: B256,
    pub suggested_fee_recipient: String,
    pub withdrawals: Withdrawals,
    pub parent_beacon_block_root: Option<B256>,
}
