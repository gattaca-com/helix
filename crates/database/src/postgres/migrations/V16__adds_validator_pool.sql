CREATE TABLE validator_pools (
  "api_key" varchar PRIMARY KEY,
  "name" varchar
);

ALTER TABLE validator_preferences
ADD COLUMN "header_delay" boolean;
