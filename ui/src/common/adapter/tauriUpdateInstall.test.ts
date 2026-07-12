/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import { describe, expect, test } from 'bun:test';
import {
  AUTO_INSTALL_UNSUPPORTED_ERROR,
  installUpdateWithPreflight,
  type UpdaterInstallContext,
} from './tauriUpdateInstall';

const safe: UpdaterInstallContext = {
  platform: 'macos',
  appBundlePath: '/Applications/NomiFun.app',
  tempDir: '/private/var/folders/tmp',
  appDeviceId: 7,
  tempDeviceId: 7,
  autoInstallSupported: true,
  reason: null,
};

describe('installUpdateWithPreflight', () => {
  test('safe context installs and then relaunches', async () => {
    const calls: string[] = [];

    await installUpdateWithPreflight({
      getContext: async () => safe,
      install: async () => void calls.push('install'),
      relaunch: async () => void calls.push('relaunch'),
    });

    expect(calls).toEqual(['install', 'relaunch']);
  });

  test('unsafe context never calls install or relaunch', async () => {
    const calls: string[] = [];
    const result = installUpdateWithPreflight({
      getContext: async () => ({ ...safe, autoInstallSupported: false, reason: 'mounted_volume' }),
      install: async () => void calls.push('install'),
      relaunch: async () => void calls.push('relaunch'),
    });

    let errorMessage = '';
    try {
      await result;
    } catch (error) {
      errorMessage = error instanceof Error ? error.message : String(error);
    }

    expect(errorMessage).toBe(`${AUTO_INSTALL_UNSUPPORTED_ERROR}:mounted_volume`);
    expect(calls).toEqual([]);
  });
});
