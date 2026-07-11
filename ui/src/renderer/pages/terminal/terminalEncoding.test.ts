/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */
import { describe, expect, test } from 'bun:test';
import { createStreamingDecoder, decodeBase64ToString, encodeStringToBase64 } from './terminalEncoding';

function bytesToBase64(bytes: Uint8Array): string {
  let binary = '';
  bytes.forEach((byte) => {
    binary += String.fromCharCode(byte);
  });
  return btoa(binary);
}

describe('terminal UTF-8 encoding', () => {
  test('round-trips Chinese text and emoji through Base64', () => {
    const text = '中文文件名.md 🍜';
    expect(decodeBase64ToString(encodeStringToBase64(text))).toBe(text);
  });

  test('decodes every possible two-chunk byte split without replacement', () => {
    const text = '终端中文与 emoji：🍜🚀';
    const bytes = new TextEncoder().encode(text);
    for (let split = 1; split < bytes.length; split += 1) {
      const decode = createStreamingDecoder();
      const output =
        decode(bytesToBase64(bytes.slice(0, split))) + decode(bytesToBase64(bytes.slice(split)));
      expect(output).toBe(text);
      expect(output.includes('\uFFFD')).toBe(false);
    }
  });

  test('decodes a stream split into individual bytes', () => {
    const text = '逐字节：中文🍜';
    const decode = createStreamingDecoder();
    const output = [...new TextEncoder().encode(text)]
      .map((byte) => decode(bytesToBase64(Uint8Array.of(byte))))
      .join('');
    expect(output).toBe(text);
  });
});
