import type { SkillInfo } from '@/renderer/pages/settings/AssistantSettings/types';

export type SkillDisplay = {
  name: string;
  description: string;
};

const resolveMapValue = (map: Record<string, string> | undefined, localeKey: string): string | undefined => {
  const exact = map?.[localeKey]?.trim();
  if (exact) return exact;
  const family = localeKey.split('-')[0];
  const familyKey = Object.keys(map ?? {}).find((key) => key.split('-')[0] === family);
  const familyValue = familyKey ? map?.[familyKey]?.trim() : undefined;
  return familyValue || undefined;
};

export const resolveSkillDisplay = (skill: SkillInfo, localeKey: string): SkillDisplay => ({
  name: resolveMapValue(skill.name_i18n, localeKey) || skill.name,
  description: resolveMapValue(skill.description_i18n, localeKey) || skill.description || '',
});
