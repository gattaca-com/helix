ALTER TABLE merged_blocks
    ADD COLUMN request_time_ns BIGINT NOT NULL DEFAULT 0,
    ADD COLUMN sim_start_time_ns BIGINT NOT NULL DEFAULT 0,
    ADD COLUMN sim_end_time_ns BIGINT NOT NULL DEFAULT 0,
    ADD COLUMN finalize_time_ns BIGINT NOT NULL DEFAULT 0;
