ALTER TABLE block_submission DROP CONSTRAINT IF EXISTS block_submission_pkey;

ALTER TABLE block_submission 
ADD COLUMN IF NOT EXISTS region_id smallint,
ADD COLUMN IF NOT EXISTS optimistic_version smallint,
ADD COLUMN IF NOT EXISTS metadata character varying;

ALTER TABLE get_header
ADD COLUMN IF NOT EXISTS receive bigint,
ADD COLUMN IF NOT EXISTS validation_complete bigint,
ADD COLUMN IF NOT EXISTS best_bid_fetched bigint;
