/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import { describe, expect, test } from 'bun:test';
import { fromApiConversation } from '../../../../../common/adapter/apiModelMapper';
import { DEFAULT_WORKPATH_KEY } from './workpathKey';
import { buildWorkpathTree } from './workpathTree';

const conv = (o: Record<string, unknown>) =>
  ({ id: o.id ?? 'c1', name: o.name ?? 'conv', modified_at: o.modified_at ?? 100, extra: o.extra ?? {}, type: 'acp', created_at: o.created_at ?? 1 }) as never;
const term = (o: Record<string, unknown>) =>
  ({ id: o.id ?? 't1', name: o.name ?? 'term', cwd: o.cwd ?? '/w', created_at: o.created_at ?? 2, updated_at: o.updated_at ?? 100, pinned: o.pinned ?? false, pinned_at: o.pinned_at, is_default_workpath: o.is_default_workpath ?? false }) as never;

describe('buildWorkpathTree', () => {
  test('custom_workspace 会话归 workpath，其余归 default；default 节点恒存在', () => {
    const tree = buildWorkpathTree([conv({ id: 'a', extra: { workspace: '/w/p1/', custom_workspace: true } }), conv({ id: 'b', extra: { workspace: '/tmp/x-temp-b' } })], [], []);
    const def = tree.find((n) => n.key === DEFAULT_WORKPATH_KEY)!;
    const p1 = tree.find((n) => n.key === '/w/p1')!;
    expect(p1.interactive.map((s) => s.id)).toEqual(['a']);
    expect(def.interactive.map((s) => s.id)).toEqual(['b']);
  });
  test('cron 会话不被排除', () => {
    const tree = buildWorkpathTree([conv({ id: 'cron1', extra: { workspace: '/w/p1', custom_workspace: true, cron_job_id: 'j1' } })], [], []);
    expect(tree.find((n) => n.key === '/w/p1')!.interactive).toHaveLength(1);
  });
  test('终端按 cwd 聚合，与同路径会话同节点；is_default_workpath 归 default', () => {
    const tree = buildWorkpathTree([conv({ id: 'a', extra: { workspace: '/w/p1', custom_workspace: true } })], [term({ id: 't1', cwd: '/w/p1/' }), term({ id: 't2', is_default_workpath: true })], []);
    const p1 = tree.find((n) => n.key === '/w/p1')!;
    expect(p1.terminal.map((s) => s.id)).toEqual(['t1']);
    expect(tree.find((n) => n.key === DEFAULT_WORKPATH_KEY)!.terminal.map((s) => s.id)).toEqual(['t2']);
  });
  test('entry 保留会话创建时间用于侧边栏年龄字段', () => {
    const tree = buildWorkpathTree(
      [conv({ id: 'c-age', created_at: 1_000, extra: { workspace: '/w/p1', custom_workspace: true } })],
      [term({ id: 't-age', cwd: '/w/p1', created_at: 2_000 })],
      []
    );
    const node = tree.find((n) => n.key === '/w/p1')!;

    expect(node.interactive[0].createdAt).toBe(1_000);
    expect(node.terminal[0].createdAt).toBe(2_000);
  });
  test('组内排序：pinned(pinnedAt 倒序) 在前，余者 activity 倒序', () => {
    const node = buildWorkpathTree(
      [conv({ id: 'old', modified_at: 10, extra: { workspace: '/p', custom_workspace: true } }), conv({ id: 'new', modified_at: 90, extra: { workspace: '/p', custom_workspace: true } }), conv({ id: 'pin1', modified_at: 50, extra: { workspace: '/p', custom_workspace: true, pinned: true, pinned_at: 1 } }), conv({ id: 'pin2', modified_at: 40, extra: { workspace: '/p', custom_workspace: true, pinned: true, pinned_at: 2 } })],
      [],
      []
    );
    expect(node.find((n) => n.key === '/p')!.interactive.map((s) => s.id)).toEqual(['pin2', 'pin1', 'new', 'old']);
  });
  test('节点排序：置顶序 → default → activity 倒序', () => {
    const tree = buildWorkpathTree([conv({ id: 'a', modified_at: 10, extra: { workspace: '/p-old', custom_workspace: true } }), conv({ id: 'b', modified_at: 99, extra: { workspace: '/p-new', custom_workspace: true } }), conv({ id: 'c', modified_at: 5, extra: { workspace: '/p-pin', custom_workspace: true } })], [], ['/p-pin']);
    expect(tree.map((n) => n.key)).toEqual(['/p-pin', DEFAULT_WORKPATH_KEY, '/p-new', '/p-old']);
  });
  test('置顶 key 未归一化（带尾斜杠）也能命中节点', () => {
    const tree = buildWorkpathTree([conv({ id: 'a', extra: { workspace: '/p-pin', custom_workspace: true } })], [], ['/p-pin/']);
    expect(tree[0].key).toBe('/p-pin');
    expect(tree[0].pinned).toBe(true);
  });
  test('displayName 取路径末段', () => {
    const tree = buildWorkpathTree([conv({ extra: { workspace: '/Users/a/my-proj', custom_workspace: true } })], [], []);
    expect(tree.find((n) => n.key !== DEFAULT_WORKPATH_KEY)!.displayName).toBe('my-proj');
  });
  test('显式创建但还没有会话的 workpath 也会显示为空项目节点', () => {
    const tree = buildWorkpathTree([], [], [], ['/Users/a/empty-project/']);
    const project = tree.find((n) => n.key === '/Users/a/empty-project')!;

    expect(project.displayName).toBe('empty-project');
    expect(project.interactive).toHaveLength(0);
    expect(project.terminal).toHaveLength(0);
  });
  test('DB 顶层 pinned 列与 extra 冲突时列优先（经 fromApiConversation 镜像后入树）', () => {
    // 列说置顶（extra 说没置顶）→ 置顶；pinned_at 取列值
    const colPinnedId = 'conv_0190f5fe-7c00-7a00-8000-000000000001';
    const colPinned = fromApiConversation({ ...(conv({ id: colPinnedId, modified_at: 10, extra: { workspace: '/p', custom_workspace: true, pinned: false, pinned_at: 999 } }) as Record<string, unknown>), pinned: true, pinned_at: 500 });
    const plain = conv({ id: 'plain', modified_at: 90, extra: { workspace: '/p', custom_workspace: true } });
    const node = buildWorkpathTree([plain, colPinned as never], [], []).find((n) => n.key === '/p')!;
    expect(node.interactive.map((s) => s.id)).toEqual([colPinnedId, 'plain']);
    expect(node.interactive[0].pinnedAt).toBe(500);
  });
});
