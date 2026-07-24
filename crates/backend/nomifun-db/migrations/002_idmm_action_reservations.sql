-- Durable exactly-once admission fence for IDMM actions. This is deliberately
-- separate from idmm_interventions: audit rows are disposable, while losing a
-- reservation could authorize the same side effect again.
CREATE TABLE idmm_action_reservations (
    id                INTEGER PRIMARY KEY AUTOINCREMENT,
    reservation_id    TEXT NOT NULL UNIQUE
                      CHECK (
                          length(reservation_id) = 36
                          AND lower(reservation_id) = reservation_id
                          AND reservation_id GLOB '????????-????-7???-[89ab]???-????????????'
                          AND replace(reservation_id, '-', '') NOT GLOB '*[^0-9a-f]*'
                      ),
    user_id           TEXT NOT NULL
                      CHECK (
                          length(user_id) = 36
                          AND lower(user_id) = user_id
                          AND user_id GLOB '????????-????-7???-[89ab]???-????????????'
                          AND replace(user_id, '-', '') NOT GLOB '*[^0-9a-f]*'
                      ),
    conversation_id   TEXT NOT NULL
                      CHECK (
                          length(conversation_id) = 36
                          AND lower(conversation_id) = conversation_id
                          AND conversation_id GLOB '????????-????-7???-[89ab]???-????????????'
                          AND replace(conversation_id, '-', '') NOT GLOB '*[^0-9a-f]*'
                      ),
    turn_id           TEXT NOT NULL
                      CHECK (
                          length(turn_id) = 36
                          AND lower(turn_id) = turn_id
                          AND turn_id GLOB '????????-????-7???-[89ab]???-????????????'
                          AND replace(turn_id, '-', '') NOT GLOB '*[^0-9a-f]*'
                      ),
    turn_generation   INTEGER NOT NULL CHECK (turn_generation >= 0),
    action_identity   TEXT NOT NULL
                      CHECK (
                          length(action_identity) = 64
                          AND lower(action_identity) = action_identity
                          AND action_identity NOT GLOB '*[^0-9a-f]*'
                      ),
    status            TEXT NOT NULL DEFAULT 'reserved'
                      CHECK (status IN ('reserved', 'applied', 'failed')),
    settlement_source TEXT
                      CHECK (
                          settlement_source IS NULL
                          OR settlement_source IN ('execution', 'recovery')
                      ),
    failure_reason    TEXT,
    reserved_at       INTEGER NOT NULL,
    settled_at        INTEGER,
    CHECK (
        (
            status = 'reserved'
            AND settlement_source IS NULL
            AND failure_reason IS NULL
            AND settled_at IS NULL
        )
        OR (
            status = 'applied'
            AND settlement_source = 'execution'
            AND failure_reason IS NULL
            AND settled_at IS NOT NULL
        )
        OR (
            status = 'failed'
            AND settlement_source IN ('execution', 'recovery')
            AND failure_reason IS NOT NULL
            AND length(failure_reason) > 0
            AND settled_at IS NOT NULL
        )
    )
);

CREATE INDEX idx_idmm_action_reservations_user_id
    ON idmm_action_reservations(user_id);
CREATE INDEX idx_idmm_action_reservations_conversation_id
    ON idmm_action_reservations(conversation_id);
CREATE INDEX idx_idmm_action_reservations_turn_id
    ON idmm_action_reservations(turn_id);
CREATE UNIQUE INDEX uq_idmm_action_reservations_exact_action
    ON idmm_action_reservations(
        conversation_id,
        turn_id,
        turn_generation,
        action_identity
    );
