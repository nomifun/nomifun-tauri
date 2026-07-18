import { readFileSync } from 'node:fs';
import { describe, expect, test } from 'bun:test';

const source = readFileSync(new URL('./index.tsx', import.meta.url), 'utf8');

describe('authoritative conversation deletion route handling', () => {
  test('clears the matching active conversation and replaces its deleted route', () => {
    expect(source.includes("if (event.conversation_id !== conversationId)")).toBe(true);
    expect(source.includes("if (event.action === 'deleted')")).toBe(true);
    expect(source.includes("emitter.emit('conversation.deleted', conversationId)")).toBe(true);
    expect(source.includes('void mutate(undefined, { revalidate: false })')).toBe(true);
    expect(source.includes("void navigate('/guid', { replace: true })")).toBe(true);
  });

  test('does not turn non-deleted list changes into local deletion success', () => {
    const deletedBranch = source.indexOf("if (event.action === 'deleted')");
    const localDelete = source.indexOf("emitter.emit('conversation.deleted', conversationId)");
    const ordinaryBranch = source.indexOf("if (event.action !== 'updated' && event.action !== 'created')");

    expect(deletedBranch).toBeGreaterThan(-1);
    expect(localDelete).toBeGreaterThan(deletedBranch);
    expect(localDelete).toBeLessThan(ordinaryBranch);
  });
});
