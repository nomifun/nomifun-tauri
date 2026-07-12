/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import { expect, test } from 'bun:test';
import { readFileSync } from 'node:fs';

const actionModulePath = './ScheduledTaskActions';

async function loadActionModule() {
  try {
    return await import(actionModulePath);
  } catch {
    return {};
  }
}

function readActionSource(): string {
  try {
    return readFileSync(new URL('./ScheduledTaskActions.tsx', import.meta.url), 'utf8');
  } catch {
    return '';
  }
}

test('keeps manual-only jobs remove-only and maps scheduled jobs to their current toggle action', async () => {
  const actionModule = (await loadActionModule()) as {
    getScheduledTaskMenuActions?: (enabled: boolean, isManualOnly: boolean) => string[];
  };
  const getActions = actionModule.getScheduledTaskMenuActions;

  expect(typeof getActions).toBe('function');
  if (!getActions) return;

  expect(getActions(true, false)).toEqual(['pause', 'remove']);
  expect(getActions(false, false)).toEqual(['resume', 'remove']);
  expect(getActions(true, true)).toEqual(['remove']);
  expect(getActions(false, true)).toEqual(['remove']);
});

test('keeps the desktop more trigger visible for row hover, focus, and an open menu', () => {
  const actionSource = readActionSource();

  expect(actionSource.includes("import { DeleteOne, More, PauseOne, PlayOne } from '@icon-park/react'")).toBe(true);
  expect(actionSource.includes('group-hover:opacity-100')).toBe(true);
  expect(actionSource.includes('focus-visible:opacity-100')).toBe(true);
  expect(actionSource.includes("menuVisible && '!pointer-events-auto !opacity-100'")).toBe(true);
  expect(actionSource.includes('onClick={(event) => event.stopPropagation()}')).toBe(true);
  expect(actionSource.includes('setMenuVisible((visible) => !visible);')).toBe(false);
  expect(actionSource.includes('setMenuVisible(true);')).toBe(true);
  expect(actionSource.includes('Modal.confirm({')).toBe(true);
});
