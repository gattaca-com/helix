ALTER TABLE late_payload
DROP CONSTRAINT late_payload_pkey,
DROP COLUMN id,
ADD COLUMN proposer_pubkey bytea;
