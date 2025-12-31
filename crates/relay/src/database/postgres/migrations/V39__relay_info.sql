CREATE TABLE IF NOT EXISTS relay_info (
    "version" integer PRIMARY KEY,
    "adjustments_enabled" boolean NOT NULL DEFAULT true
);

INSERT INTO relay_info ("version")
VALUES (1)
ON CONFLICT ("version") DO NOTHING;
