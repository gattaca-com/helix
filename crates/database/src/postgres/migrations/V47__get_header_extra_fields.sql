ALTER TABLE get_header
ADD COLUMN IF NOT EXISTS builder_pubkey bytea,
ADD COLUMN IF NOT EXISTS proposer_fee_recipient bytea,
ADD COLUMN IF NOT EXISTS block_number bigint,
ADD COLUMN IF NOT EXISTS extra_data bytea;
