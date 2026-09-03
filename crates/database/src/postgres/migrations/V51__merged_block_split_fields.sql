-- Was V49, colliding with V49__validator_prefs_api_key; IF NOT EXISTS keeps it a no-op where the columns already landed.
ALTER TABLE merged_blocks ADD COLUMN IF NOT EXISTS base_builder_revenue NUMERIC(78, 0);
ALTER TABLE merged_blocks ADD COLUMN IF NOT EXISTS relay_revenue NUMERIC(78, 0);
ALTER TABLE merged_blocks ADD COLUMN IF NOT EXISTS original_gas_used BIGINT;
ALTER TABLE merged_blocks ADD COLUMN IF NOT EXISTS merged_gas_used BIGINT;
ALTER TABLE merged_blocks ADD COLUMN IF NOT EXISTS total_merged_value NUMERIC(78, 0);
ALTER TABLE merged_blocks ADD COLUMN IF NOT EXISTS was_top_builder BOOLEAN;
ALTER TABLE merged_blocks ADD COLUMN IF NOT EXISTS top_bid NUMERIC(78, 0);
