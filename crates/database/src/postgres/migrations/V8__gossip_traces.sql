CREATE TABLE "header_submission" (
  "block_hash" bytea PRIMARY KEY,
  "timestamp" bigint,
  "slot_number" integer,
  "block_number" integer,
  "parent_hash" bytea,
  "builder_pubkey" bytea,
  "proposer_pubkey" bytea,
  "proposer_fee_recipient" bytea,
  "gas_limit" integer,
  "gas_used" integer,
  "value" numeric(78),
  "created_at" timestamptz DEFAULT (now())
);

CREATE TABLE "header_submission_trace" (
  "block_hash" bytea,
  "region_id" smallint,
  "receive" bigint,
  "decode" bigint,
  "pre_checks" bigint,
  "signature" bigint,
  "floor_bid_checks" bigint,
  "auctioneer_update" bigint,
  "request_finish" bigint
);

CREATE TABLE "gossiped_header_trace" (
    "block_hash" bytea,
    "region_id" smallint,
    "on_receive" bigint,
    "on_gossip_receive" bigint,
    "pre_checks" bigint,
    "auctioneer_update" bigint
);

CREATE TABLE "gossiped_payload_trace" (
    "block_hash" bytea,
    "region_id" smallint,
    "receive" bigint,
    "pre_checks" bigint,
    "auctioneer_update" bigint
);

ALTER TABLE "header_submission_trace" ADD FOREIGN KEY ("region_id") REFERENCES "region" ("id");
ALTER TABLE "gossiped_header_trace" ADD FOREIGN KEY ("region_id") REFERENCES "region" ("id");
ALTER TABLE "gossiped_payload_trace" ADD FOREIGN KEY ("region_id") REFERENCES "region" ("id");

SELECT create_hypertable('header_submission_trace', 'receive', chunk_time_interval => 86400000000000, if_not_exists => TRUE);
SELECT create_hypertable('gossiped_header_trace', 'on_gossip_receive', chunk_time_interval => 86400000000000, if_not_exists => TRUE);
SELECT create_hypertable('gossiped_payload_trace', 'receive', chunk_time_interval => 86400000000000, if_not_exists => TRUE);