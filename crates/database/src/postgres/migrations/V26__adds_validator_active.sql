ALTER TABLE validator_registrations
  ADD COLUMN IF NOT EXISTS "active" BOOLEAN DEFAULT TRUE;
