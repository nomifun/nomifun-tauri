import { describe, expect, test } from 'bun:test';
import { sanitizeFileName } from './conversationExport';

describe('sanitizeFileName', () => {
  test('replaces Windows-invalid punctuation and control characters', () => {
    expect(sanitizeFileName('OpenCode\r\n\r\ninstall:*?"<>|')).toBe('OpenCode_install_');
  });
});
