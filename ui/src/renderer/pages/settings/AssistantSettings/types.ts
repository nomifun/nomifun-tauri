import type { Assistant } from '@/common/types/agent/assistantTypes';

// Skill info type
export type SkillSource = 'builtin' | 'custom' | 'extension';

export type SkillInfo = {
  name: string;
  description: string;
  name_i18n?: Record<string, string>;
  description_i18n?: Record<string, string>;
  location: string;
  relative_location?: string;
  is_custom: boolean;
  source: SkillSource;
  // Tag keys referencing the shared assistant tag vocabulary. Resolved at the
  // route layer from the user sidecar table (?? built-in seed ?? empty).
  audience_tags?: string[];
  scenario_tags?: string[];
};

// Pending skill to import
export type PendingSkill = {
  path: string;
  name: string;
  description: string;
};

// Builtin auto-injected skill info
export type BuiltinAutoSkill = {
  name: string;
  description: string;
};

export type AssistantListItem = Assistant;
