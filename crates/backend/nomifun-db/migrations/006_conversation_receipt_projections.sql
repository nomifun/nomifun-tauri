-- Conversation delivery receipts are permanent replay evidence. Their
-- operation scope must survive transcript reset/clear, while their optional
-- projections into the current Conversation/message aggregate must be
-- detachable before those projections are deleted.
ALTER TABLE conversation_delivery_receipts
    ADD COLUMN projected_conversation_id TEXT
        CHECK (
            projected_conversation_id IS NULL
            OR (
                length(projected_conversation_id) = 36
                AND lower(projected_conversation_id) = projected_conversation_id
                AND projected_conversation_id GLOB '????????-????-7???-[89ab]???-????????????'
                AND replace(projected_conversation_id, '-', '') NOT GLOB '*[^0-9a-f]*'
            )
        );

ALTER TABLE conversation_delivery_receipts
    ADD COLUMN projected_message_id TEXT
        CHECK (
            projected_message_id IS NULL
            OR (
                length(projected_message_id) = 36
                AND lower(projected_message_id) = projected_message_id
                AND projected_message_id GLOB '????????-????-7???-[89ab]???-????????????'
                AND replace(projected_message_id, '-', '') NOT GLOB '*[^0-9a-f]*'
            )
        );

UPDATE conversation_delivery_receipts
SET projected_conversation_id = conversation_id,
    projected_message_id = CASE
        WHEN EXISTS (
            SELECT 1 FROM messages
            WHERE messages.message_id = conversation_delivery_receipts.message_id
              AND messages.conversation_id = conversation_delivery_receipts.conversation_id
        )
        THEN message_id
        ELSE NULL
    END;

DROP INDEX idx_delivery_receipts_message_id;
DROP INDEX idx_delivery_receipts_conversation_id;

CREATE INDEX idx_delivery_receipts_conversation_id
    ON conversation_delivery_receipts(projected_conversation_id);
CREATE UNIQUE INDEX idx_delivery_receipts_message_id
    ON conversation_delivery_receipts(projected_message_id)
    WHERE projected_message_id IS NOT NULL;
