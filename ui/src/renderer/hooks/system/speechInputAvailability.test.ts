/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import { describe, expect, test } from 'bun:test';
import { getSpeechInputAvailabilityForEnvironment } from './speechInputAvailability';

const availableMediaApis = {
  hasMediaDevices: true,
  hasMediaRecorder: true,
};

describe('speech input availability', () => {
  test('uses the system microphone in the desktop shell', () => {
    expect(
      getSpeechInputAvailabilityForEnvironment({
        ...availableMediaApis,
        hostname: 'tauri.localhost',
        isDesktopShell: true,
        isSecureContext: false,
      })
    ).toBe('record');
  });

  test('uses the microphone in secure and localhost browser contexts', () => {
    expect(
      getSpeechInputAvailabilityForEnvironment({
        ...availableMediaApis,
        hostname: 'nomifun.example',
        isDesktopShell: false,
        isSecureContext: true,
      })
    ).toBe('record');
    expect(
      getSpeechInputAvailabilityForEnvironment({
        ...availableMediaApis,
        hostname: 'localhost',
        isDesktopShell: false,
        isSecureContext: false,
      })
    ).toBe('record');
  });

  test('never degrades the microphone control into file upload', () => {
    expect(
      getSpeechInputAvailabilityForEnvironment({
        ...availableMediaApis,
        hostname: '192.168.1.20',
        isDesktopShell: false,
        isSecureContext: false,
      })
    ).toBe('unsupported');
    expect(
      getSpeechInputAvailabilityForEnvironment({
        hasMediaDevices: false,
        hasMediaRecorder: false,
        hostname: 'tauri.localhost',
        isDesktopShell: true,
        isSecureContext: false,
      })
    ).toBe('unsupported');
  });
});
