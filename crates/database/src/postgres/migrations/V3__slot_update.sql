-- drop the slot table and create updated table
DROP TABLE IF EXISTS slot;

CREATE TABLE slot (
  "number" integer PRIMARY KEY,
  "epoch" integer,
  "timestamp" timestamptz,
  "block_number" integer,
  "missed" boolean,
  "proposer_index" integer,
  "block_hash" bytea,
  "fee_recipient"  bytea
);

CREATE INDEX IF NOT EXISTS "slot_timestamp" ON "slot" ("timestamp");
CREATE INDEX IF NOT EXISTS "slot_block_number" ON "slot" ("block_number");
