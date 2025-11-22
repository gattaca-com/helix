use alloy_primitives::U256;

#[derive(Clone, serde::Serialize, serde::Deserialize, Default, Eq, PartialEq)]
pub struct BuilderInfo {
    #[serde(with = "serde_utils::quoted_u256")]
    pub collateral: U256,
    pub is_optimistic: bool,
    /// Whether the builder is optimistic for regional filtering.
    #[serde(default)]
    pub is_optimistic_for_regional_filtering: bool,
    pub builder_id: Option<String>,
    pub builder_ids: Option<Vec<String>>,
    pub api_key: Option<String>,
}

impl BuilderInfo {
    pub fn can_process_regional_slot_optimistically(&self) -> bool {
        self.is_optimistic && self.is_optimistic_for_regional_filtering
    }

    pub fn builder_id(&self) -> &str {
        self.builder_id.as_deref().unwrap_or("")
    }
}

impl core::fmt::Debug for BuilderInfo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "BuilderInfo {{ collateral: {:?}, is_optimistic: {:?}, is_optimistic_for_regional_filtering: {:?}, builder_id: {:?}, builder_ids: {:?}, api_key: *** }}",
            self.collateral,
            self.is_optimistic,
            self.is_optimistic_for_regional_filtering,
            self.builder_id,
            self.builder_ids,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_builder_info_default() {
        let builder_info = BuilderInfo::default();
        assert_eq!(builder_info.collateral, U256::ZERO);
        assert!(!builder_info.is_optimistic);
        assert!(!builder_info.is_optimistic_for_regional_filtering);
        assert_eq!(builder_info.builder_id, None);
        assert_eq!(builder_info.builder_ids, None);
        assert_eq!(builder_info.api_key, None);
    }

    #[test]
    fn test_can_process_regional_slot_optimistically_both_true() {
        let builder_info = BuilderInfo {
            collateral: U256::from(1000),
            is_optimistic: true,
            is_optimistic_for_regional_filtering: true,
            builder_id: Some("test_builder".to_string()),
            builder_ids: None,
            api_key: None,
        };
        assert!(builder_info.can_process_regional_slot_optimistically());
    }

    #[test]
    fn test_can_process_regional_slot_optimistically_only_optimistic() {
        let builder_info = BuilderInfo {
            collateral: U256::from(1000),
            is_optimistic: true,
            is_optimistic_for_regional_filtering: false,
            builder_id: Some("test_builder".to_string()),
            builder_ids: None,
            api_key: None,
        };
        assert!(!builder_info.can_process_regional_slot_optimistically());
    }

    #[test]
    fn test_can_process_regional_slot_optimistically_only_regional() {
        let builder_info = BuilderInfo {
            collateral: U256::from(1000),
            is_optimistic: false,
            is_optimistic_for_regional_filtering: true,
            builder_id: Some("test_builder".to_string()),
            builder_ids: None,
            api_key: None,
        };
        assert!(!builder_info.can_process_regional_slot_optimistically());
    }

    #[test]
    fn test_can_process_regional_slot_optimistically_both_false() {
        let builder_info = BuilderInfo {
            collateral: U256::from(1000),
            is_optimistic: false,
            is_optimistic_for_regional_filtering: false,
            builder_id: Some("test_builder".to_string()),
            builder_ids: None,
            api_key: None,
        };
        assert!(!builder_info.can_process_regional_slot_optimistically());
    }

    #[test]
    fn test_builder_id_with_some() {
        let builder_info = BuilderInfo {
            collateral: U256::from(1000),
            is_optimistic: true,
            is_optimistic_for_regional_filtering: true,
            builder_id: Some("test_builder_123".to_string()),
            builder_ids: None,
            api_key: None,
        };
        assert_eq!(builder_info.builder_id(), "test_builder_123");
    }

    #[test]
    fn test_builder_id_with_none() {
        let builder_info = BuilderInfo {
            collateral: U256::from(1000),
            is_optimistic: true,
            is_optimistic_for_regional_filtering: true,
            builder_id: None,
            builder_ids: None,
            api_key: None,
        };
        assert_eq!(builder_info.builder_id(), "");
    }

    #[test]
    fn test_builder_info_serialization() {
        let builder_info = BuilderInfo {
            collateral: U256::from(5000),
            is_optimistic: true,
            is_optimistic_for_regional_filtering: false,
            builder_id: Some("builder_1".to_string()),
            builder_ids: Some(vec!["id1".to_string(), "id2".to_string()]),
            api_key: Some("secret_key".to_string()),
        };

        let serialized = serde_json::to_string(&builder_info).unwrap();
        let deserialized: BuilderInfo = serde_json::from_str(&serialized).unwrap();
        
        assert_eq!(builder_info, deserialized);
    }

    #[test]
    fn test_builder_info_debug_hides_api_key() {
        let builder_info = BuilderInfo {
            collateral: U256::from(1000),
            is_optimistic: true,
            is_optimistic_for_regional_filtering: true,
            builder_id: Some("test_builder".to_string()),
            builder_ids: None,
            api_key: Some("super_secret_key".to_string()),
        };

        let debug_str = format!("{:?}", builder_info);
        assert!(!debug_str.contains("super_secret_key"));
        assert!(debug_str.contains("***"));
    }

    #[test]
    fn test_builder_info_with_multiple_builder_ids() {
        let builder_info = BuilderInfo {
            collateral: U256::from(2000),
            is_optimistic: false,
            is_optimistic_for_regional_filtering: false,
            builder_id: Some("main_builder".to_string()),
            builder_ids: Some(vec![
                "builder_1".to_string(),
                "builder_2".to_string(),
                "builder_3".to_string(),
            ]),
            api_key: Some("key123".to_string()),
        };

        assert_eq!(builder_info.builder_id(), "main_builder");
        assert_eq!(builder_info.builder_ids.as_ref().unwrap().len(), 3);
    }
}
