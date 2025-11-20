ALTER TABLE delivered_payload
ADD COLUMN IF NOT EXISTS slot_number integer,
ADD COLUMN IF NOT EXISTS builder_pubkey bytea,
ADD COLUMN IF NOT EXISTS proposer_pubkey bytea,
ADD COLUMN IF NOT EXISTS proposer_fee_recipient bytea,
ADD COLUMN IF NOT EXISTS value numeric(78, 0),
ADD COLUMN IF NOT EXISTS num_txs integer,
ADD COLUMN IF NOT EXISTS filtering smallint;

CREATE INDEX IF NOT EXISTS idx_delivered_payload_slot_number ON delivered_payload(slot_number);
CREATE INDEX IF NOT EXISTS idx_delivered_payload_builder_pubkey ON delivered_payload(builder_pubkey);
CREATE INDEX IF NOT EXISTS idx_delivered_payload_proposer_pubkey ON delivered_payload(proposer_pubkey);
CREATE INDEX IF NOT EXISTS idx_delivered_payload_filtering ON delivered_payload(filtering);
CREATE INDEX IF NOT EXISTS idx_delivered_payload_slot_filtering ON delivered_payload(slot_number, filtering);
