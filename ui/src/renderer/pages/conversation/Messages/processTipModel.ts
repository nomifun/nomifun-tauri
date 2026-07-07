/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import type { IMessageTips } from '@/common/chat/chatLib';
import { toDisplayText } from '@/common/chat/displayText';

const CONTEXT_COMPRESSION_PATTERN = /\b(?:microcompact|autocompact|context compaction|context compact|compact(?:ed|ion)?)\b/i;

export const isContextCompressionTip = (message: IMessageTips): boolean =>
  CONTEXT_COMPRESSION_PATTERN.test(toDisplayText(message.content.content));
