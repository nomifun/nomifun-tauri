/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import { describe, expect, test } from 'bun:test';
import type { ImageModelServiceStatus, ImageModelState } from '@/common/types/provider/imageModelService';
import {
  canDeleteImageModel,
  emptyImageModelState,
  imageModelPrimaryAction,
  imageModelProgressPercent,
  imageModelProgressTotals,
  isImageModelActivityPending,
} from './imageModelView';

const state = (installPhase: ImageModelState['installPhase']): ImageModelState => ({
  ...emptyImageModelState('z-image-turbo-q3-k'),
  installPhase,
});

const status = (model: ImageModelState, runtimePhase: ImageModelServiceStatus['runtimePhase'] = 'unavailable'):
  ImageModelServiceStatus => ({
    protocolVersion: '1',
    artifactsReady: model.installPhase === 'installed',
    inferenceReady: false,
    runtimePhase,
    models: [model],
    lastError: null,
  });

describe('local image model view state', () => {
  test('maps install phases to resumable actions', () => {
    expect(imageModelPrimaryAction(state('not_installed'))).toBe('install');
    expect(imageModelPrimaryAction(state('downloading'))).toBe('pause');
    expect(imageModelPrimaryAction(state('verifying'))).toBe('pause');
    expect(imageModelPrimaryAction(state('extracting'))).toBe('pause');
    expect(imageModelPrimaryAction(state('paused'))).toBe('resume');
    expect(imageModelPrimaryAction(state('failed'))).toBe('retry');
    expect(imageModelPrimaryAction(state('installed'))).toBe('none');
    expect(imageModelPrimaryAction({ ...state('failed'), errorKind: 'unsupported_platform' })).toBe('none');
  });

  test('deletion requires retained local files', () => {
    expect(canDeleteImageModel(state('installed'))).toBe(true);
    expect(canDeleteImageModel(state('paused'))).toBe(true);
    expect(canDeleteImageModel(state('not_installed'))).toBe(false);
    expect(canDeleteImageModel(state('failed'))).toBe(false);
    const failedWithPartial = state('failed');
    failedWithPartial.componentProgress[1].downloadedBytes = 10;
    expect(canDeleteImageModel(failedWithPartial)).toBe(true);
  });

  test('aggregates all four stable component rows and clamps percentages', () => {
    const downloading = state('downloading');
    downloading.componentProgress[0] = {
      ...downloading.componentProgress[0],
      downloadedBytes: 50,
      totalBytes: 100,
      bytesPerSecond: 5,
    };
    downloading.componentProgress[1] = {
      ...downloading.componentProgress[1],
      downloadedBytes: 25,
      totalBytes: 100,
      bytesPerSecond: 10,
    };
    expect(imageModelProgressTotals(downloading)).toEqual({
      downloadedBytes: 75,
      totalBytes: 200,
      bytesPerSecond: 15,
    });
    expect(imageModelProgressPercent(75, 200)).toBe(37.5);
    expect(imageModelProgressPercent(250, 200)).toBe(100);
    expect(imageModelProgressPercent(1, 0)).toBeNull();
  });

  test('polls quickly only during install work or image generation', () => {
    expect(isImageModelActivityPending(status(state('downloading')))).toBe(true);
    expect(isImageModelActivityPending(status(state('verifying')))).toBe(true);
    expect(isImageModelActivityPending(status(state('extracting')))).toBe(true);
    expect(isImageModelActivityPending(status(state('installed'), 'busy'))).toBe(true);
    expect(isImageModelActivityPending(status(state('installed'), 'ready'))).toBe(false);
  });
});
