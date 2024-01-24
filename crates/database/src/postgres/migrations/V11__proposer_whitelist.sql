CREATE TABLE "proposer_whitelist" (
  "pub_key" bytea PRIMARY KEY,
  "name" varchar,
  "created_at" timestamptz DEFAULT (now())
);