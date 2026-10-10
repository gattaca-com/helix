-- v4 optimistic spec: demotions charge a pool and carry the offending bid value.
ALTER TABLE demotions
ADD COLUMN IF NOT EXISTS "collateral_id" varchar,
ADD COLUMN IF NOT EXISTS "bid_value_wei" numeric(78);

-- Retention is `report.ts_ms >= promotion.ts_ms`, so the promotion timestamp must survive
-- restart. Without it the retained set cannot be derived from the append-only demotion history.
CREATE TABLE IF NOT EXISTS promotions (
  "collateral_id" varchar PRIMARY KEY,
  "promotion_time" bigint NOT NULL,
  "slot_number" integer,
  "public_key" bytea
);

CREATE INDEX IF NOT EXISTS demotions_collateral_id_time
ON demotions ("collateral_id", "demotion_time");
