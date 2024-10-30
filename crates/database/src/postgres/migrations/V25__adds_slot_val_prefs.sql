CREATE TABLE slot_preferences (
  "slot_number" integer PRIMARY KEY,
  "proposer_pubkey" bytea,
  "filtering" smallint,
  "trusted_builders" varchar[],
  "header_delay" boolean,
  "gossip_blobs" boolean,
);