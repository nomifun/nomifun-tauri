-- Exact-once Cron job projection.
--
-- Historical code updated `cron_jobs` before settling the durable run
-- reservation. A process death between those writes left no exact run id on
-- the aggregate, so startup could only guess from timestamps and could double
-- increment `run_count`. Existing rows are deliberately marked
-- `legacy_unknown`: a still-reserved legacy row must be quarantined rather
-- than guessed. New reservations explicitly start `pending` and are settled
-- together with their job projection in one transaction.
ALTER TABLE cron_run_reservations
    ADD COLUMN job_projection_state TEXT NOT NULL DEFAULT 'legacy_unknown'
        CHECK (job_projection_state IN ('legacy_unknown', 'pending', 'applied'));

ALTER TABLE cron_run_reservations
    ADD COLUMN job_projected_at_ms INTEGER;

CREATE INDEX idx_cron_run_reservations_projection
    ON cron_run_reservations(status, job_projection_state, created_at_ms);
