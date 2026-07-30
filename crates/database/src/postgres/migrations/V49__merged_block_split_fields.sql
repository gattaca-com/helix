ALTER TABLE merged_blocks ADD COLUMN base_builder_revenue NUMERIC(78, 0);
ALTER TABLE merged_blocks ADD COLUMN relay_revenue NUMERIC(78, 0);
ALTER TABLE merged_blocks ADD COLUMN original_gas_used BIGINT;
ALTER TABLE merged_blocks ADD COLUMN merged_gas_used BIGINT;
ALTER TABLE merged_blocks ADD COLUMN total_merged_value NUMERIC(78, 0);
ALTER TABLE merged_blocks ADD COLUMN was_top_builder BOOLEAN;
ALTER TABLE merged_blocks ADD COLUMN top_bid NUMERIC(78, 0);
