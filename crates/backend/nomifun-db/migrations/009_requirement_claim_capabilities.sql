-- Unforgeable capability for one exact AutoWork claim.
--
-- `claim_generation` remains the monotonic audit identity. `claim_token` is
-- the 256-bit secret authority minted only by the atomic pending->in_progress
-- allocator and never exposed through the public Requirement DTO.
ALTER TABLE requirements
    ADD COLUMN claim_token TEXT
        CHECK (
            claim_token IS NULL
            OR (
                length(claim_token) = 64
                AND lower(claim_token) = claim_token
                AND claim_token NOT GLOB '*[^0-9a-f]*'
            )
        );

-- Terminal turn receipts retain the same capability so every irreversible
-- PTY boundary can revalidate the Requirement in one SQLite statement.
ALTER TABLE terminal_turn_admissions
    ADD COLUMN claim_token TEXT
        CHECK (
            claim_token IS NULL
            OR (
                length(claim_token) = 64
                AND lower(claim_token) = claim_token
                AND claim_token NOT GLOB '*[^0-9a-f]*'
            )
        );

-- Older builds could leave a row labelled pending while retaining an owner,
-- active-turn timestamp, or renewable lease. That is not fresh work: it is an
-- incomplete execution teardown whose effects are unknown. Park it before the
-- pending guards are installed, preserving identity evidence and never making
-- it selectable by a new runner.
UPDATE requirements
SET status = 'needs_review',
    lease_expires_at = NULL,
    completion_note = COALESCE(
        completion_note,
        'Legacy pending Requirement retained execution authority evidence; it was parked for review and was not executed again.'
    )
WHERE status = 'pending'
  AND (
      owner_conversation_id IS NOT NULL
      OR owner_terminal_id IS NOT NULL
      OR active_turn_started_at IS NOT NULL
      OR lease_expires_at IS NOT NULL
  );

-- Historical builds did not physically enforce one active Requirement per
-- typed session.  If such corruption exists, every duplicate is ambiguous:
-- park all of them (not an arbitrary winner) while retaining owner, generation,
-- token, and timestamps as audit evidence.
UPDATE requirements
SET status = 'needs_review',
    lease_expires_at = NULL,
    completion_note = COALESCE(
        completion_note,
        'Multiple active Requirements were recorded for one conversation; all were parked for review and none was executed again.'
    )
WHERE status = 'in_progress'
  AND owner_conversation_id IN (
      SELECT owner_conversation_id
      FROM requirements
      WHERE status = 'in_progress'
        AND owner_conversation_id IS NOT NULL
      GROUP BY owner_conversation_id
      HAVING COUNT(*) > 1
  );

UPDATE requirements
SET status = 'needs_review',
    lease_expires_at = NULL,
    completion_note = COALESCE(
        completion_note,
        'Multiple active Requirements were recorded for one terminal; all were parked for review and none was executed again.'
    )
WHERE status = 'in_progress'
  AND owner_terminal_id IN (
      SELECT owner_terminal_id
      FROM requirements
      WHERE status = 'in_progress'
        AND owner_terminal_id IS NOT NULL
      GROUP BY owner_terminal_id
      HAVING COUNT(*) > 1
  );

-- An active row created by an older build has no unforgeable authority. Its
-- effects may already have started, so fail closed rather than minting a token
-- or making it claimable.
UPDATE requirements
SET status = 'needs_review',
    lease_expires_at = NULL,
    completion_note = COALESCE(
        completion_note,
        'Legacy AutoWork claim had no durable capability token; execution outcome is ambiguous and it was not executed again.'
    )
WHERE status = 'in_progress'
  AND claim_token IS NULL;

-- The matching legacy PTY receipts are also absorbing. Requirement rows were
-- parked above in the same migration transaction.
UPDATE terminal_turn_admissions
SET phase = 'settled',
    outcome = 'needs_review',
    detail = COALESCE(
        detail,
        'Legacy terminal turn had no durable Requirement capability token; it was not submitted again.'
    ),
    settled_at = COALESCE(settled_at, admitted_at)
WHERE phase IS NOT 'settled'
  AND claim_token IS NULL;

CREATE UNIQUE INDEX uq_requirements_active_conversation_owner
    ON requirements(owner_conversation_id)
    WHERE status = 'in_progress' AND owner_conversation_id IS NOT NULL;

CREATE UNIQUE INDEX uq_requirements_active_terminal_owner
    ON requirements(owner_terminal_id)
    WHERE status = 'in_progress' AND owner_terminal_id IS NOT NULL;

-- Last-resort invariant shared by Windows, Linux, and macOS: no code path,
-- including a future raw SQL caller, may reopen a completed or cancelled
-- Requirement. Failed and needs_review remain explicitly resumable by humans.
CREATE TRIGGER trg_requirements_absorb_done_cancelled
BEFORE UPDATE OF status ON requirements
FOR EACH ROW
WHEN OLD.status IN ('done', 'cancelled')
 AND NEW.status IS NOT OLD.status
BEGIN
    SELECT RAISE(ABORT, 'completed or cancelled Requirement status is immutable');
END;

