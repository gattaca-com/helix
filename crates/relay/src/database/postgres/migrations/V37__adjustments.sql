CREATE TABLE IF NOT EXISTS "bid_adjustments" (
    "slot" bigint NOT NULL,
    "builder_pubkey" bytea NOT NULL,
    "block_number" bigint NOT NULL,
    "delta" numeric(78) NOT NULL,
    "submitted_block_hash" bytea NOT NULL,
    "submitted_received_at" varchar NOT NULL,
    "submitted_value" numeric(78) NOT NULL,
    "adjusted_block_hash" bytea NOT NULL,
    "adjusted_value" numeric(78) NOT NULL
);

CREATE INDEX IF NOT EXISTS "bid_adjustments_slot" ON "bid_adjustments" ("slot");
