ALTER TABLE bid_adjustments
    ADD COLUMN IF NOT EXISTS metadata JSONB;
