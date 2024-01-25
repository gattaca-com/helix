CREATE TABLE "trusted_proposers" (
  "pub_key" bytea PRIMARY KEY,
  "name" varchar,
  "created_at" timestamptz DEFAULT (now())
);