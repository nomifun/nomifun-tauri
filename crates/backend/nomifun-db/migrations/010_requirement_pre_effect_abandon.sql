-- A Requirement may return from active execution authority to `pending` only
-- when one SQLite writer transaction proves that the exact claim never crossed
-- either durable receiver-admission boundary.
--
-- The guard row is not a caller-supplied boolean.  Its INSERT trigger
-- independently validates the exact Requirement generation/capability/typed
-- owner and proves absence of every Conversation authority receipt, aggregate
-- active operation, and Terminal admission for that logical claim.  The UPDATE
-- trigger repeats those proofs at the mutation boundary.  SQLite's serialized
-- writer transactions therefore totally order receiver admission against
-- abandon: only one can win, on Windows, Linux, and macOS.
CREATE TABLE requirement_pre_effect_abandon_guards (
    id                      INTEGER PRIMARY KEY AUTOINCREMENT,
    requirement_id          TEXT NOT NULL
                            CHECK (
                                length(requirement_id) = 36
                                AND lower(requirement_id) = requirement_id
                                AND requirement_id GLOB '????????-????-7???-[89ab]???-????????????'
                                AND replace(requirement_id, '-', '') NOT GLOB '*[^0-9a-f]*'
                            ),
    claim_generation        INTEGER NOT NULL CHECK (claim_generation >= 1),
    claim_token             TEXT NOT NULL
                            CHECK (
                                length(claim_token) = 64
                                AND lower(claim_token) = claim_token
                                AND claim_token NOT GLOB '*[^0-9a-f]*'
                            ),
    owner_conversation_id   TEXT
                            CHECK (
                                owner_conversation_id IS NULL
                                OR (
                                    length(owner_conversation_id) = 36
                                    AND lower(owner_conversation_id) = owner_conversation_id
                                    AND owner_conversation_id GLOB '????????-????-7???-[89ab]???-????????????'
                                    AND replace(owner_conversation_id, '-', '') NOT GLOB '*[^0-9a-f]*'
                                )
                            ),
    owner_terminal_id       TEXT
                            CHECK (
                                owner_terminal_id IS NULL
                                OR (
                                    length(owner_terminal_id) = 36
                                    AND lower(owner_terminal_id) = owner_terminal_id
                                    AND owner_terminal_id GLOB '????????-????-7???-[89ab]???-????????????'
                                    AND replace(owner_terminal_id, '-', '') NOT GLOB '*[^0-9a-f]*'
                                )
                            ),
    created_at              INTEGER NOT NULL,
    CHECK (
        (owner_conversation_id IS NOT NULL AND owner_terminal_id IS NULL)
        OR
        (owner_conversation_id IS NULL AND owner_terminal_id IS NOT NULL)
    )
);

CREATE UNIQUE INDEX idx_requirement_pre_effect_abandon_requirement_id
    ON requirement_pre_effect_abandon_guards(requirement_id);
CREATE INDEX idx_requirement_pre_effect_abandon_owner_conversation
    ON requirement_pre_effect_abandon_guards(owner_conversation_id)
    WHERE owner_conversation_id IS NOT NULL;
CREATE INDEX idx_requirement_pre_effect_abandon_owner_terminal
    ON requirement_pre_effect_abandon_guards(owner_terminal_id)
    WHERE owner_terminal_id IS NOT NULL;

CREATE TRIGGER trg_requirements_pre_effect_abandon_guard_insert
BEFORE INSERT ON requirement_pre_effect_abandon_guards
FOR EACH ROW
WHEN NOT EXISTS (
    SELECT 1
      FROM requirements AS requirement
     WHERE requirement.requirement_id = NEW.requirement_id
       AND requirement.status = 'in_progress'
       AND requirement.claim_generation = NEW.claim_generation
       AND requirement.claim_token = NEW.claim_token
       AND requirement.owner_conversation_id IS NEW.owner_conversation_id
       AND requirement.owner_terminal_id IS NEW.owner_terminal_id
       AND NOT EXISTS (
           SELECT 1
             FROM conversation_delivery_receipts AS receipt
            WHERE json_extract(
                      receipt.request_payload,
                      '$.autowork_authority.requirement_id'
                  ) = requirement.requirement_id
              AND json_extract(
                      receipt.request_payload,
                      '$.autowork_authority.claim_generation'
                  ) = requirement.claim_generation
       )
       AND NOT EXISTS (
           SELECT 1
             FROM conversations AS conversation
            WHERE conversation.conversation_id = requirement.owner_conversation_id
              AND (
                  conversation.status = 'running'
                  OR conversation.active_turn_operation_id IS NOT NULL
              )
       )
       AND NOT EXISTS (
           SELECT 1
             FROM terminal_turn_admissions AS admission
            WHERE admission.requirement_id = requirement.requirement_id
              AND admission.claim_generation = requirement.claim_generation
       )
)
BEGIN
    SELECT RAISE(
        ABORT,
        'Requirement pre-effect abandon guard requires exact authority and receiver-admission absence'
    );
END;

CREATE TRIGGER trg_requirements_pre_effect_abandon_guard_immutable
BEFORE UPDATE ON requirement_pre_effect_abandon_guards
BEGIN
    SELECT RAISE(
        ABORT,
        'Requirement pre-effect abandon guards are immutable'
    );
END;

