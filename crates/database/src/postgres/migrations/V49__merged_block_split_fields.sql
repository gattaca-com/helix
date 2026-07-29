ALTER TABLE merged_blocks ADD COLUMN base_builder_revenue NUMERIC(78, 0) NOT NULL DEFAULT 0;
ALTER TABLE merged_blocks ADD COLUMN relay_revenue NUMERIC(78, 0) NOT NULL DEFAULT 0;
ALTER TABLE merged_blocks ADD COLUMN original_gas_used BIGINT NOT NULL DEFAULT 0;
ALTER TABLE merged_blocks ADD COLUMN merged_gas_used BIGINT NOT NULL DEFAULT 0;
ALTER TABLE merged_blocks ADD COLUMN total_merged_value NUMERIC(78, 0) NOT NULL DEFAULT 0;
