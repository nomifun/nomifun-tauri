-- Monotonic execution identity for AutoWork.
--
-- `attempt_count` is presentation/backoff state and may be compensated by
-- existing flows. `claim_generation` never moves backwards, so a process
-- restart can replay one claim without creating a second model turn.
ALTER TABLE requirements
    ADD COLUMN claim_generation INTEGER NOT NULL DEFAULT 0
        CHECK (claim_generation >= 0);

-- Existing `in_progress` rows predate durable execution identities. Their
-- model/tool/PTY effects may already have started, but generation 0 cannot be
-- authenticated by the new exact-CAS protocol. Preserve owner and audit
-- timestamps, clear only the renewable lease, and park them permanently for
-- review. Never manufacture generation 1: pending legacy rows naturally
-- receive 1 on their first atomic claim.
UPDATE requirements
SET status = 'needs_review',
    lease_expires_at = NULL,
    completion_note = COALESCE(
        completion_note,
        'Legacy AutoWork claim had no durable generation; execution outcome is ambiguous and it was not executed again.'
    )
WHERE status = 'in_progress'
  AND claim_generation = 0;
