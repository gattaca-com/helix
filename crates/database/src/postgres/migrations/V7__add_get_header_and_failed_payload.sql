
CREATE TABLE "failed_payload" (
    "region_id" smallint,
    "block_hash" bytea,
    "error" varchar,
    "created_at" timestamptz DEFAULT (now())
);

CREATE TABLE "get_header" (
    "slot_number" integer,
    "region_id" smallint,
    "parent_hash" bytea,
    "proposer_pubkey" bytea,
    "block_hash" bytea,
    "created_at" timestamptz DEFAULT (now())
);

CREATE TABLE "get_header_trace" (
    "block_hash" bytea,
    "region_id" smallint,
    "receive" bigint,
    "validation_complete" bigint,
    "best_bid_fetched" bigint
);

ALTER TABLE "get_header_trace" ADD FOREIGN KEY ("region_id") REFERENCES "region" ("id");

SELECT create_hypertable('get_header_trace', 'receive', chunk_time_interval => 86400000000000, if_not_exists => TRUE);
