
CREATE TABLE proposer_duties_archive AS TABLE proposer_duties WITH NO DATA;

CREATE INDEX IF NOT EXISTS "submission_trace_block_hash" ON "submission_trace" ("block_hash");