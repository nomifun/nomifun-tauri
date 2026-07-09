/**
 * Faceted filter for the Skills Hub grid. Mirrors the assistant page's
 * `filterAssistantsByTags` semantics: search text (name + description) AND
 * audience-facet AND scenario-facet. Within a facet, a skill matches if it
 * carries ANY of the selected keys (OR). An empty facet imposes no constraint.
 */
import type { SkillInfo } from '@/renderer/pages/settings/AssistantSettings/types';
import { resolveSkillDisplay } from './skillDisplay';

/** Selected tag keys per dimension. Empty array = no constraint on that dimension. */
export type SkillTagFilterState = { audience: string[]; scenario: string[] };

export const filterSkillsByTags = (
  skills: SkillInfo[],
  query: string,
  tagFilter: SkillTagFilterState,
  localeKey = 'en-US'
): SkillInfo[] => {
  const q = query.trim().toLowerCase();
  const matchesFacet = (have: string[] | undefined, selected: string[]) =>
    selected.length === 0 || (have ?? []).some((k) => selected.includes(k));
  return skills.filter((s) => {
    if (q) {
      const display = resolveSkillDisplay(s, localeKey);
      const text = `${s.name} ${s.description ?? ''} ${display.name} ${display.description}`.toLowerCase();
      if (!text.includes(q)) return false;
    }
    return matchesFacet(s.audience_tags, tagFilter.audience) && matchesFacet(s.scenario_tags, tagFilter.scenario);
  });
};
