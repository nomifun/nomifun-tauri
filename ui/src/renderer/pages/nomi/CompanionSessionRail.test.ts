/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import { readFileSync } from 'node:fs';
import { describe, expect, test } from 'bun:test';
import React from 'react';
import { renderToStaticMarkup } from 'react-dom/server';
import type { ICompanionWithStatus } from '@/common/adapter/ipcBridge';
import CompanionSessionRail from './CompanionSessionRail';

const source = readFileSync(new URL('./CompanionSessionRail.tsx', import.meta.url), 'utf8');

describe('CompanionSessionRail layout', () => {
  test('renders a freshly created companion without a configured model', () => {
    const companion = {
      id: 'companion_0198f6b1-0ef0-7000-8000-000000000001',
      name: 'Fresh companion',
      character: 'mochi',
      persona: {},
      model: null,
      appearance: {},
      created_at: 0,
      status: {
        companion_id: 'companion_0198f6b1-0ef0-7000-8000-000000000001',
        xp: 0,
        level: 1,
        mood: 'content',
        memories_active: 0,
        memories_archived: 0,
        suggestions_new: 0,
        skills_active: 0,
        model_configured: false,
        collect_any_enabled: false,
      },
    } as ICompanionWithStatus;

    const html = renderToStaticMarkup(
      React.createElement(CompanionSessionRail, {
        companions: [companion],
        selectedId: companion.id,
        onSelect: () => undefined,
        onCreated: () => undefined,
        onDeleted: () => undefined,
      })
    );

    expect(html.includes('Fresh companion')).toBe(true);
  });

  test('places the create companion entry above the companion roster', () => {
    const createEntry = source.indexOf('onClick={openCreate}');
    const roster = source.indexOf('{companions.map((p) => {');

    expect(createEntry).toBeGreaterThan(-1);
    expect(roster).toBeGreaterThan(-1);
    expect(createEntry).toBeLessThan(roster);
  });

  test('renders the create companion entry as the selected card-style design', () => {
    const createEntry = source.slice(source.indexOf('onClick={openCreate}'), source.indexOf('<div className=\'flex-1'));

    expect(createEntry.includes('w-30px h-30px')).toBe(true);
    expect(createEntry.includes('shadow-[0_5px_12px_rgba(var(--primary-rgb),0.22)]')).toBe(true);
    expect(createEntry.includes("t('nomi.companions.create')")).toBe(true);
    expect(createEntry.includes("t('nomi.companions.createHint')")).toBe(true);
  });

  test('awaits the page refresh before showing the creation success message', () => {
    const refresh = source.indexOf('await onCreated(profile)');
    const success = source.indexOf("Message.success(t('nomi.companions.created'");

    expect(refresh).toBeGreaterThan(-1);
    expect(success).toBeGreaterThan(-1);
    expect(refresh).toBeLessThan(success);
  });

  test('reports a roster refresh failure without treating companion creation as failed', () => {
    const refresh = source.indexOf('await onCreated(profile)');
    const refreshCatch = source.indexOf('catch (refreshError)');
    const warning = source.indexOf('Message.warning(');
    const genericCreateCatch = source.indexOf('} catch (e) {');
    const refreshFailureBranch = source.slice(refreshCatch, genericCreateCatch);

    expect(refreshCatch).toBeGreaterThan(refresh);
    expect(warning).toBeGreaterThan(refreshCatch);
    expect(warning).toBeLessThan(genericCreateCatch);
    expect(refreshFailureBranch.includes("t('nomi.companions.created'")).toBe(true);
    expect(refreshFailureBranch.includes('String(refreshError)')).toBe(true);
  });
});
