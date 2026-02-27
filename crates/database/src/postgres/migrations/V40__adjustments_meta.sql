
CREATE INDEX IF NOT EXISTS "bid_adjustments_submitted_block_hash" ON "bid_adjustments" ("submitted_block_hash");
CREATE INDEX IF NOT EXISTS "bid_adjustments_adjusted_block_hash" ON "bid_adjustments" ("adjusted_block_hash");
CREATE INDEX IF NOT EXISTS "bid_adjustments_submitted_received_at" ON "bid_adjustments" ("submitted_received_at");

ALTER TABLE block_submission 
ADD COLUMN IF NOT EXISTS is_adjusted boolean;
