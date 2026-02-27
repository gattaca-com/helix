CREATE TABLE "pending_blocks" (
  "block_hash" bytea PRIMARY KEY,
  "builder_pubkey" bytea,
  "slot" integer,
  "pending" boolean,
  "created_at" timestamptz DEFAULT (now())
);