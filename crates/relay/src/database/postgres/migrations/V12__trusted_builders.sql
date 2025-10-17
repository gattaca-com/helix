ALTER TABLE builder_info
ADD COLUMN "builder_id" varchar;

ALTER TABLE validator_preferences
ADD COLUMN "trusted_builders" varchar[];