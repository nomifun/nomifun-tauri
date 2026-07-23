/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import type { IChannelPluginStatus } from '@/common/types/channel/channel';
import type { ChannelPluginId, CompanionId, PublicAgentId } from '@/common/types/ids';
import type { ChannelPlatform } from '@/renderer/components/settings/SettingsModal/contents/channels/channelTarget';

export interface EnabledChannelStatusQuery {
  platform: ChannelPlatform;
  enabledPluginId?: ChannelPluginId;
  companionId?: CompanionId;
  publicAgentId?: PublicAgentId;
}

export type ChannelConfigTarget = { platform: ChannelPlatform; channelPluginId?: ChannelPluginId } | null;

export interface ChannelOwnerQuery {
  companionId?: CompanionId;
  publicAgentId?: PublicAgentId;
}

const nonEmptyOwnerId = <T extends string>(value: T | null | undefined): T | undefined =>
  value == null || value.length === 0 ? undefined : value;

export function findEnabledChannelStatus(
  statuses: IChannelPluginStatus[],
  query: EnabledChannelStatusQuery
): IChannelPluginStatus | null {
  const enabledPluginId = query.enabledPluginId;
  if (enabledPluginId != null) {
    const byId = statuses.find((status) => status.plugin_id === enabledPluginId);
    if (byId) return byId;
  }

  const companionId = nonEmptyOwnerId(query.companionId);
  const publicAgentId = nonEmptyOwnerId(query.publicAgentId);
  return (
    statuses.find((status) => {
      if (status.type !== query.platform) return false;
      if (publicAgentId) return nonEmptyOwnerId(status.publicAgentId) === publicAgentId;
      if (companionId) return nonEmptyOwnerId(status.companionId) === companionId;
      return false;
    }) ?? null
  );
}

/**
 * When the config modal is in create mode (no channelPluginId), move it onto the
 * entity the caller just resolved. The caller — findEnabledChannelStatus (by the
 * backend-returned business ID) or the owner-scoped adopt effect — already
 * guarantees `status` is the intended entity, so we retarget by business ID rather than
 * re-checking owner equality, which was fragile against id normalization /
 * binding-commit-lag skew and left the toggle stuck OFF after a real success.
 */
export function retargetConfigAfterStatus(
  current: ChannelConfigTarget,
  status: IChannelPluginStatus | null
): ChannelConfigTarget {
  if (!current || current.channelPluginId || !status || status.type !== current.platform) return current;
  return { platform: current.platform, channelPluginId: status.plugin_id };
}

/** Trimmed owner check: does this row currently belong to the given owner? */
export function statusOwnedBy(status: IChannelPluginStatus, owner: ChannelOwnerQuery): boolean {
  const companionId = nonEmptyOwnerId(owner.companionId);
  const publicAgentId = nonEmptyOwnerId(owner.publicAgentId);
  if (publicAgentId) return nonEmptyOwnerId(status.publicAgentId) === publicAgentId;
  if (companionId) return nonEmptyOwnerId(status.companionId) === companionId;
  return false;
}

/** A row with no companion and no public-agent owner (a free, bindable bot). */
export function statusIsUnbound(status: IChannelPluginStatus): boolean {
  return !nonEmptyOwnerId(status.companionId) && !nonEmptyOwnerId(status.publicAgentId);
}
