/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import { describe, expect, test } from 'bun:test';
import {
  buildAgentSkillRows,
  customSkillNamesForImportedAgentSkills,
  defaultSelectedAgentSkillKeys,
  mergeImportedSkillNames,
  summarizeAgentSkillImport,
  type ExternalAgentSkillSource,
} from './agentSkillImportUtils';

const sources: ExternalAgentSkillSource[] = [
  {
    name: 'Claude Skills',
    path: '/Users/muri/.claude/skills',
    source: 'claude',
    skill_count: 2,
    skills: [
      { name: 'research', description: 'Deep research workflow', path: '/Users/muri/.claude/skills/research' },
      { name: 'publish', description: 'Publish content', path: '/Users/muri/.claude/skills/publish' },
    ],
  },
  {
    name: 'Codex / Agent Skills',
    path: '/Users/muri/.agents/skills',
    source: 'agents',
    skill_count: 1,
    skills: [{ name: 'research', description: 'Shared research workflow', path: '/Users/muri/.agents/skills/research' }],
  },
];

describe('agent skill import utilities', () => {
  test('flattens external sources while preserving source and existing-library state', () => {
    const rows = buildAgentSkillRows(sources, new Set(['publish']));

    expect(rows).toHaveLength(3);
    expect(rows.map((row) => row.key)).toEqual([
      'claude::research::/Users/muri/.claude/skills/research',
      'claude::publish::/Users/muri/.claude/skills/publish',
      'agents::research::/Users/muri/.agents/skills/research',
    ]);
    expect(rows[1].alreadyImported).toBe(true);
    expect(rows[2].sourceName).toBe('Codex / Agent Skills');
  });

  test('selects only not-yet-imported skills by default', () => {
    const rows = buildAgentSkillRows(sources, new Set(['publish']));

    expect(defaultSelectedAgentSkillKeys(rows)).toEqual([
      'claude::research::/Users/muri/.claude/skills/research',
      'agents::research::/Users/muri/.agents/skills/research',
    ]);
  });

  test('merges imported names into current preset selection without duplicates', () => {
    expect(mergeImportedSkillNames(['publish'], ['research', 'publish', 'research'])).toEqual([
      'publish',
      'research',
    ]);
  });

  test('summarizes migration progress for library and preset flows', () => {
    const rows = buildAgentSkillRows(sources, new Set(['publish']));

    expect(summarizeAgentSkillImport(rows, rows.slice(0, 2))).toEqual({
      selectedCount: 2,
      alreadyImportedCount: 1,
      importableCount: 1,
    });
  });

  test('adds newly imported and existing custom skills to preset custom list only', () => {
    expect(
      customSkillNamesForImportedAgentSkills(
        [
          {
            name: 'planning-with-files',
            alreadyImported: true,
          },
          {
            name: 'publish',
            alreadyImported: true,
          },
          {
            name: 'research',
            alreadyImported: false,
          },
        ],
        new Set(['publish'])
      )
    ).toEqual(['publish', 'research']);
  });
});
