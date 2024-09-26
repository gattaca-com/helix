use ethereum_consensus::{ssz, ssz::prelude::*, types::mainnet::SignedBeaconBlock, Fork};

use crate::deneb::SignedBlockContents;

#[derive(Debug, Clone, PartialEq, Eq, Serializable, HashTreeRoot, serde::Serialize)]
#[ssz(transparent)]
#[serde(untagged)]
pub enum VersionedSignedProposal {
    Bellatrix(SignedBeaconBlock),
    Capella(SignedBeaconBlock),
    Deneb(SignedBlockContents),
}

impl Default for VersionedSignedProposal {
    fn default() -> Self {
        Self::Capella(SignedBeaconBlock::Capella(Default::default()))
    }
}

impl VersionedSignedProposal {
    pub fn version(&self) -> Fork {
        match self {
            Self::Bellatrix(block) => block.version(),
            Self::Capella(block) => block.version(),
            Self::Deneb(block_contents) => block_contents.signed_block.version(),
        }
    }

    pub fn beacon_block(&self) -> &SignedBeaconBlock {
        match self {
            Self::Bellatrix(block) => block,
            Self::Capella(block) => block,
            Self::Deneb(block_contents) => &block_contents.signed_block,
        }
    }

    pub fn block_contents(&self) -> &SignedBlockContents {
        match self {
            Self::Bellatrix(_) => unreachable!("VersionedSignedProposal::Bellatrix is not supported in block_contents"),
            Self::Capella(_) => {
                unreachable!("VersionedSignedProposal::Capella is not supported in block_contents")
            }
            Self::Deneb(block_contents) => block_contents,
        }
    }

    pub fn get_ssz_bytes_to_publish(&self) -> Result<Vec<u8>, SerializeError> {
        match self {
            Self::Bellatrix(block) => ssz::prelude::serialize(block),
            Self::Capella(block) => ssz::prelude::serialize(block),
            Self::Deneb(block_contents) => ssz::prelude::serialize(&block_contents.signed_block),
        }
    }
}

impl<'de> serde::Deserialize<'de> for VersionedSignedProposal {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = serde_json::Value::deserialize(deserializer)?;
        if let Ok(inner) = <_ as serde::Deserialize>::deserialize(&value) {
            return Ok(Self::Deneb(inner))
        }
        if let Ok(inner) = <_ as serde::Deserialize>::deserialize(&value) {
            return Ok(Self::Capella(inner))
        }
        if let Ok(inner) = <_ as serde::Deserialize>::deserialize(&value) {
            return Ok(Self::Bellatrix(inner))
        }
        Err(serde::de::Error::custom("no variant could be deserialized from input"))
    }
}
