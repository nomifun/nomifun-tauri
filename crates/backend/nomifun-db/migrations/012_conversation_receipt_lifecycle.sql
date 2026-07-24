-- Conversation delivery receipts are absorbing execution evidence.
--
-- The application repository already writes accepted -> completed exactly
-- once, but the database must reject raw SQL which rolls a completed receipt
-- back to accepted or rewrites its terminal outcome. Otherwise an old
-- operation can once again satisfy the Conversation Running admission guard.
--
-- Projection columns deliberately remain detachable after completion. They
-- describe whether the immutable receipt is currently materialized in the
-- transcript; they are not part of the execution lifecycle outcome.

CREATE TRIGGER trg_conversation_delivery_receipts_lifecycle_insert_guard
BEFORE INSERT ON conversation_delivery_receipts
WHEN (
    NEW.status = 'accepted'
    AND (
        NEW.result_ok IS NOT NULL
        OR NEW.result_text IS NOT NULL
        OR NEW.result_error IS NOT NULL
        OR NEW.completed_at IS NOT NULL
    )
) OR (
    NEW.status = 'completed'
    AND (
        typeof(NEW.completed_at) <> 'integer'
        OR NEW.completed_at < NEW.created_at
        OR typeof(NEW.result_ok) <> 'integer'
        OR NEW.result_ok NOT IN (0, 1)
    )
)
BEGIN
    SELECT RAISE(
        ABORT,
        'Conversation delivery receipt has an invalid lifecycle shape'
    );
END;

CREATE TRIGGER trg_conversation_delivery_receipts_lifecycle_update_guard
BEFORE UPDATE OF status, result_ok, result_text, result_error, completed_at
ON conversation_delivery_receipts
WHEN (
    OLD.status = 'completed'
    AND (
        NEW.status IS NOT OLD.status
        OR NEW.result_ok IS NOT OLD.result_ok
        OR NEW.result_text IS NOT OLD.result_text
        OR NEW.result_error IS NOT OLD.result_error
        OR NEW.completed_at IS NOT OLD.completed_at
    )
) OR (
    NEW.status = 'accepted'
    AND (
        NEW.result_ok IS NOT NULL
        OR NEW.result_text IS NOT NULL
        OR NEW.result_error IS NOT NULL
        OR NEW.completed_at IS NOT NULL
    )
) OR (
    NEW.status = 'completed'
    AND (
        typeof(NEW.completed_at) <> 'integer'
        OR NEW.completed_at < NEW.created_at
        OR typeof(NEW.result_ok) <> 'integer'
        OR NEW.result_ok NOT IN (0, 1)
    )
)
BEGIN
    SELECT RAISE(
        ABORT,
        'Conversation delivery receipt lifecycle is absorbing and terminal outcomes are immutable'
    );
END;
