/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import { describe, expect, test } from 'bun:test';
import { downmixAudioChannels, encodePcm16Wav, resampleMonoPcm } from './RecordedAudioWav';

const ascii = (view: DataView, offset: number, length: number): string =>
  String.fromCharCode(...Array.from({ length }, (_, index) => view.getUint8(offset + index)));

describe('recorded audio WAV conversion', () => {
  test('encodes a canonical mono PCM16 WAV header and samples', () => {
    const buffer = encodePcm16Wav(new Float32Array([-1, -0.5, 0, 0.5, 1]), 16_000);
    const view = new DataView(buffer);

    expect(ascii(view, 0, 4)).toBe('RIFF');
    expect(view.getUint32(4, true)).toBe(46);
    expect(ascii(view, 8, 4)).toBe('WAVE');
    expect(ascii(view, 12, 4)).toBe('fmt ');
    expect(view.getUint32(16, true)).toBe(16);
    expect(view.getUint16(20, true)).toBe(1);
    expect(view.getUint16(22, true)).toBe(1);
    expect(view.getUint32(24, true)).toBe(16_000);
    expect(view.getUint32(28, true)).toBe(32_000);
    expect(view.getUint16(32, true)).toBe(2);
    expect(view.getUint16(34, true)).toBe(16);
    expect(ascii(view, 36, 4)).toBe('data');
    expect(view.getUint32(40, true)).toBe(10);
    expect(buffer.byteLength).toBe(54);
    expect(view.getInt16(44, true)).toBe(-32_768);
    expect(view.getInt16(46, true)).toBe(-16_384);
    expect(view.getInt16(48, true)).toBe(0);
    expect(view.getInt16(50, true)).toBe(16_384);
    expect(view.getInt16(52, true)).toBe(32_767);
  });

  test('downmixes channels by averaging each sample', () => {
    const mono = downmixAudioChannels([
      new Float32Array([1, -1, 0.5]),
      new Float32Array([-1, 1, -0.5]),
    ]);
    expect(Array.from(mono)).toEqual([0, 0, 0]);
  });

  test('downsamples with weighted interval averages', () => {
    const output = resampleMonoPcm(new Float32Array([0, 1, 2, 3]), 4, 2);
    expect(Array.from(output)).toEqual([0.5, 2.5]);
  });

  test('upsamples with linear interpolation', () => {
    const output = resampleMonoPcm(new Float32Array([0, 1]), 2, 4);
    expect(Array.from(output)).toEqual([0, 0.5, 1, 1]);
  });
});
