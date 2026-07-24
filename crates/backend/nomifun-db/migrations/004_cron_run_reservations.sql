-- Durable Cron occurrence identity.
--
-- `cron_job_runs` is deliberately only a seven-row presentation history.
-- This reservation ledger is the non-prunable admission record used to absorb
-- scheduler, HTTP, Gateway, retry, and process-restart replays.
ALTER TABLE cron_jobs
    ADD COLUMN schedule_revision INTEGER NOT NULL DEFAULT 1
        CHECK (schedule_revision > 0);

CREATE TABLE cron_run_reservations (
    id                  INTEGER PRIMARY KEY AUTOINCREMENT,
    cron_job_run_id     TEXT NOT NULL UNIQUE
                        CHECK (
                            length(cron_job_run_id) = 36
                            AND lower(cron_job_run_id) = cron_job_run_id
                            AND cron_job_run_id GLOB '????????-????-7???-[89ab]???-????????????'
                            AND replace(cron_job_run_id, '-', '') NOT GLOB '*[^0-9a-f]*'
                        ),
    cron_job_id         TEXT NOT NULL
                        CHECK (
                            length(cron_job_id) = 36
                            AND lower(cron_job_id) = cron_job_id
                            AND cron_job_id GLOB '????????-????-7???-[89ab]???-????????????'
                            AND replace(cron_job_id, '-', '') NOT GLOB '*[^0-9a-f]*'
                        ),
    trigger_kind        TEXT NOT NULL CHECK (trigger_kind IN ('scheduled', 'run_now')),
    operation_key       TEXT NOT NULL UNIQUE CHECK (length(operation_key) BETWEEN 1 AND 1024),
    request_fingerprint TEXT NOT NULL CHECK (length(request_fingerprint) BETWEEN 1 AND 1024),
    schedule_revision   INTEGER CHECK (schedule_revision > 0),
    planned_at_ms       INTEGER,
    status              TEXT NOT NULL DEFAULT 'reserved'
                        CHECK (status IN ('reserved', 'ok', 'error', 'skipped', 'missed')),
    conversation_id     TEXT,
    result_error        TEXT,
    created_at_ms       INTEGER NOT NULL,
    updated_at_ms       INTEGER NOT NULL,
    settled_at_ms       INTEGER,
    CHECK (
        (trigger_kind = 'scheduled' AND schedule_revision IS NOT NULL AND planned_at_ms IS NOT NULL)
        OR
        (trigger_kind = 'run_now' AND schedule_revision IS NULL AND planned_at_ms IS NULL)
    ),
    CHECK (
        (status = 'reserved' AND settled_at_ms IS NULL)
        OR
        (status <> 'reserved' AND settled_at_ms IS NOT NULL)
    ),
    CHECK (
        conversation_id IS NULL
        OR (
            length(conversation_id) = 36
            AND lower(conversation_id) = conversation_id
            AND conversation_id GLOB '????????-????-7???-[89ab]???-????????????'
            AND replace(conversation_id, '-', '') NOT GLOB '*[^0-9a-f]*'
        )
    )
);

CREATE UNIQUE INDEX uq_cron_run_reservations_scheduled_occurrence
    ON cron_run_reservations(cron_job_id, schedule_revision, planned_at_ms)
    WHERE trigger_kind = 'scheduled';
CREATE INDEX idx_cron_run_reservations_cron_job_id
    ON cron_run_reservations(cron_job_id);
CREATE INDEX idx_cron_run_reservations_conversation_id
    ON cron_run_reservations(conversation_id);
CREATE INDEX idx_cron_run_reservations_unsettled
    ON cron_run_reservations(cron_job_id, updated_at_ms)
    WHERE status = 'reserved';
