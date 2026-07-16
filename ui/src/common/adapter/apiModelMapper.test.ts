/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import { describe, expect, test } from 'bun:test';
import { fromApiConversation } from './apiModelMapper';
import { parseRemoteAgentId } from '../types/ids';

// 最小 ApiConversation 片段：只构造 mapper 关心的字段
const apiConv = (o: Record<string, unknown>) => ({
  id: 'conv_0190f5fe-7c00-7a00-8000-000000000001',
  name: 'conv',
  type: 'acp',
  created_at: 1,
  modified_at: 2,
  ...o,
});

type MappedExtra =
  | {
      pinned?: boolean;
      pinned_at?: number;
      custom_workspace?: boolean;
      remote_agent_id?: ReturnType<typeof parseRemoteAgentId>;
      remoteAgentId?: ReturnType<typeof parseRemoteAgentId>;
    }
  | null
  | undefined;
const extraOf = (raw: Record<string, unknown>): MappedExtra => (fromApiConversation(raw) as { extra?: MappedExtra }).extra;

describe('fromApiConversation 置顶镜像（DB 顶层 pinned 列 → extra）', () => {
  test('顶层列置顶 → 镜像进 extra（含服务端维护的 pinned_at）', () => {
    const extra = extraOf(apiConv({ pinned: true, pinned_at: 1712345678000, extra: {} }));
    expect(extra?.pinned).toBe(true);
    expect(extra?.pinned_at).toBe(1712345678000);
  });

  test('extra 为空/缺失时列置顶也能生成镜像', () => {
    const extra = extraOf(apiConv({ pinned: true, pinned_at: 100, extra: null }));
    expect(extra?.pinned).toBe(true);
    expect(extra?.pinned_at).toBe(100);
  });

  test('冲突时列优先：列置顶覆盖 extra.pinned=false，pinned_at 取列值', () => {
    const extra = extraOf(apiConv({ pinned: true, pinned_at: 200, extra: { pinned: false, pinned_at: 999 } }));
    expect(extra?.pinned).toBe(true);
    expect(extra?.pinned_at).toBe(200);
  });

  test('OR 兼容：列未置顶但旧数据仅 extra 置顶 → 不丢，pinned_at 保留 extra 来源', () => {
    const extra = extraOf(apiConv({ pinned: false, extra: { pinned: true, pinned_at: 300 } }));
    expect(extra?.pinned).toBe(true);
    expect(extra?.pinned_at).toBe(300);
  });

  test('两侧均未置顶 → 不注入 pinned key', () => {
    const extra = extraOf(apiConv({ pinned: false, extra: {} }));
    expect(extra && 'pinned' in extra).toBe(false);
  });

  test('列置顶但列 pinned_at 缺失 → 回退 extra.pinned_at', () => {
    const extra = extraOf(apiConv({ pinned: true, extra: { pinned: true, pinned_at: 400 } }));
    expect(extra?.pinned).toBe(true);
    expect(extra?.pinned_at).toBe(400);
  });

  test('custom_workspace 推导不受置顶镜像影响', () => {
    const extra = extraOf(apiConv({ pinned: true, pinned_at: 1, extra: { workspace: '/w/p1' } }));
    expect(extra?.custom_workspace).toBe(true);
    expect(extra?.pinned).toBe(true);
  });

  test('remote_agent_id is mirrored to the legacy remoteAgentId UI key', () => {
    const remoteAgentId = parseRemoteAgentId('ragent_0190f5fe-7c00-7a00-8000-000000000001');
    const extra = extraOf(apiConv({ type: 'remote', extra: { remote_agent_id: remoteAgentId } }));
    expect(extra?.remote_agent_id).toBe(remoteAgentId);
    expect(extra?.remoteAgentId).toBe(remoteAgentId);
  });
});

describe('fromApiConversation 协作方案顶层契约', () => {
  test('保留顶层 execution_template_id，不从旧 extra 回填', () => {
    const currentTemplateId = 'aext_0190f5fe-7c00-7a00-8000-000000000001';
    const topLevel = fromApiConversation(
      apiConv({
        execution_template_id: currentTemplateId,
        extra: { execution_template_id: 'template-stale' },
      }),
    ) as { execution_template_id?: string };
    expect(topLevel.execution_template_id).toBe(currentTemplateId);

    const legacyExtraOnly = fromApiConversation(
      apiConv({ extra: { execution_template_id: 'template-stale' } }),
    ) as { execution_template_id?: string };
    expect(legacyExtraOnly.execution_template_id).toBeUndefined();
  });
});
