/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

/**
 * 渠道行寻址目标（多机器人模型）。
 *
 * Addresses one channel row (`assistant_plugins` row) for the per-owner
 * multi-bot flows:
 * - `channelId` present → update that row (`achn_` prefixed id, or a legacy
 *   platform-name id migrated from the single-bot era);
 * - `channelId` absent → create mode: the first enable creates a new row of
 *   the form's platform bound to its owner (backend rejects with 409 when the
 *   same bot is already bound to another owner).
 *
 * The bind owner is EITHER a desktop companion (`companionId`) OR a public
 * agent (`publicAgentId`) — exactly one is set; they are mutually exclusive
 * (a channel bot serves a single object). The enable call forwards whichever
 * one is present as `companion_id` / `public_agent_id`.
 *
 * Forms that receive no `channelTarget` keep the legacy global-settings
 * behavior (one implicit row per platform, addressed by platform name).
 */
export interface ChannelTarget {
  channelId?: string;
  companionId?: string;
  publicAgentId?: string;
}

/** Builtin IM platforms a companion can connect (the channel config forms cover this set). */
export type MasterAgentPlatform = 'telegram' | 'lark' | 'dingtalk' | 'weixin' | 'wecom' | 'discord' | 'slack' | 'matrix' | 'mattermost' | 'twitch' | 'nostr' | 'qqbot';
