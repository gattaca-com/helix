#[derive(Clone, Copy, Default, Debug, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
#[repr(i16)]
pub enum OptimisticVersion {
    #[default]
    NotOptimistic = 0,
    V1 = 1,
    // V2 = 2,
    // V3 = 3,
}

impl OptimisticVersion {
    pub fn is_optimistic(&self) -> bool {
        matches!(self, Self::V1)
    }
}
