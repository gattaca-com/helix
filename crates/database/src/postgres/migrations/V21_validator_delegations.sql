CREATE TABLE "validator_delegations" (
    "validator_pubkey" bytea NOT NULL,
    "delegatee_pubkey" bytea NOT NULL,
    "created_at" timestamptz DEFAULT (now()),
    PRIMARY KEY ("validator_pubkey", "delegatee_pubkey")
);