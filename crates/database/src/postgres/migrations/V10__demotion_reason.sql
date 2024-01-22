
ALTER TABLE demotions
ADD COLUMN "reason" varchar DEFAULT 'unknown',
ADD COLUMN "block_hash" bytea;

ALTER TABLE submission_trace
ADD COLUMN "optimistic_version" smallint DEFAULT 0;

ALTER TABLE pending_blocks
DROP COLUMN "pending",
ADD COLUMN "header_receive" timestamptz,
ADD COLUMN "payload_receive" timestamptz;