-- While its exact active Requirement still exists, a guard may be consumed
-- only by the AFTER trigger below.  This prevents a caller from deleting and
-- replacing the capability to rewrite its generation/token/typed owner.
CREATE TRIGGER trg_requirements_pre_effect_abandon_guard_delete_guard
BEFORE DELETE ON requirement_pre_effect_abandon_guards
FOR EACH ROW
WHEN EXISTS (
    SELECT 1
      FROM requirements AS requirement
     WHERE requirement.requirement_id = OLD.requirement_id
       AND requirement.status = 'in_progress'
       AND requirement.claim_generation = OLD.claim_generation
       AND requirement.claim_token = OLD.claim_token
       AND requirement.owner_conversation_id IS OLD.owner_conversation_id
       AND requirement.owner_terminal_id IS OLD.owner_terminal_id
)
BEGIN
    SELECT RAISE(
        ABORT,
        'active Requirement pre-effect abandon guard can only be consumed by guarded transition'
    );
END;

-- INSERT is the command, not a durable permit.  A valid command immediately
-- performs the exact transition inside the same SQLite statement/transaction.
-- The guarded UPDATE consumes the command row; if it does not, aborting this
-- INSERT rolls back every nested trigger effect.
CREATE TRIGGER trg_requirements_pre_effect_abandon_guard_apply
AFTER INSERT ON requirement_pre_effect_abandon_guards
FOR EACH ROW
BEGIN
    UPDATE requirements
       SET status = 'pending',
           completion_note = NULL,
           owner_conversation_id = NULL,
           owner_terminal_id = NULL,
           active_turn_started_at = NULL,
           lease_expires_at = NULL,
           attempt_count = MAX(attempt_count - 1, 0),
           claim_token = NULL,
           updated_at = MAX(updated_at, NEW.created_at)
     WHERE requirement_id = NEW.requirement_id
       AND status = 'in_progress'
       AND claim_generation = NEW.claim_generation
       AND claim_token = NEW.claim_token
       AND owner_conversation_id IS NEW.owner_conversation_id
       AND owner_terminal_id IS NEW.owner_terminal_id;

    SELECT CASE
        WHEN EXISTS (
            SELECT 1
              FROM requirement_pre_effect_abandon_guards AS guard
             WHERE guard.id = NEW.id
        )
        THEN RAISE(
            ABORT,
            'Requirement pre-effect abandon command did not complete its exact transition'
        )
    END;
END;

-- Ordinary raw active->pending writes have no exact guard and are rejected.
-- A stale guard is also insufficient: receiver absence and the complete
-- pending-row shape are checked again at the UPDATE cutpoint.
CREATE TRIGGER trg_requirements_active_to_pending_pre_effect_guard
BEFORE UPDATE ON requirements
FOR EACH ROW
WHEN OLD.status = 'in_progress'
 AND NEW.status = 'pending'
 AND (
     NOT EXISTS (
         SELECT 1
           FROM requirement_pre_effect_abandon_guards AS guard
          WHERE guard.requirement_id = OLD.requirement_id
            AND guard.claim_generation = OLD.claim_generation
            AND guard.claim_token = OLD.claim_token
            AND guard.owner_conversation_id IS OLD.owner_conversation_id
            AND guard.owner_terminal_id IS OLD.owner_terminal_id
     )
     OR EXISTS (
         SELECT 1
           FROM conversation_delivery_receipts AS receipt
          WHERE json_extract(
                    receipt.request_payload,
                    '$.autowork_authority.requirement_id'
                ) = OLD.requirement_id
            AND json_extract(
                    receipt.request_payload,
                    '$.autowork_authority.claim_generation'
                ) = OLD.claim_generation
     )
     OR EXISTS (
         SELECT 1
           FROM conversations AS conversation
          WHERE conversation.conversation_id = OLD.owner_conversation_id
            AND (
                conversation.status = 'running'
                OR conversation.active_turn_operation_id IS NOT NULL
            )
     )
     OR EXISTS (
         SELECT 1
           FROM terminal_turn_admissions AS admission
          WHERE admission.requirement_id = OLD.requirement_id
            AND admission.claim_generation = OLD.claim_generation
     )
     OR NEW.claim_generation IS NOT OLD.claim_generation
     OR NEW.claim_token IS NOT NULL
     OR NEW.completion_note IS NOT NULL
     OR NEW.owner_conversation_id IS NOT NULL
     OR NEW.owner_terminal_id IS NOT NULL
     OR NEW.active_turn_started_at IS NOT NULL
     OR NEW.lease_expires_at IS NOT NULL
     OR NEW.started_at IS NOT OLD.started_at
     OR NEW.attempt_count IS NOT MAX(OLD.attempt_count - 1, 0)
 )
BEGIN
    SELECT RAISE(
        ABORT,
        'active Requirement may become pending only through exact pre-effect abandon'
    );
END;

-- Consume the one-transaction capability as part of the guarded UPDATE.  The
-- repository also issues an exact DELETE before COMMIT so a no-row/stale path
-- cannot leave authorization behind.
CREATE TRIGGER trg_requirements_pre_effect_abandon_guard_consume
AFTER UPDATE ON requirements
FOR EACH ROW
WHEN OLD.status = 'in_progress'
 AND NEW.status = 'pending'
BEGIN
    DELETE FROM requirement_pre_effect_abandon_guards
     WHERE requirement_id = OLD.requirement_id
       AND claim_generation = OLD.claim_generation
       AND claim_token = OLD.claim_token
       AND owner_conversation_id IS OLD.owner_conversation_id
       AND owner_terminal_id IS OLD.owner_terminal_id;
END;
