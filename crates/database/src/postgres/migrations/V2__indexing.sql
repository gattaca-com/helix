
-- create indexes
CREATE INDEX IF NOT EXISTS "block_submission_block_hash" ON "block_submission" ("block_hash");
CREATE INDEX IF NOT EXISTS "block_submission_slot_number" ON "block_submission" ("slot_number");
CREATE INDEX IF NOT EXISTS "block_submission_block_number" ON "block_submission" ("block_number");

CREATE INDEX IF NOT EXISTS "delivered_payload_block_hash" ON "delivered_payload" ("block_hash");
CREATE INDEX IF NOT EXISTS "delivered_payload_block_number" ON "delivered_payload" ("block_number");

CREATE INDEX IF NOT EXISTS "slot_block_number" ON "slot" ("block_number");
CREATE INDEX IF NOT EXISTS "slot_number" ON "slot" ("number");

CREATE INDEX IF NOT EXISTS "transaction_block_hash" ON "transaction" ("block_hash");

CREATE INDEX IF NOT EXISTS "withdrawal_block_hash" ON "withdrawal" ("block_hash");

CREATE INDEX IF NOT EXISTS "late_payload_block_hash" ON "late_payload" ("block_hash");
CREATE INDEX IF NOT EXISTS "late_payload_slot_number" ON "late_payload" ("slot_number");
