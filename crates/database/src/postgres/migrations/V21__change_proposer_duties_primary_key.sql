BEGIN;

-- Ensure there are no duplicate slot_number values before making it a primary key
SELECT COUNT(*), slot_number
FROM proposer_duties
GROUP BY slot_number
HAVING COUNT(*) > 1;

-- If the above query returns any rows, handle the duplicates before continuing
-- with the migration. If no duplicates, proceed:

-- Drop the existing primary key constraint on public_key
ALTER TABLE proposer_duties
DROP CONSTRAINT proposer_duties_pkey;

-- Add a new primary key constraint on slot_number
ALTER TABLE proposer_duties
ADD PRIMARY KEY (slot_number);

COMMIT;
