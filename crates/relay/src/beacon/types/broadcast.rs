use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub enum BroadcastValidation {
    #[default]
    Gossip,
    Consensus,
    ConsensusAndEquivocation,
}

impl std::fmt::Display for BroadcastValidation {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        let printable = match self {
            Self::Gossip => "gossip",
            Self::Consensus => "consensus",
            Self::ConsensusAndEquivocation => "consensus_and_equivocation",
        };
        write!(f, "{printable}")
    }
}