CREATE TRIGGER trg_requirements_active_identity_exit_guard
BEFORE UPDATE ON requirements
FOR EACH ROW
WHEN OLD.status = 'in_progress'
 AND NEW.status IS NOT 'pending'
 AND (
        NEW.claim_generation IS NOT OLD.claim_generation
        OR NEW.claim_token IS NOT OLD.claim_token
        OR NEW.owner_conversation_id IS NOT OLD.owner_conversation_id
        OR NEW.owner_terminal_id IS NOT OLD.owner_terminal_id
        OR NEW.active_turn_started_at IS NOT OLD.active_turn_started_at
        OR NEW.started_at IS NOT OLD.started_at
        OR NEW.attempt_count IS NOT OLD.attempt_count
 )
BEGIN
    SELECT RAISE(ABORT, 'active Requirement identity is immutable until exact requeue');
END;

-- An in-progress row is executable authority.  Reject every raw INSERT/UPDATE
-- path that tries to manufacture it without a fresh capability and exactly
-- one typed owner.  The column CHECK above validates the token encoding.
CREATE TRIGGER trg_requirements_in_progress_insert_guard
BEFORE INSERT ON requirements
FOR EACH ROW
WHEN NEW.status = 'in_progress'
BEGIN
    SELECT RAISE(ABORT, 'in-progress Requirement may only be entered by atomically claiming a pending row');
END;

CREATE TRIGGER trg_requirements_in_progress_update_guard
BEFORE UPDATE ON requirements
FOR EACH ROW
WHEN NEW.status = 'in_progress'
 AND (
        NEW.claim_generation IS NULL
        OR NEW.claim_generation <= 0
        OR NEW.claim_token IS NULL
        OR NEW.active_turn_started_at IS NULL
        OR NEW.lease_expires_at IS NULL
        OR NEW.started_at IS NULL
        OR NEW.lease_expires_at <= NEW.active_turn_started_at
        OR (
            OLD.status = 'in_progress'
            AND (
                NEW.claim_generation IS NOT OLD.claim_generation
                OR NEW.claim_token IS NOT OLD.claim_token
                OR NEW.owner_conversation_id IS NOT OLD.owner_conversation_id
                OR NEW.owner_terminal_id IS NOT OLD.owner_terminal_id
                OR NEW.active_turn_started_at IS NOT OLD.active_turn_started_at
                OR NEW.started_at IS NOT OLD.started_at
                OR NEW.attempt_count IS NOT OLD.attempt_count
            )
        )
        OR (
            OLD.status = 'pending'
            AND (
                OLD.claim_token IS NOT NULL
                OR NEW.claim_generation IS NOT OLD.claim_generation + 1
                OR NEW.attempt_count IS NOT OLD.attempt_count + 1
            )
        )
        OR OLD.status IS NULL
        OR OLD.status NOT IN ('pending', 'in_progress')
        OR NOT (
            (NEW.owner_conversation_id IS NOT NULL AND NEW.owner_terminal_id IS NULL)
            OR
            (NEW.owner_conversation_id IS NULL AND NEW.owner_terminal_id IS NOT NULL)
        )
 )
BEGIN
    SELECT RAISE(ABORT, 'in-progress Requirement requires generation, capability, and exactly one typed owner');
END;

-- Pending is non-authority.  Any retry/resume must erase the prior execution
-- capability and typed owner before the row can be selected again.
CREATE TRIGGER trg_requirements_pending_insert_guard
BEFORE INSERT ON requirements
FOR EACH ROW
WHEN NEW.status = 'pending'
 AND (
        NEW.claim_token IS NOT NULL
        OR NEW.owner_conversation_id IS NOT NULL
        OR NEW.owner_terminal_id IS NOT NULL
        OR NEW.active_turn_started_at IS NOT NULL
        OR NEW.lease_expires_at IS NOT NULL
 )
BEGIN
    SELECT RAISE(ABORT, 'pending Requirement cannot carry execution authority');
END;

CREATE TRIGGER trg_requirements_pending_update_guard
BEFORE UPDATE ON requirements
FOR EACH ROW
WHEN NEW.status = 'pending'
 AND (
        NEW.claim_token IS NOT NULL
        OR NEW.owner_conversation_id IS NOT NULL
        OR NEW.owner_terminal_id IS NOT NULL
        OR NEW.active_turn_started_at IS NOT NULL
        OR NEW.lease_expires_at IS NOT NULL
 )
BEGIN
    SELECT RAISE(ABORT, 'pending Requirement cannot carry execution authority');
END;

-- An open terminal receipt represents an effect that may still cross the PTY
-- boundary.  It must carry the same unforgeable Requirement capability.
CREATE TRIGGER trg_terminal_turn_admissions_open_insert_guard
BEFORE INSERT ON terminal_turn_admissions
FOR EACH ROW
WHEN NEW.phase IS NOT 'settled'
 AND NEW.claim_token IS NULL
BEGIN
    SELECT RAISE(ABORT, 'open terminal turn admission requires a Requirement capability');
END;

CREATE TRIGGER trg_terminal_turn_admissions_open_update_guard
BEFORE UPDATE ON terminal_turn_admissions
FOR EACH ROW
WHEN (
        NEW.phase IS NOT 'settled'
        AND NEW.claim_token IS NULL
     )
 OR NEW.claim_token IS NOT OLD.claim_token
BEGIN
    SELECT RAISE(ABORT, 'terminal turn admission capability is required and immutable');
END;
