ALTER TABLE validator_registrations
ADD COLUMN "user_agent" varchar;

ALTER TABLE delivered_payload
ADD COLUMN "user_agent" varchar;