/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import {
  parseConversationArtifactId,
  type ConversationArtifactId,
} from './ids';

/**
 * Stable identity of a row in `conversation_artifacts`.
 *
 * This is intentionally distinct from `PersistedArtifactId`, which identifies
 * durable tool-output receipts embedded in messages.
 */
export { parseConversationArtifactId };
export type { ConversationArtifactId };
