-- Durable, fail-closed admission fence for an automatic turn written into a
-- PTY. A requirement claim alone proves ownership of work, but it cannot prove
-- whether bytes crossed the PTY boundary before a crash. This receipt is never
-- TTL-evicted: admitted/body_written/effects_started rows remain absorbing
-- forever. `body_written` is the special two-part TUI state where prompt bytes
-- reached the PTY but the submit key has not been authorized or written.
CREATE TABLE terminal_turn_admissions (
    id                  INTEGER PRIMARY KEY AUTOINCREMENT,
    turn_token          TEXT NOT NULL UNIQUE
                        CHECK (
                            length(turn_token) = 36
                            AND lower(turn_token) = turn_token
                            AND turn_token GLOB '????????-????-7???-[89ab]???-????????????'
                            AND replace(turn_token, '-', '') NOT GLOB '*[^0-9a-f]*'
                        ),
    terminal_id         TEXT NOT NULL
                        CHECK (
                            length(terminal_id) = 36
                            AND lower(terminal_id) = terminal_id
                            AND terminal_id GLOB '????????-????-7???-[89ab]???-????????????'
                            AND replace(terminal_id, '-', '') NOT GLOB '*[^0-9a-f]*'
                        ),
    pty_epoch           INTEGER NOT NULL CHECK (pty_epoch >= 0),
    requirement_id      TEXT NOT NULL
                        CHECK (
                            length(requirement_id) = 36
                            AND lower(requirement_id) = requirement_id
                            AND requirement_id GLOB '????????-????-7???-[89ab]???-????????????'
                            AND replace(requirement_id, '-', '') NOT GLOB '*[^0-9a-f]*'
                        ),
    claim_generation    INTEGER NOT NULL CHECK (claim_generation >= 1),
    phase               TEXT NOT NULL DEFAULT 'admitted'
                        CHECK (
                            phase IN (
                                'admitted',
                                'body_written',
                                'effects_started',
                                'settled'
                            )
                        ),
    outcome             TEXT
                        CHECK (
                            outcome IS NULL
                            OR outcome IN ('done', 'failed', 'needs_review', 'cancelled')
                        ),
    detail              TEXT,
    admitted_at         INTEGER NOT NULL,
    effects_started_at  INTEGER,
    settled_at          INTEGER,
    CHECK (
        (
            phase = 'admitted'
            AND outcome IS NULL
            AND effects_started_at IS NULL
            AND settled_at IS NULL
        )
        OR (
            phase = 'body_written'
            AND outcome IS NULL
            AND effects_started_at IS NULL
            AND settled_at IS NULL
        )
        OR (
            phase = 'effects_started'
            AND outcome IS NULL
            AND effects_started_at IS NOT NULL
            AND settled_at IS NULL
        )
        OR (
            phase = 'settled'
            AND outcome IS NOT NULL
            AND settled_at IS NOT NULL
        )
    )
);

CREATE UNIQUE INDEX uq_terminal_turn_admissions_exact_claim
    ON terminal_turn_admissions(
        terminal_id,
        pty_epoch,
        requirement_id,
        claim_generation
    );

CREATE INDEX idx_terminal_turn_admissions_requirement
    ON terminal_turn_admissions(requirement_id, claim_generation);

-- A Requirement claim generation is a single logical attempt. Relaunching the
-- PTY may change `pty_epoch`, but it must never mint a second right to execute
-- that same attempt.
CREATE UNIQUE INDEX uq_terminal_turn_admissions_requirement_claim
    ON terminal_turn_admissions(requirement_id, claim_generation);

CREATE INDEX idx_terminal_turn_admissions_terminal_epoch
    ON terminal_turn_admissions(terminal_id, pty_epoch);
