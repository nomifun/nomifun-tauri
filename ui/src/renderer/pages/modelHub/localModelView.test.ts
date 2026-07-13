/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import { describe, expect, test } from 'bun:test';
import type { LocalModelServiceStatus, LocalModelState } from '@/common/types/provider/localModelService';
import {
  canDeleteLocalModel,
  emptyLocalModelState,
  isLocalModelActivityPending,
  localModelPrimaryAction,
  localModelProgressPercent,
} from './localModelView';

const state = (installPhase: LocalModelState['installPhase']): LocalModelState => ({
  ...emptyLocalModelState('model-a'),
  installPhase,
});

const status = (model: LocalModelState, runtimePhase: LocalModelServiceStatus['runtime']['phase'] = 'stopped'):
  LocalModelServiceStatus => ({
    kind: 'local',
    protocolVersion: '1',
    providerId: 'nomifun-local-model',
    enabled: false,
    ready: false,
    activeModelId: null,
    runtime: {
      version: null,
      backend: null,
      phase: runtimePhase,
      errorKind: null,
      message: null,
    },
    models: [model],
    lastError: null,
  });

describe('local model view state', () => {
  test('maps every install phase to its primary action', () => {
    expect(localModelPrimaryAction(state('not_installed'), false)).toBe('install');
    expect(localModelPrimaryAction(state('downloading'), false)).toBe('cancel');
    expect(localModelPrimaryAction(state('verifying'), false)).toBe('cancel');
    expect(localModelPrimaryAction(state('paused'), false)).toBe('resume');
    expect(localModelPrimaryAction(state('failed'), false)).toBe('retry');
    expect(localModelPrimaryAction(state('installed'), false)).toBe('activate');
    expect(localModelPrimaryAction(state('installed'), true)).toBe('deactivate');
  });

  test('only permits deletion of inactive retained files', () => {
    expect(canDeleteLocalModel(state('installed'), false)).toBe(true);
    expect(canDeleteLocalModel(state('paused'), false)).toBe(true);
    expect(canDeleteLocalModel(state('failed'), false)).toBe(true);
    expect(canDeleteLocalModel(state('installed'), true)).toBe(false);
    expect(canDeleteLocalModel(state('downloading'), false)).toBe(false);
  });

  test('clamps transfer progress and handles an unknown total', () => {
    expect(localModelProgressPercent(null)).toBeNull();
    expect(localModelProgressPercent({ component: 'model', downloadedBytes: 20, totalBytes: 0, bytesPerSecond: 0 })).toBeNull();
    expect(localModelProgressPercent({ component: 'model', downloadedBytes: 25, totalBytes: 100, bytesPerSecond: 1 })).toBe(25);
    expect(localModelProgressPercent({ component: 'model', downloadedBytes: 120, totalBytes: 100, bytesPerSecond: 1 })).toBe(100);
  });

  test('uses fast polling only while transfer or runtime transitions are active', () => {
    expect(isLocalModelActivityPending(status(state('downloading')))).toBe(true);
    expect(isLocalModelActivityPending(status(state('verifying')))).toBe(true);
    expect(isLocalModelActivityPending(status(state('installed'), 'starting'))).toBe(true);
    expect(isLocalModelActivityPending(status(state('installed'), 'stopping'))).toBe(true);
    expect(isLocalModelActivityPending(status(state('installed'), 'ready'))).toBe(false);
  });
});
