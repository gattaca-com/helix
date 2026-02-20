ALTER TABLE delivered_payload
ADD COLUMN IF NOT EXISTS region_id smallint REFERENCES region(id);
