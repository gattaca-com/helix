ALTER TABLE validator_preferences
ADD COLUMN "disable_inclusion_lists" boolean NOT NULL DEFAULT false;

ALTER TABLE delivered_payload_preferences
ADD COLUMN "disable_inclusion_lists" boolean NOT NULL DEFAULT false;

CREATE TABLE "inclusion_list_txs" (
    "tx_hash" bytea NOT NULL PRIMARY KEY,
    "bytes" bytea NOT NULL,
    "nonce" integer NOT NULL,
    "gas_priority_fee" integer NOT NULL,
    "sender" bytea NOT NULL,
    "wait_time" BIGINT NOT NULL,
    "slot_included" integer NOT NULL
);

-- CREATE TABLE "inclusion_lists" (
--   "slot_n" integer NOT NULL PRIMARY KEY,
--   "tx_hash" bytea NOT NULL,
--   FOREIGN KEY (tx_hash) REFERENCES inclusion_list_txs(tx_hash)
-- );
