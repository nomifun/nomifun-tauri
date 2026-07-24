-- Durable Conversation turn generation authority.
--
-- `admission_epoch` invalidates requests that began before reset/stop/orphan
-- cleanup. `active_turn_operation_id` binds a durable Running generation to
-- the exact receipt that admitted it, so an old finalizer cannot finish a
-- replacement turn.
ALTER TABLE conversations
    ADD COLUMN admission_epoch INTEGER NOT NULL DEFAULT 0
        CHECK (admission_epoch >= 0);

ALTER TABLE conversations
    ADD COLUMN active_turn_operation_id TEXT;

CREATE INDEX idx_conversations_active_turn_operation
    ON conversations(active_turn_operation_id)
    WHERE active_turn_operation_id IS NOT NULL;

-- Delivery receipts are permanent replay evidence. Their outcome/projection
-- fields may advance, but the idempotency scope and caller-minted candidate
-- can never be rewritten into a different operation after admission.
CREATE TRIGGER trg_conversation_delivery_receipts_identity_immutable
BEFORE UPDATE OF
    operation_id,
    message_id,
    conversation_id,
    user_id,
    kind,
    request_payload,
    created_at
ON conversation_delivery_receipts
WHEN OLD.operation_id IS NOT NEW.operation_id
  OR OLD.message_id IS NOT NEW.message_id
  OR OLD.conversation_id IS NOT NEW.conversation_id
  OR OLD.user_id IS NOT NEW.user_id
  OR OLD.kind IS NOT NEW.kind
  OR OLD.request_payload IS NOT NEW.request_payload
  OR OLD.created_at IS NOT NEW.created_at
BEGIN
    SELECT RAISE(
        ABORT,
        'Conversation delivery receipt identity is immutable'
    );
END;

CREATE TRIGGER trg_conversation_delivery_receipts_no_delete
BEFORE DELETE ON conversation_delivery_receipts
BEGIN
    SELECT RAISE(
        ABORT,
        'Conversation delivery receipts are retained indefinitely'
    );
END;

-- A Conversation may only enter Running through the same transaction that
-- first materializes its exact accepted turn receipt.  Existing legacy
-- Running rows are intentionally left untouched by this migration; the
-- triggers govern only future INSERT/UPDATE statements.
CREATE TRIGGER trg_conversations_running_admission_guard
BEFORE UPDATE OF status, active_turn_operation_id, admission_epoch ON conversations
WHEN OLD.status IS NOT 'running'
 AND NEW.status = 'running'
 AND (
     NEW.active_turn_operation_id IS NULL
     OR trim(NEW.active_turn_operation_id) = ''
     OR NEW.admission_epoch IS NOT OLD.admission_epoch + 1
     OR NOT EXISTS (
         SELECT 1
           FROM conversation_delivery_receipts AS receipt
          WHERE receipt.operation_id = NEW.active_turn_operation_id
            AND receipt.user_id = NEW.user_id
            AND receipt.conversation_id = NEW.conversation_id
            AND receipt.kind = 'turn'
            AND receipt.status = 'accepted'
     )
 )
BEGIN
    SELECT RAISE(
        ABORT,
        'Conversation Running admission requires an exact accepted turn receipt and next epoch'
    );
END;

-- Running is a durable execution authority, so it may only be retired after
-- every accepted turn receipt for the generation has been made terminal in
-- the same transaction. Exact generations additionally require their owner
-- receipt to exist in Completed state. Legacy pre-008 active-null rows remain
-- recoverable once no accepted turn receipt remains.
CREATE TRIGGER trg_conversations_running_exit_guard
BEFORE UPDATE OF status, active_turn_operation_id, admission_epoch ON conversations
WHEN OLD.status = 'running'
 AND NEW.status IS NOT 'running'
 AND (
     NEW.status IS NOT 'finished'
     OR NEW.active_turn_operation_id IS NOT NULL
     OR NEW.admission_epoch IS NOT OLD.admission_epoch + 1
     OR EXISTS (
         SELECT 1
           FROM conversation_delivery_receipts AS receipt
          WHERE receipt.user_id = OLD.user_id
            AND receipt.conversation_id = OLD.conversation_id
            AND receipt.kind = 'turn'
            AND receipt.status = 'accepted'
     )
     OR (
         OLD.active_turn_operation_id IS NOT NULL
         AND NOT EXISTS (
             SELECT 1
               FROM conversation_delivery_receipts AS receipt
              WHERE receipt.operation_id = OLD.active_turn_operation_id
                AND receipt.user_id = OLD.user_id
                AND receipt.conversation_id = OLD.conversation_id
                AND receipt.kind = 'turn'
                AND receipt.status = 'completed'
         )
     )
 )
BEGIN
    SELECT RAISE(
        ABORT,
        'Conversation Running exit requires completed turn receipts, Finished state, cleared owner, and next epoch'
    );
END;

-- Hard-deleting Running would erase the only aggregate-side record of live
-- execution authority. Callers must first settle and finish the exact turn.
CREATE TRIGGER trg_conversations_running_delete_guard
BEFORE DELETE ON conversations
WHEN OLD.status = 'running'
BEGIN
    SELECT RAISE(
        ABORT,
        'Conversation Running authority cannot be deleted'
    );
END;

CREATE TRIGGER trg_conversations_running_insert_guard
BEFORE INSERT ON conversations
WHEN NEW.status = 'running'
BEGIN
    SELECT RAISE(
        ABORT,
        'Conversation cannot be inserted Running'
    );
END;

-- While one Running generation is active, its durable owner and generation
-- are immutable.  Terminal transitions may clear the owner and advance the
-- epoch, but an in-place rewrite cannot impersonate a successor turn.
CREATE TRIGGER trg_conversations_running_owner_immutable
BEFORE UPDATE OF status, active_turn_operation_id, admission_epoch ON conversations
WHEN OLD.status = 'running'
 AND NEW.status = 'running'
 AND (
     NEW.active_turn_operation_id IS NOT OLD.active_turn_operation_id
     OR NEW.admission_epoch IS NOT OLD.admission_epoch
 )
BEGIN
    SELECT RAISE(
        ABORT,
        'Conversation Running owner and epoch are immutable'
    );
END;
