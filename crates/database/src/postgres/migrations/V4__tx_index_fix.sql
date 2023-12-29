-- Drop the existing unique constraint if it exists
ALTER TABLE "transaction" DROP CONSTRAINT IF EXISTS transaction_block_hash_bytes_key;

-- Create a unique index using MD5 hashes
CREATE UNIQUE INDEX transaction_block_hash_bytes_key ON "transaction" (MD5(block_hash::text), MD5(bytes::text));
