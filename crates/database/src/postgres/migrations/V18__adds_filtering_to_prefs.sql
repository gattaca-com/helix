ALTER TABLE validator_preferences
ADD COLUMN "filtering" smallint NOT NULL DEFAULT 0;

ALTER TABLE delivered_payload_preferences
ADD COLUMN "filtering" smallint NOT NULL DEFAULT 0;
