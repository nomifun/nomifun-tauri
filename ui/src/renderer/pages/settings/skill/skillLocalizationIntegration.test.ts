/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import { describe, expect, test } from 'bun:test';
import { readFileSync } from 'node:fs';

const readSource = (url: URL) => readFileSync(url, 'utf8');

describe('skill localization integration', () => {
  test('preset editor resolves every persisted skill source through the shared display helper', () => {
    const source = readSource(new URL('../PresetSettings/PresetEditDrawer.tsx', import.meta.url));

    expect(source.includes('const LocalizedSkillContent')).toBe(true);
    expect(source.includes('resolveSkillDisplay(skill, localeKey)')).toBe(true);
    expect(source.match(/<LocalizedSkillContent/g)?.length).toBe(4);
    expect(source.includes("t('settings.pending'")).toBe(true);
  });

  test('guid skill surfaces retain and render localized metadata', () => {
    const page = readSource(new URL('../../guid/GuidPage.tsx', import.meta.url));
    const drawer = readSource(new URL('../../guid/components/PresetPickerDrawer.tsx', import.meta.url));
    const card = readSource(new URL('../../guid/components/DrawerSkillCard.tsx', import.meta.url));
    const popover = readSource(new URL('../../guid/components/ComposerEntryStrip.tsx', import.meta.url));

    expect(page.includes('name_i18n: s.name_i18n')).toBe(true);
    expect(page.includes('description_i18n: s.description_i18n')).toBe(true);
    expect(drawer.includes('filterSkillsByTags(skillInfos, query, tagFilter as SkillTagFilterState, localeKey)')).toBe(
      true
    );
    expect(drawer.includes('localeKey={localeKey}')).toBe(true);
    expect(card.includes('resolveSkillDisplay(skill, localeKey)')).toBe(true);
    expect(card.includes('tag.label_i18n?.[localeKey] || tag.label')).toBe(true);
    expect(popover.includes('resolveSkillDisplay(skill, localeKey)')).toBe(true);
  });
});
