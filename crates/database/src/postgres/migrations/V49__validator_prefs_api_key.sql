ALTER TABLE validator_preferences
ADD COLUMN IF NOT EXISTS api_key text;

ALTER TABLE validator_registrations
ADD COLUMN IF NOT EXISTS ip_addr text;
