/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

/**
 * Input parameters for determining slash command list availability.
 */
export interface SlashCommandListAvailabilityInput {
  /** Current persisted conversation type. */
  conversation_type?: string;
}

/**
 * Determines whether the slash command autocomplete list should be enabled.
 *
 * Slash commands are supported by ACP and nomi agent types. The backend's
 * `/slash-commands` endpoint returns an empty list for other agent types
 * (openclaw-gateway / nanobot / remote), so calling it from those is waste
 * (and additionally 404s when the agent has not been warmed up yet).
 *
 * @param input - Conversation type and status information
 * @returns true if slash commands should be enabled
 */
export function isSlashCommandListEnabled(input: SlashCommandListAvailabilityInput): boolean {
  return input.conversation_type === 'acp' || input.conversation_type === 'nomi';
}
