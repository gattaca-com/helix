
CREATE TABLE proposer_duties_archive (
  "public_key" bytea,
  "slot_number" integer PRIMARY KEY,
  "validator_index" integer
);

CREATE INDEX IF NOT EXISTS "submission_trace_block_hash" ON "submission_trace" ("block_hash");