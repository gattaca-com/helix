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
    fn test_builder_info_default_is_safe() {
        let builder_info = BuilderInfo::default();
        
        // Default should be conservative (not optimistic)
        assert_eq!(builder_info.collateral, U256::ZERO, "Default collateral should be zero");
        assert!(!builder_info.is_optimistic, "Should not be optimistic by default");
        assert!(!builder_info.is_optimistic_for_regional_filtering, "Should not allow regional filtering by default");
        assert!(!builder_info.can_process_regional_slot_optimistically(), "Default should not process regional slots optimistically");
        
        // No identifiers by default
        assert_eq!(builder_info.builder_id, None);
        assert_eq!(builder_info.builder_ids, None);
        assert_eq!(builder_info.builder_id(), "", "builder_id() should return empty string when None");
        assert_eq!(builder_info.api_key, None);
    }

    #[test]
    fn test_regional_optimistic_requires_both_flags() {
        // BOTH flags must be true - this is critical for safety
        let both_true = BuilderInfo {
            collateral: U256::from(1000),
            is_optimistic: true,
            is_optimistic_for_regional_filtering: true,
            ..Default::default()
        };
        assert!(both_true.can_process_regional_slot_optimistically(),
                "Should process optimistically when BOTH flags are true");

        // Only is_optimistic = true (UNSAFE for regional)
        let only_optimistic = BuilderInfo {
            is_optimistic: true,
            is_optimistic_for_regional_filtering: false,
            ..Default::default()
        };
        assert!(!only_optimistic.can_process_regional_slot_optimistically(),
                "Should NOT process optimistically without regional flag");

        // Only regional flag = true (UNSAFE without general optimistic)
        let only_regional = BuilderInfo {
            is_optimistic: false,
            is_optimistic_for_regional_filtering: true,
            ..Default::default()
        };
        assert!(!only_regional.can_process_regional_slot_optimistically(),
                "Should NOT process optimistically without is_optimistic flag");

        // Both false (conservative default)
        let both_false = BuilderInfo::default();
        assert!(!both_false.can_process_regional_slot_optimistically(),
                "Default (both false) should not process optimistically");
    }

    #[test]
    fn test_regional_optimistic_ignores_collateral_amount() {
        // The method only checks boolean flags, not collateral amount
        // This tests actual implementation behavior
        let zero_collateral = BuilderInfo {
            collateral: U256::ZERO,
            is_optimistic: true,
            is_optimistic_for_regional_filtering: true,
            ..Default::default()
        };
        assert!(zero_collateral.can_process_regional_slot_optimistically(),
                "Regional optimistic check doesn't depend on collateral");

        let max_collateral = BuilderInfo {
            collateral: U256::MAX,
            is_optimistic: true,
            is_optimistic_for_regional_filtering: true,
            ..Default::default()
        };
        assert!(max_collateral.can_process_regional_slot_optimistically(),
                "Works with max collateral too");
    }

    #[test]
    fn test_builder_id_accessor_edge_cases() {
        // Normal case
        let normal = BuilderInfo {
            builder_id: Some("builder_123".to_string()),
            ..Default::default()
        };
        assert_eq!(normal.builder_id(), "builder_123");

        // None returns empty string (not panic)
        let none = BuilderInfo {
            builder_id: None,
            ..Default::default()
        };
        assert_eq!(none.builder_id(), "", "None should return empty string");

        // Empty string (different from None)
        let empty_string = BuilderInfo {
            builder_id: Some(String::new()),
            ..Default::default()
        };
        assert_eq!(empty_string.builder_id(), "", "Empty Some() should return empty string");

        // Very long ID
        let long_id = "a".repeat(1000);
        let long = BuilderInfo {
            builder_id: Some(long_id.clone()),
            ..Default::default()
        };
        assert_eq!(long.builder_id(), &long_id, "Should handle long IDs");

        // Special characters and Unicode
        let special = BuilderInfo {
            builder_id: Some("builder-123_TEST!@#$%^&*()🚀".to_string()),
            ..Default::default()
        };
        assert_eq!(special.builder_id(), "builder-123_TEST!@#$%^&*()🚀", "Should preserve special characters");
    }

    #[test]
    fn test_serialization_round_trip() {
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
        
        assert_eq!(builder_info, deserialized, "Round trip should preserve all fields");
    }

    #[test]
    fn test_serialization_with_missing_optional_fields() {
        // Test that missing fields deserialize to None/defaults
        let json = r#"{"collateral":"1000","is_optimistic":true}"#;
        let deserialized: BuilderInfo = serde_json::from_str(json).unwrap();
        
        assert_eq!(deserialized.collateral, U256::from(1000));
        assert!(deserialized.is_optimistic);
        assert!(!deserialized.is_optimistic_for_regional_filtering, "Should default to false with #[serde(default)]");
        assert_eq!(deserialized.builder_id, None);
        assert_eq!(deserialized.builder_ids, None);
        assert_eq!(deserialized.api_key, None);
    }

    #[test]
    fn test_collateral_serialization_as_quoted_string() {
        // U256 should serialize as quoted string (not number) due to JavaScript number limits
        let large_collateral = BuilderInfo {
            collateral: U256::from_str_radix("1000000000000000000000000", 10).unwrap(), // > JavaScript MAX_SAFE_INTEGER
            ..Default::default()
        };
        
        let json = serde_json::to_string(&large_collateral).unwrap();
        assert!(json.contains("\"collateral\":\""), "Collateral should be quoted string");
        
        // Should round-trip correctly
        let deserialized: BuilderInfo = serde_json::from_str(&json).unwrap();
        assert_eq!(large_collateral.collateral, deserialized.collateral);
    }

    #[test]
    fn test_serialization_edge_cases() {
        // Max U256
        let max_u256 = BuilderInfo {
            collateral: U256::MAX,
            ..Default::default()
        };
        let json = serde_json::to_string(&max_u256).unwrap();
        let deserialized: BuilderInfo = serde_json::from_str(&json).unwrap();
        assert_eq!(max_u256.collateral, deserialized.collateral, "Should handle U256::MAX");

        // Empty vectors vs None
        let empty_vec = BuilderInfo {
            builder_ids: Some(vec![]),
            ..Default::default()
        };
        let json = serde_json::to_string(&empty_vec).unwrap();
        let deserialized: BuilderInfo = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.builder_ids, Some(vec![]), "Empty vec should not become None");
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
    fn test_builder_id_vs_builder_ids_relationship() {
        // Both fields can coexist - builder_id() only returns single ID
        let both = BuilderInfo {
            builder_id: Some("primary".to_string()),
            builder_ids: Some(vec![
                "secondary_1".to_string(),
                "secondary_2".to_string(),
            ]),
            ..Default::default()
        };
        assert_eq!(both.builder_id(), "primary", "builder_id() returns single ID");
        assert_eq!(both.builder_ids.as_ref().unwrap().len(), 2, "builder_ids separate from builder_id");

        // builder_id() doesn't use builder_ids as fallback
        let only_ids = BuilderInfo {
            builder_id: None,
            builder_ids: Some(vec!["fallback".to_string()]),
            ..Default::default()
        };
        assert_eq!(only_ids.builder_id(), "", "builder_id() doesn't fallback to builder_ids");

        // Empty builder_ids vec
        let empty_ids = BuilderInfo {
            builder_ids: Some(vec![]),
            ..Default::default()
        };
        assert_eq!(empty_ids.builder_ids.as_ref().unwrap().len(), 0);

        // Many IDs (test vec capacity)
        let many_ids: Vec<String> = (0..1000).map(|i| format!("builder_{}", i)).collect();
        let many = BuilderInfo {
            builder_ids: Some(many_ids.clone()),
            ..Default::default()
        };
        assert_eq!(many.builder_ids.as_ref().unwrap().len(), 1000, "Should handle many IDs");
    }

    #[test]
    fn test_collateral_edge_cases() {
        // Zero collateral (valid)
        let zero = BuilderInfo {
            collateral: U256::ZERO,
            ..Default::default()
        };
        assert_eq!(zero.collateral, U256::ZERO);

        // One wei
        let one = BuilderInfo {
            collateral: U256::from(1),
            ..Default::default()
        };
        assert_eq!(one.collateral, U256::from(1));

        // Maximum U256
        let max = BuilderInfo {
            collateral: U256::MAX,
            ..Default::default()
        };
        assert_eq!(max.collateral, U256::MAX);

        // Large realistic value (e.g., 1000 ETH in wei)
        let realistic = U256::from_str_radix("1000000000000000000000", 10).unwrap(); // 1000 ETH
        let large = BuilderInfo {
            collateral: realistic,
            ..Default::default()
        };
        assert_eq!(large.collateral, realistic);
    }
}
