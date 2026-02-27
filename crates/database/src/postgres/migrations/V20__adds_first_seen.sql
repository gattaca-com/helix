ALTER TABLE block_submission
ADD COLUMN "first_seen" bigint DEFAULT 4102444800000000000;

ALTER TABLE header_submission
ADD COLUMN "first_seen" bigint DEFAULT 4102444800000000000;
