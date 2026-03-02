ALTER TABLE validator_preferences
ADD COLUMN IF NOT EXISTS "disable_inclusion_lists" boolean NOT NULL DEFAULT false;

ALTER TABLE delivered_payload_preferences
ADD COLUMN IF NOT EXISTS "disable_inclusion_lists" boolean NOT NULL DEFAULT false;

ALTER TABLE slot_preferences
ADD COLUMN IF NOT EXISTS "disable_inclusion_lists" boolean NOT NULL DEFAULT false;

CREATE TABLE IF NOT EXISTS "inclusion_list_txs" (
    "tx_hash" bytea NOT NULL PRIMARY KEY,
    "bytes" bytea NOT NULL,
    "slot" bigint NOT NULL,
    "block_parent_hash" bytea NOT NULL,
    "proposer_pubkey" bytea NOT NULL
);
