export type SkillDisplay = {
  name: string;
  description: string;
};

/**
 * The display metadata shared by regular, auto-injected, and lightweight skill
 * records. Keep localization resolution here so every UI surface follows the
 * same exact-locale → language-family → canonical fallback order.
 */
export type LocalizableSkill = {
  name: string;
  description?: string;
  name_i18n?: Record<string, string>;
  description_i18n?: Record<string, string>;
};

const resolveMapValue = (map: Record<string, string> | undefined, localeKey: string): string | undefined => {
  const exact = map?.[localeKey]?.trim();
  if (exact) return exact;
  const family = localeKey.toLowerCase().split('-')[0];
  const familyKey = Object.keys(map ?? {}).find((key) => key.toLowerCase().split('-')[0] === family);
  const familyValue = familyKey ? map?.[familyKey]?.trim() : undefined;
  return familyValue || undefined;
};

export const resolveSkillDisplay = (skill: LocalizableSkill, localeKey: string): SkillDisplay => ({
  name: resolveMapValue(skill.name_i18n, localeKey) || skill.name,
  description: resolveMapValue(skill.description_i18n, localeKey) || skill.description || '',
});
