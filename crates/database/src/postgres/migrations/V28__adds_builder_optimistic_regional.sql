DO $$
BEGIN
    IF NOT EXISTS (
        SELECT 1
        FROM information_schema.columns
        WHERE table_schema = 'public'
          AND table_name = 'builder_info'
          AND column_name = 'is_optimistic_for_regional_filtering'
    ) THEN
        ALTER TABLE public.builder_info ADD COLUMN is_optimistic_for_regional_filtering boolean DEFAULT false;
    END IF;
END$$;
