/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import { useMemo } from 'react';
import { useTranslation } from 'react-i18next';
import useSWR from 'swr';
import { ipcBridge } from '@/common';
import type { TChatConversation } from '@/common/config/storage';
import type { Preset, PresetReference, ResolvedPresetSnapshot } from '@/common/types/agent/presetTypes';
import CoworkLogo from '@/renderer/assets/icons/cowork.svg';
import { resolveExtensionAssetUrl } from '@/renderer/utils/platform';

export interface PresetInfo {
  preset_id: PresetReference;
  name: string;
  logo: string;
  isEmoji: boolean;
  revision?: number;
}

export function resolvePresetConfigId(conversation: TChatConversation): PresetReference | null {
  return conversation.preset_id ?? null;
}

export function resolvePresetSnapshot(conversation: TChatConversation): ResolvedPresetSnapshot | null {
  const value = conversation.preset_snapshot;
  if (!value || typeof value !== 'object') return null;
  const candidate = value as Partial<ResolvedPresetSnapshot>;
  return typeof candidate.preset_id === 'string' && typeof candidate.preset_name === 'string'
    ? (candidate as ResolvedPresetSnapshot)
    : null;
}

function normalizeAvatar(avatar: string | undefined): { logo: string; isEmoji: boolean } {
  const value = avatar?.trim() || '';
  if (!value) return { logo: '◆', isEmoji: true };
  if (value === 'cowork.svg') return { logo: CoworkLogo, isEmoji: false };
  const resolved = resolveExtensionAssetUrl(value) || value;
  const isImage = /\.(svg|png|jpe?g|webp|gif)$/i.test(resolved) || /^(https?:|file:\/\/|data:|\/)/i.test(resolved);
  return isImage ? { logo: resolved, isEmoji: false } : { logo: value, isEmoji: true };
}

export function usePresetInfo(conversation: TChatConversation | undefined): {
  info: PresetInfo | null;
  isLoading: boolean;
} {
  const { i18n } = useTranslation();
  const presetId = conversation ? resolvePresetConfigId(conversation) : null;
  const snapshot = conversation ? resolvePresetSnapshot(conversation) : null;
  const { data: preset, isLoading } = useSWR<Preset | null>(presetId ? `preset.${presetId}` : null, async () => {
    if (!presetId) return null;
    try {
      return await ipcBridge.presets.get.invoke({ preset_id: presetId });
    } catch {
      return null;
    }
  });

  return useMemo(() => {
    if (!presetId) return { info: null, isLoading: false };
    const locale = i18n.language || 'en-US';
    const name = preset?.name_i18n?.[locale] || preset?.name || snapshot?.preset_name || presetId;
    const avatar = normalizeAvatar(preset?.avatar);
    return {
      info: {
        preset_id: presetId,
        name,
        logo: avatar.logo,
        isEmoji: avatar.isEmoji,
        revision: snapshot?.preset_revision ?? preset?.revision,
      },
      isLoading: !snapshot && isLoading,
    };
  }, [i18n.language, isLoading, preset, presetId, snapshot]);
}
