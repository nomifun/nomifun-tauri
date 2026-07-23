import { resolveExtensionAssetUrl } from '@/renderer/utils/platform';
import type { PresetListItem } from './types';
import type { PresetTagId } from '@/common/types/ids';

/**
 * Check if a string is an emoji (simple check for common emoji patterns).
 */
export const isEmoji = (str: string): boolean => {
  if (!str) return false;
  const emojiRegex = /^(?:\p{Emoji_Presentation}|\p{Emoji}️)(?:‍(?:\p{Emoji_Presentation}|\p{Emoji}️))*$/u;
  return emojiRegex.test(str);
};

/**
 * Resolve an avatar string to an image src URL, or undefined if it is not an image.
 */
export const resolveAvatarImageSrc = (
  avatar: string | undefined,
  avatarImageMap: Record<string, string>
): string | undefined => {
  const value = avatar?.trim();
  if (!value) return undefined;

  const mapped = avatarImageMap[value];
  if (mapped) return mapped;

  const resolved = resolveExtensionAssetUrl(value) || value;
  const isImage = /\.(svg|png|jpe?g|webp|gif)$/i.test(resolved) || /^(https?:|file:\/\/|data:|\/)/i.test(resolved);
  return isImage ? resolved : undefined;
};

/**
 * Sort presets by sortOrder. The backend already returns sorted lists; this
 * is a deterministic fallback for local reorder operations.
 */
export const sortPresets = (list: PresetListItem[]): PresetListItem[] =>
  [...list].toSorted((a, b) => a.sort_order - b.sort_order);

/** Selected preset-tag business IDs per dimension. Empty = no constraint. */
export type TagFilterState = { audience: PresetTagId[]; scenario: PresetTagId[] };

/**
 * Faceted filter: search text (name + description) AND audience-facet AND
 * scenario-facet. Within a facet, an preset matches if it carries ANY of
 * the selected keys (OR). Empty facet = no constraint.
 */
export const filterPresetsByTags = (
  presets: PresetListItem[],
  query: string,
  tagFilter: TagFilterState,
  localeKey: string
): PresetListItem[] => {
  const normalizedQuery = query.trim().toLowerCase();
  const matchesFacet = (have: PresetTagId[] | undefined, selected: PresetTagId[]) =>
    selected.length === 0 || (have ?? []).some((k) => selected.includes(k));

  return presets.filter((preset) => {
    if (normalizedQuery) {
      const searchableText = [
        preset.name_i18n?.[localeKey] || preset.name,
        preset.description_i18n?.[localeKey] || preset.description || '',
      ]
        .join(' ')
        .toLowerCase();
      if (!searchableText.includes(normalizedQuery)) return false;
    }
    return (
      matchesFacet(preset.audience_tag_ids, tagFilter.audience) &&
      matchesFacet(preset.scenario_tag_ids, tagFilter.scenario)
    );
  });
};
