CREATE TABLE IF NOT EXISTS merged_blocks (
    slot BIGINT NOT NULL,
    block_number BIGINT NOT NULL,
    original_block_hash BYTEA NOT NULL,
    block_hash BYTEA PRIMARY KEY,
    original_value NUMERIC(78, 0) NOT NULL,
    merged_value NUMERIC(78, 0) NOT NULL,
    original_tx_count INTEGER NOT NULL,
    merged_tx_count INTEGER NOT NULL,
    original_blob_count INTEGER NOT NULL,
    merged_blob_count INTEGER NOT NULL,
    builder_inclusions VARCHAR NOT NULL,
    inserted_at TIMESTAMP NOT NULL
);

CREATE INDEX idx_merged_blocks_slot ON merged_blocks(slot);
CREATE INDEX idx_merged_blocks_block_number ON merged_blocks(block_number);