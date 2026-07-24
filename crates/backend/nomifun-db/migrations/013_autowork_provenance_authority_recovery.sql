-- Recover AutoWork claims that an older Conversation admission predicate
-- rejected before receiver admission because it compared Requirement
-- provenance (`created_by = 'user' | 'agent'`) with a Conversation user UUID.
--
-- This migration intentionally recognizes only the exact failure signature.
-- A durable Conversation receipt or terminal admission is permanent evidence
-- that effects may have started, so any row carrying either remains parked for
-- human review. SQLite serializes this UPDATE with receiver admission: either
-- the evidence is written first and the row is not selected, or this statement
-- clears the obsolete capability first and a later admission cannot validate.
UPDATE requirements
SET status = 'pending',
    completion_note = NULL,
    owner_conversation_id = NULL,
    owner_terminal_id = NULL,
    active_turn_started_at = NULL,
    lease_expires_at = NULL,
    attempt_count = MAX(attempt_count - 1, 0),
    claim_token = NULL,
    updated_at = MAX(
        updated_at,
        CAST(strftime('%s', 'now') AS INTEGER) * 1000
    )
WHERE status = 'needs_review'
  AND completion_note = 'AutoWork did not start another turn because durable Conversation state is ambiguous: AutoWork Requirement authority was revoked, superseded, or targets another Conversation. Explicit reset or human review is required.'
  AND created_by IN ('user', 'agent')
  AND owner_conversation_id IS NOT NULL
  AND owner_terminal_id IS NULL
  AND claim_generation >= 1
  AND claim_token IS NOT NULL
  AND attempt_count > 0
  AND active_turn_started_at IS NOT NULL
  AND started_at IS NOT NULL
  AND lease_expires_at IS NULL
  AND completed_at IS NULL
  AND EXISTS (
      SELECT 1
      FROM conversations AS conversation
      WHERE conversation.conversation_id = requirements.owner_conversation_id
        -- This mismatch is the decisive signature of the removed predicate.
        AND conversation.user_id IS NOT requirements.created_by
        AND conversation.status IN ('pending', 'finished')
        AND conversation.active_turn_operation_id IS NULL
        -- Do not override a user who disabled or rebound AutoWork after the
        -- incident. Such rows remain explicit review items.
        AND json_extract(
                conversation.extra,
                '$.autowork.enabled'
            ) = 1
        AND json_extract(
                conversation.extra,
                '$.autowork.tag'
            ) = requirements.tag
  )
  AND NOT EXISTS (
      SELECT 1
      FROM conversation_delivery_receipts AS receipt
      WHERE json_extract(
                receipt.request_payload,
                '$.autowork_authority.requirement_id'
            ) = requirements.requirement_id
        AND json_extract(
                receipt.request_payload,
                '$.autowork_authority.claim_generation'
            ) = requirements.claim_generation
  )
  AND NOT EXISTS (
      SELECT 1
      FROM terminal_turn_admissions AS admission
      WHERE admission.requirement_id = requirements.requirement_id
        AND admission.claim_generation = requirements.claim_generation
  );
