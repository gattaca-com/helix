CREATE TABLE IF NOT EXISTS relay_info (
    "version" integer PRIMARY KEY,
    "adjustments_enabled" boolean NOT NULL DEFAULT true
);

INSERT INTO relay_info ("version")
VALUES (1)
ON CONFLICT ("version") DO NOTHING;

ALTER TABLE "bid_adjustments" 
    ADD COLUMN IF NOT EXISTS is_dry_run boolean;
