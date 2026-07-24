/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

function randomBytes(): Uint8Array {
  const bytes = new Uint8Array(16);
  const cryptoObj = globalThis.crypto;
  if (!cryptoObj || typeof cryptoObj.getRandomValues !== 'function') {
    throw new Error('WebCrypto getRandomValues is required to mint stable UUIDv7 business IDs');
  }
  cryptoObj.getRandomValues(bytes);
  return bytes;
}

/** Generate a canonical RFC 9562 UUIDv7 without a runtime dependency. */
export function uuidv7(): string {
  const bytes = randomBytes();
  let timestamp = BigInt(Date.now());
  for (let index = 5; index >= 0; index -= 1) {
    bytes[index] = Number(timestamp & 0xffn);
    timestamp >>= 8n;
  }
  bytes[6] = (bytes[6] & 0x0f) | 0x70;
  bytes[8] = (bytes[8] & 0x3f) | 0x80;
  const hex = Array.from(bytes, (value) => value.toString(16).padStart(2, '0')).join('');
  return `${hex.slice(0, 8)}-${hex.slice(8, 12)}-${hex.slice(12, 16)}-${hex.slice(16, 20)}-${hex.slice(20)}`;
}
