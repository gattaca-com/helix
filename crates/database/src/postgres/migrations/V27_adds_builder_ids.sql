DO $$
BEGIN
    IF NOT EXISTS (
        SELECT 1
        FROM information_schema.columns
        WHERE table_schema = 'public'
          AND table_name = 'builder_info'
          AND column_name = 'builder_ids'
    ) THEN
        ALTER TABLE public.builder_info ADD COLUMN builder_ids character varying[];
    END IF;
END$$;

DO $$
BEGIN
    IF NOT EXISTS (
        SELECT 1
        FROM information_schema.columns
        WHERE table_schema = 'public'
          AND table_name = 'validator_preferences'
          AND column_name = 'delay_ms'
    ) THEN
        ALTER TABLE public.validator_preferences ADD COLUMN delay_ms bigint DEFAULT 650;
    END IF;
END$$;

DO $$
BEGIN
    IF NOT EXISTS (
        SELECT 1
        FROM information_schema.columns
        WHERE table_schema = 'public'
          AND table_name = 'validator_preferences'
          AND column_name = 'manual_override'
    ) THEN
        ALTER TABLE public.validator_preferences ADD COLUMN manual_override boolean DEFAULT false;
    END IF;
END$$;

DO $$
BEGIN
    IF NOT EXISTS (
        SELECT 1
        FROM information_schema.columns
        WHERE table_schema = 'public'
          AND table_name = 'get_header'
          AND column_name = 'mev_boost'
    ) THEN
        ALTER TABLE public.get_header ADD COLUMN mev_boost boolean DEFAULT false;
    END IF;
END$$;

SET check_function_bodies = OFF;


CREATE OR REPLACE FUNCTION public.array_concat_uniq(arr1 anyarray, arr2 anyarray)
RETURNS anyarray
LANGUAGE sql
AS $function$
    SELECT array_agg(DISTINCT e)
    FROM unnest(arr1 || arr2) e;
$function$;
