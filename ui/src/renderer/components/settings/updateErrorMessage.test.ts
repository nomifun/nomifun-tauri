/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import { describe, expect, test } from 'bun:test';
import { getUpdateErrorMessageKey } from './updateErrorMessage';

describe('getUpdateErrorMessageKey', () => {
  test('maps invalid remote release JSON errors to the localized feed-unavailable message', () => {
    expect(getUpdateErrorMessageKey('Could not fetch a valid release JSON from the remote')).toBe(
      'update.releaseFeedUnavailable'
    );
  });

  test('keeps unknown updater errors on the generic failure message', () => {
    expect(getUpdateErrorMessageKey('permission denied')).toBe('update.checkFailed');
  });

  test.each([
    'NOMIFUN_UPDATER_AUTO_INSTALL_UNSUPPORTED:mounted_volume',
    'Cross-device link (os error 18)',
    'operation crosses devices',
  ])('maps unsafe macOS install error %s to recovery guidance', (message) => {
    expect(getUpdateErrorMessageKey(message)).toBe('update.crossDeviceInstallUnsupported');
  });
});
