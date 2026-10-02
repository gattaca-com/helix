CREATE TABLE IF NOT EXISTS merged_txs (
    slot BIGINT NOT NULL,
    tx_hash BYTEA NOT NULL,
    block_hash BYTEA NOT NULL,
    base_block_hash BYTEA NOT NULL,
    reason TEXT,
    inserted_at TIMESTAMP NOT NULL DEFAULT NOW(),
    PRIMARY KEY (slot, tx_hash)
);
