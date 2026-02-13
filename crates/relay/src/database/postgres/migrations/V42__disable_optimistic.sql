ALTER TABLE validator_preferences
ADD COLUMN IF NOT EXISTS "disable_optimistic" boolean NOT NULL DEFAULT false;

ALTER TABLE delivered_payload_preferences
ADD COLUMN IF NOT EXISTS "disable_optimistic" boolean NOT NULL DEFAULT false;

ALTER TABLE slot_preferences
ADD COLUMN IF NOT EXISTS "disable_optimistic" boolean NOT NULL DEFAULT false;
