/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import type { ITerminalSession } from '@/common/adapter/ipcBridge';
import type { TChatConversation } from '@/common/config/storage';
import type { ConversationId, TerminalId } from '@/common/types/ids';
import { DEFAULT_WORKPATH_KEY, workpathKey } from './workpathKey';

export type SessionKind = 'interactive' | 'terminal';

type SessionEntryBase = {
  name: string;
  pinned: boolean;
  /** 未置顶为 0 */
  pinnedAt: number;
  /** conversation.modified_at / terminal.updated_at */
  activityAt: number;
  /** conversation.created_at / terminal.created_at */
  createdAt: number;
};

export type SessionEntry =
  | (SessionEntryBase & {
      kind: 'interactive';
      id: ConversationId;
      conversation: TChatConversation;
    })
  | (SessionEntryBase & {
      kind: 'terminal';
      id: TerminalId;
      terminal: ITerminalSession;
    });

export type WorkpathNode = {
  key: string;
  displayName: string;
  pinned: boolean;
  activityAt: number;
  interactive: Extract<SessionEntry, { kind: 'interactive' }>[];
  terminal: Extract<SessionEntry, { kind: 'terminal' }>[];
};

/**
 * 将交互会话 + 终端会话按 workpath 聚合成两层树。
 * 归属/排序规则见 SessionList 统一重构 spec：
 * - 交互会话 `extra.custom_workspace === true && extra.workspace` → workpathKey(workspace)，否则 default
 * - 终端 `is_default_workpath === true` → default，否则 workpathKey(cwd)
 * - 组内：pinned 在前（pinnedAt 倒序），其余 activityAt 倒序
 * - 节点：pinnedWorkpathKeys 序最前 → default 节点 → 其余 activityAt 倒序
 * - default 节点恒存在；显式传入的空 workpath 节点也会产生
 */
export function buildWorkpathTree(
  conversations: TChatConversation[],
  terminals: ITerminalSession[],
  pinnedWorkpathKeys: string[],
  emptyWorkpaths: string[] = []
): WorkpathNode[] {
  const nodes = new Map<string, WorkpathNode>();
  const ensure = (key: string): WorkpathNode => {
    let n = nodes.get(key);
    if (!n) {
      n = {
        key,
        // default 节点 displayName 先放 key，UI 层覆盖
        displayName: key === DEFAULT_WORKPATH_KEY ? key : (key.split('/').filter(Boolean).pop() ?? key),
        pinned: false,
        activityAt: 0,
        interactive: [],
        terminal: [],
      };
      nodes.set(key, n);
    }
    return n;
  };
  ensure(DEFAULT_WORKPATH_KEY);

  for (const path of emptyWorkpaths) {
    const key = workpathKey(path);
    if (key !== DEFAULT_WORKPATH_KEY) ensure(key);
  }

  for (const c of conversations) {
    const extra = (c.extra ?? {}) as Record<string, unknown>;
    const key = extra.custom_workspace === true && typeof extra.workspace === 'string' ? workpathKey(extra.workspace) : DEFAULT_WORKPATH_KEY;
    ensure(key).interactive.push({
      kind: 'interactive',
      id: c.id,
      name: c.name,
      pinned: c.pinned === true,
      pinnedAt: c.pinned_at ?? 0,
      activityAt: c.modified_at ?? 0,
      createdAt: c.created_at ?? 0,
      conversation: c,
    });
  }
  for (const t of terminals) {
    const key = t.is_default_workpath ? DEFAULT_WORKPATH_KEY : workpathKey(t.cwd);
    ensure(key).terminal.push({
      kind: 'terminal',
      id: t.terminal_id,
      name: t.name,
      pinned: !!t.pinned,
      pinnedAt: t.pinned_at ?? 0,
      activityAt: t.updated_at ?? 0,
      createdAt: t.created_at ?? 0,
      terminal: t,
    });
  }

  const byGroupOrder = (a: SessionEntry, b: SessionEntry) => Number(b.pinned) - Number(a.pinned) || (a.pinned ? b.pinnedAt - a.pinnedAt : 0) || b.activityAt - a.activityAt;

  // 置顶 key 入口处归一化，调用方传原始路径（带尾斜杠等）也不会静默失配
  const pinIndex = new Map(pinnedWorkpathKeys.map((k, i) => [workpathKey(k), i]));
  const result = [...nodes.values()].map((n) => {
    n.interactive.sort(byGroupOrder);
    n.terminal.sort(byGroupOrder);
    n.activityAt = Math.max(0, ...n.interactive.map((s) => s.activityAt), ...n.terminal.map((s) => s.activityAt));
    n.pinned = pinIndex.has(n.key);
    return n;
  });
  result.sort((a, b) => {
    const pa = pinIndex.has(a.key);
    const pb = pinIndex.has(b.key);
    if (pa !== pb) return pa ? -1 : 1;
    if (pa && pb) return pinIndex.get(a.key)! - pinIndex.get(b.key)!;
    const da = a.key === DEFAULT_WORKPATH_KEY;
    const db = b.key === DEFAULT_WORKPATH_KEY;
    if (da !== db) return da ? -1 : 1;
    return b.activityAt - a.activityAt;
  });
  return result;
}
