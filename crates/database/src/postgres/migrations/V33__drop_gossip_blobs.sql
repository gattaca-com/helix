ALTER TABLE validator_preferences
DROP COLUMN IF EXISTS "gossip_blobs";

ALTER TABLE slot_preferences
DROP COLUMN IF EXISTS "gossip_blobs";