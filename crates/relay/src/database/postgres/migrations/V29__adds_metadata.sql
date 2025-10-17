DO $$
BEGIN
    IF NOT EXISTS (
        SELECT 1
        FROM information_schema.columns
        WHERE table_name = 'submission_trace'
          AND column_name = 'metadata'
    ) THEN
        ALTER TABLE submission_trace ADD COLUMN metadata varchar;
    END IF;
END
$$;

DO $$
BEGIN
    IF NOT EXISTS (
        SELECT 1
        FROM information_schema.columns
        WHERE table_name = 'header_submission_trace'
          AND column_name = 'metadata'
    ) THEN
        ALTER TABLE header_submission_trace ADD COLUMN metadata varchar;
    END IF;
END
$$;