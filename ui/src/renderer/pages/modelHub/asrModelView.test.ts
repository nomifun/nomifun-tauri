/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import { describe, expect, test } from 'bun:test';
import { readFileSync } from 'node:fs';
import { LOCAL_MODEL_CAPABILITIES } from './localModelCapabilityView';
import { localAsrSpeechInputStateKey } from './useLocalAsrModels';

const readSource = (url: URL): string => readFileSync(url, 'utf8');

describe('local ASR UI integration', () => {
  test('exposes speech recognition as an implemented local capability', () => {
    expect(
      LOCAL_MODEL_CAPABILITIES.find((capability) => capability.key === 'speech_recognition')
    ).toEqual({ key: 'speech_recognition', implemented: true });
  });

  test('renders the ASR panel and wires Guid voice input', () => {
    const localModelsContent = readSource(new URL('./LocalModelsContent.tsx', import.meta.url));
    const guidPage = readSource(new URL('../guid/GuidPage.tsx', import.meta.url));

    expect(localModelsContent.includes('<AsrModelsPanel controller={asr} />')).toBe(true);
    expect(guidPage.includes('speechInputNode=')).toBe(true);
    expect(guidPage.includes('<SpeechInputButton')).toBe(true);
  });

  test('keeps simple ASR metadata directly visible without a details disclosure', () => {
    const panel = readSource(new URL('./AsrModelsPanel.tsx', import.meta.url));
    expect(panel.includes('{model.description}')).toBe(true);
    expect(panel.includes('{model.modelSize}')).toBe(true);
    expect(panel.includes('asrEngineLabel(model.engine)')).toBe(true);
    expect(panel.includes("model.engine === 'fun_asr_llama_cpp'")).toBe(true);
    expect(panel.includes('LocalModelDetails')).toBe(false);
    expect(panel.includes('<details')).toBe(false);
  });

  test('publishes speech-input availability changes observed by status polling', () => {
    const hook = readSource(new URL('./useLocalAsrModels.ts', import.meta.url));
    expect(hook.includes('onSuccess: observeStatus')).toBe(true);

    const baseStatus = {
      protocolVersion: '2',
      enabled: false,
      ready: false,
      activeModelId: null,
      runtime: {
        version: null,
        backend: null,
        phase: 'stopped' as const,
        errorKind: null,
        message: null,
      },
      models: [],
      lastError: null,
    };
    expect(localAsrSpeechInputStateKey(baseStatus)).toBe('disabled:not-ready:none');
    expect(
      localAsrSpeechInputStateKey({
        ...baseStatus,
        enabled: true,
        ready: true,
        activeModelId: 'whisper-small-q5-1',
      })
    ).toBe('enabled:ready:whisper-small-q5-1');
  });

  test('activating a local ASR model also selects it for speech input', () => {
    const panel = readSource(new URL('./AsrModelsPanel.tsx', import.meta.url));
    expect(panel.includes("provider: 'local'")).toBe(true);
    expect(panel.includes('saveSpeechToTextConfig')).toBe(true);
    expect(panel.includes("current.provider === 'local' && current.model === modelId")).toBe(true);
  });
});
