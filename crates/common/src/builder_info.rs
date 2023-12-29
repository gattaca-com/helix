use ethereum_consensus::{
    primitives::U256,
    serde::as_str,
};


#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default, Eq, PartialEq)]
pub struct BuilderInfo {
    #[serde(with = "as_str")]
    pub collateral: U256,
    pub is_optimistic: bool,
}
