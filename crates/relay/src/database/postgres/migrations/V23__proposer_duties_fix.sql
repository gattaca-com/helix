ALTER TABLE proposer_duties DROP CONSTRAINT proposer_duties_pkey;
ALTER TABLE proposer_duties ADD PRIMARY KEY (slot_number);
