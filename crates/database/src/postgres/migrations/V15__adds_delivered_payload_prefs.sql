CREATE TABLE delivered_payload_preferences (
  "block_hash" bytea PRIMARY KEY,
  "censoring" boolean,
  "trusted_builders" varchar[]
);