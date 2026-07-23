import { describe, expect, test } from 'bun:test';
import { readFileSync } from 'node:fs';

const readSource = (url: URL): string => readFileSync(url, 'utf8');

describe('Guid collaboration templates', () => {
  test('keeps saved authoring data on the canonical template API', () => {
    const bridge = readSource(new URL('../../../common/adapter/ipcBridge.ts', import.meta.url));
    expect(bridge.includes("'/api/agent-execution-templates'")).toBe(true);
    expect(bridge.includes('/create-execution')).toBe(true);
  });

  test('passes only the selected template input into a normal Nomi conversation', () => {
    const page = readSource(new URL('./GuidPage.tsx', import.meta.url));
    const send = readSource(new URL('./hooks/useGuidSend.ts', import.meta.url));
    const picker = readSource(new URL('./components/GuidCollaborationTemplatePicker.tsx', import.meta.url));
    const bridge = readSource(new URL('../../../common/adapter/ipcBridge.ts', import.meta.url));
    const storage = readSource(new URL('../../../common/config/storage.ts', import.meta.url));
    const searchMapper = readSource(new URL('../../../common/adapter/searchMapper.ts', import.meta.url));
    const conversation = readSource(new URL('../conversation/components/ChatConversation.tsx', import.meta.url));

    expect(page.includes('selectedCollaborationTemplate?.execution_template_id')).toBe(true);
    expect(send.includes('execution_template_id: executionTemplateId')).toBe(true);
    expect(
      /decision_policy: decisionPolicy,\s*execution_template_id: executionTemplateId,\s*extra: \{/.test(send),
    ).toBe(true);
    expect(picker.includes('agentExecutionTemplate.get.invoke')).toBe(true);
    expect(picker.includes('agentExecutionTemplate.create.invoke')).toBe(true);
    expect(picker.includes('agentExecutionTemplate.remove.invoke')).toBe(true);
    expect(picker.includes('!templateContainsModel(detail, mainModel)')).toBe(true);
    expect(page.includes('selectedCollaborationTemplate.models.some')).toBe(true);
    expect(bridge.includes('body.execution_template_id = p.execution_template_id')).toBe(true);
    expect(storage.includes('execution_template_id?: ExecutionTemplateId | null')).toBe(true);
    expect(searchMapper.includes('execution_template_id?: string | null')).toBe(true);
    expect(conversation.includes('conversation.execution_template_id ?? null')).toBe(true);
    expect(conversation.includes('execution_template_id: next?.id ?? null')).toBe(true);

    const legacyExtraRead = /extra(?:\?\.|\.)execution_template_id|extra\[['"]execution_template_id['"]\]/;
    for (const source of [bridge, storage, searchMapper, send, conversation]) {
      expect(legacyExtraRead.test(source)).toBe(false);
    }
  });
});
