/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import { describe, expect, test } from 'bun:test';
import { readFileSync } from 'node:fs';

const readSource = (url: URL): string => readFileSync(url, 'utf8');

describe('speech input recording handling', () => {
  test('keeps the microphone button dedicated to live recording', () => {
    const button = readSource(
      new URL('../../components/chat/SpeechInputButton.tsx', import.meta.url)
    );

    expect(button.includes("type='file'")).toBe(false);
    expect(button.includes('fileInputRef')).toBe(false);
    expect(button.includes('transcribeFile')).toBe(false);
  });

  test('declares the macOS microphone privacy reason required by the desktop WebView', () => {
    const plist = readSource(new URL('../../../../../apps/desktop/Info.plist', import.meta.url));

    expect(plist.includes('<key>NSMicrophoneUsageDescription</key>')).toBe(true);
    expect(plist.includes('voice input')).toBe(true);
  });

  test('submits the browser recording to the configured cloud service', () => {
    const hook = readSource(new URL('./useSpeechInput.ts', import.meta.url));

    expect(hook.includes('await transcribeBlob(blob)')).toBe(true);
    expect(hook.includes('convertRecordedAudioToWav')).toBe(false);
  });

  test('uses the shared speech-to-text configuration event constant', () => {
    const button = readSource(
      new URL('../../components/chat/SpeechInputButton.tsx', import.meta.url)
    );

    expect(button.includes('SPEECH_TO_TEXT_CONFIG_CHANGED_EVENT,')).toBe(true);
    expect(button.includes("from '@/renderer/services/speechToTextConfig';")).toBe(true);
    expect(
      button.includes(
        "const SPEECH_TO_TEXT_CONFIG_CHANGED_EVENT = 'nomifun:speech-to-text-config-changed';"
      )
    ).toBe(false);
  });

  test('does not expose a stale cloud speech selection', () => {
    const button = readSource(
      new URL('../../components/chat/SpeechInputButton.tsx', import.meta.url)
    );

    expect(button.includes('selectedCloudProvider.models.includes(config.model)')).toBe(true);
    expect(button.includes('selectedCloudProvider.model_enabled?.[config.model] !== false')).toBe(true);
  });

  test('does not opt the desktop multipart request into credentialed CORS', () => {
    const service = readSource(
      new URL('../../services/SpeechToTextService.ts', import.meta.url)
    );

    expect(service.includes('xhr.withCredentials = true')).toBe(false);
    expect(service.includes("buildBackendAuthHeaders('POST')")).toBe(true);
  });
});
