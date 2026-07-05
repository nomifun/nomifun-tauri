# 编排节点配置折叠进底部对话输入框 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 删掉编排节点投影视图里独立的「启动前/重跑配置」大面板，把模型覆盖 + 预置要求折叠进底部对话输入框——单一模型入口、单一主文本框、外加一个「预置要求」pill。

**Architecture:** Settled 节点复用 worker 会话自带的 `NomiSendBox`：增强其模型选择器写透 `override_model`，并在工具栏 `rightTools` 注入一个 `NodePresetPill`。Pending 节点无会话，用一条 `NodeConfigBar`（模型 pill + 预置要求 pill）替代原大面板。后端零改动（`setTaskConfig` + 引擎 override/preset 应用已存在）。

**Tech Stack:** React + TypeScript, arco-design (`Dropdown`/`Input.TextArea`/`Button`), `@icon-park/react`, i18next, UnoCSS 原子类；ipc 经 `ipcBridge.orchestrator.runs.setTaskConfig`。

## Global Constraints

- `setTaskConfig` 是**全量替换**：任何一次写入必须带齐 `{ override_provider_id, override_model, preset_prompt }` 三元组，只改目标字段、其余用 live `task` 的当前值合并，否则会清空另一半配置。
- worker 会话恒为 `nomi` 类型（`worker.rs` 确认），pill 只需注入 `NomiSendBox`。
- 视觉硬门槛（[[ui-must-be-beautiful]]）：pill/popover 沿用 `sendbox-model-btn` + composer popover 视觉语言，与既有输入框对齐。
- 前端无 vitest；每个 task 的验收 = `ui` 目录 `bun run typecheck` 退出码 0（用 `bun run typecheck`，勿用 `npx tsc` 会误报 0，见 [[typecheck-zero-and-arco-conventions]]）。禁 `any`/`ts-ignore`。arco 弹窗消息必经 `useArcoMessage`。
- `@icon-park/react` 具名导入禁起别名（[[icon-park-imports-no-alias]]）。
- 真 `<button>` 会在 WebView2 出黑框——用 `role="button"` 的 `<div>`（[[no-unocss-button-reset]]），或 arco `<Button>`。
- 改 locale 后跑根 `bun run gen:i18n`（若该项目有生成步骤）。
- 提交由用户显式发起（[[pull-before-commit]]）；本计划不含自动 commit 步骤。

---

## File Structure

- **Create** `ui/src/renderer/pages/conversation/orchestration/NodePresetPill.tsx` — 「预置要求」pill + popover（textarea + 保存）。settled/pending 共用。
- **Create** `ui/src/renderer/pages/conversation/orchestration/NodeModelPill.tsx` — 单选模型 pill（写 override）。仅 pending 用。
- **Create** `ui/src/renderer/pages/conversation/orchestration/NodeConfigBar.tsx` — pending 窄配置条 = NodeModelPill + NodePresetPill + 提示。
- **Modify** `ui/src/renderer/pages/conversation/platforms/nomi/NomiSendBox.tsx` — 新增可选 `extraRightTools?: React.ReactNode`，渲染进 `rightTools`。
- **Modify** `ui/src/renderer/pages/conversation/platforms/nomi/NomiChat.tsx` — 新增可选 `extraRightTools` 透传给 `NomiSendBox`。
- **Modify** `ui/src/renderer/pages/orchestrator/RunDetail/ReadOnlyConversationView.tsx` — 新增可选 `nodeBinding` + `extraRightTools`；`NomiReadOnlyChat.onSelectModel` 写透 override。
- **Modify** `ui/src/renderer/pages/conversation/orchestration/ProjectedWorkerView.tsx` — 删折叠面板；settled 传 `nodeBinding` + `<NodePresetPill>`；pending body 换 `<NodeConfigBar>`。
- **Delete** `ui/src/renderer/pages/conversation/orchestration/NodePreconfigPanel.tsx`。
- **Modify** `ui/src/renderer/services/i18n/locales/{en-US,zh-CN}/orchestrator.json` — 新增 pill 短标签，弃用面板专用 key。

**共享类型**：`nodeBinding = { runId: string; taskId: string; task: TRunTask; onSaved: () => void | Promise<void> }`（`TRunTask` 来自 `@/common/types/orchestrator/orchestratorTypes`，含 `override_provider_id`/`override_model`/`preset_prompt`）。

---

### Task 1: `NodePresetPill` 组件

**Files:**
- Create: `ui/src/renderer/pages/conversation/orchestration/NodePresetPill.tsx`

**Interfaces:**
- Produces: `NodePresetPill: React.FC<{ runId: string; taskId: string; task: TRunTask; onSaved: () => void | Promise<void>; className?: string }>` (default export)

- [ ] **Step 1: 实现组件**

```tsx
/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import React, { useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Button, Dropdown, Input } from '@arco-design/web-react';
import { Write } from '@icon-park/react';
import { ipcBridge } from '@/common';
import type { TRunTask } from '@/common/types/orchestrator/orchestratorTypes';
import { iconColors } from '@/renderer/styles/colors';
import { useArcoMessage } from '@/renderer/utils/ui/useArcoMessage';

type NodePresetPillProps = {
  runId: string;
  taskId: string;
  /** Live task — its current override_* are preserved on save (setTaskConfig is a full replace). */
  task: TRunTask;
  onSaved: () => void | Promise<void>;
  className?: string;
};

/**
 * NodePresetPill — the 预置要求 (per-node requirement, appended to the worker brief
 * on the NEXT 重跑/dispatch) folded into a compact composer-toolbar pill. A popover
 * hosts the textarea + inline save. Persists via `setTaskConfig`, PRESERVING the
 * task's current model override (full-replace endpoint).
 */
const NodePresetPill: React.FC<NodePresetPillProps> = ({ runId, taskId, task, onSaved, className }) => {
  const { t } = useTranslation();
  const [message, msgCtx] = useArcoMessage();
  const [open, setOpen] = useState(false);
  const [saving, setSaving] = useState(false);
  const current = task.preset_prompt ?? '';
  const [preset, setPreset] = useState(current);

  // Re-sync when the popover opens or the persisted value changes underneath us.
  React.useEffect(() => {
    if (open) setPreset(task.preset_prompt ?? '');
  }, [open, task.preset_prompt]);

  const dirty = preset !== current;
  const hasPreset = current.trim().length > 0;

  const save = async () => {
    if (saving || !dirty) return;
    setSaving(true);
    try {
      await ipcBridge.orchestrator.runs.setTaskConfig.invoke({
        run_id: runId,
        task_id: taskId,
        updates: {
          override_provider_id: task.override_provider_id ?? undefined,
          override_model: task.override_model ?? undefined,
          preset_prompt: preset.trim() || undefined,
        },
      });
      message.success(t('orchestrator.run.preconfig.savedPending', { defaultValue: '已保存，启动时自动生效' }));
      await onSaved();
      setOpen(false);
    } catch (e) {
      message.error(t('orchestrator.run.preconfig.saveError', { defaultValue: '保存失败：{{error}}', error: String(e) }));
    } finally {
      setSaving(false);
    }
  };

  const panel = (
    <div className='w-320px flex flex-col gap-8px rd-12px bg-[var(--color-bg-popup)] p-12px shadow-[0_8px_24px_rgba(0,0,0,0.12)] border border-solid border-[var(--color-border-2)]'>
      {msgCtx}
      <div className='flex items-center gap-6px text-12px font-600 text-[var(--color-text-1)]'>
        <Write theme='outline' size='13' strokeWidth={3} className='line-height-0' fill='rgb(var(--primary-6))' />
        <span>{t('orchestrator.run.preconfig.presetLabel', { defaultValue: '预置要求' })}</span>
      </div>
      <Input.TextArea
        value={preset}
        onChange={setPreset}
        autoSize={{ minRows: 3, maxRows: 10 }}
        placeholder={t('orchestrator.run.preconfig.presetPlaceholder', {
          defaultValue: '在此写下该节点执行时必须遵守的额外要求/偏好（会追加到该节点的输入，与任务描述分开）。',
        })}
      />
      <div className='flex items-center justify-between gap-8px'>
        <span className='text-11px leading-15px text-[var(--color-text-3)]'>
          {t('orchestrator.run.preconfig.presetPillHint', { defaultValue: '影响该节点下次重跑/启动' })}
        </span>
        <Button type='primary' size='mini' loading={saving} disabled={!dirty} onClick={() => void save()}>
          {t('orchestrator.run.preconfig.save', { defaultValue: '保存配置' })}
        </Button>
      </div>
    </div>
  );

  return (
    <Dropdown trigger='click' popupVisible={open} onVisibleChange={setOpen} droplist={panel} position='tr'>
      <Button
        className={`sendbox-model-btn ${className ?? ''}`}
        shape='round'
        size='small'
        aria-label={t('orchestrator.run.preconfig.presetLabel', { defaultValue: '预置要求' })}
      >
        <span className='flex items-center gap-6px min-w-0'>
          <Write
            theme='outline'
            size='14'
            className='shrink-0'
            fill={hasPreset ? 'rgb(var(--primary-6))' : iconColors.secondary}
          />
          <span className='truncate' style={hasPreset ? { color: 'rgb(var(--primary-6))' } : undefined}>
            {t('orchestrator.run.preconfig.presetPill', { defaultValue: '预置要求' })}
          </span>
        </span>
      </Button>
    </Dropdown>
  );
};

export default NodePresetPill;
```

- [ ] **Step 2: typecheck**

Run: `cd ui && bun run typecheck`
Expected: 退出码 0（确认 `TRunTask` 有 `preset_prompt`/`override_*`、`iconColors.secondary`、`--color-bg-popup` 变量存在；若 `--color-bg-popup` 不存在改用 `--color-bg-3`）。

---

### Task 2: `NodeModelPill` 组件（pending 用）

**Files:**
- Create: `ui/src/renderer/pages/conversation/orchestration/NodeModelPill.tsx`

**Interfaces:**
- Produces: `NodeModelPill: React.FC<{ runId: string; taskId: string; task: TRunTask; onSaved: () => void | Promise<void>; className?: string }>` (default export)

- [ ] **Step 1: 实现组件**（复用 `useModelRange` + `encodePair`/`decodePair` + `FOLLOW_AUTO` 哨兵，写透 override，保留 preset）

```tsx
/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import React, { useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Button, Dropdown } from '@arco-design/web-react';
import { Brain, Down } from '@icon-park/react';
import { ipcBridge } from '@/common';
import type { TModelRef, TRunTask } from '@/common/types/orchestrator/orchestratorTypes';
import NomiSelect from '@/renderer/components/base/NomiSelect';
import { decodePair, encodePair, useModelRange } from '@/renderer/pages/orchestrator/useModelRange';
import { iconColors } from '@/renderer/styles/colors';
import { useArcoMessage } from '@/renderer/utils/ui/useArcoMessage';

const FOLLOW_AUTO = '__follow_auto__';

const filterByLabel = (input: string, option: React.ReactNode): boolean => {
  const children = (option as React.ReactElement<{ children?: React.ReactNode }>)?.props?.children;
  return String(children ?? '').toLowerCase().includes(input.toLowerCase());
};

type NodeModelPillProps = {
  runId: string;
  taskId: string;
  task: TRunTask;
  onSaved: () => void | Promise<void>;
  className?: string;
};

/**
 * NodeModelPill — a single-model override pill for a PENDING node (no worker
 * conversation yet, so no NomiSendBox model selector to reuse). Lists ANY
 * configured provider×model (not just the run's fleet). Persists via
 * `setTaskConfig`, preserving the current preset_prompt.
 */
const NodeModelPill: React.FC<NodeModelPillProps> = ({ runId, taskId, task, onSaved, className }) => {
  const { t } = useTranslation();
  const [message, msgCtx] = useArcoMessage();
  const { providers, getAvailableModels, formatModelLabel, hasModels } = useModelRange();
  const [open, setOpen] = useState(false);

  const value =
    task.override_provider_id && task.override_model
      ? encodePair({ provider_id: task.override_provider_id, model: task.override_model })
      : FOLLOW_AUTO;

  const pillLabel =
    value === FOLLOW_AUTO
      ? t('orchestrator.run.preconfig.followAuto', { defaultValue: '跟随自动路由（不指定）' })
      : (task.override_model ?? '');

  const persist = async (next: string) => {
    const ref: TModelRef | null = next !== FOLLOW_AUTO ? decodePair(next) : null;
    try {
      await ipcBridge.orchestrator.runs.setTaskConfig.invoke({
        run_id: runId,
        task_id: taskId,
        updates: {
          override_provider_id: ref?.provider_id,
          override_model: ref?.model,
          preset_prompt: task.preset_prompt ?? undefined,
        },
      });
      await onSaved();
      setOpen(false);
    } catch (e) {
      message.error(t('orchestrator.run.preconfig.saveError', { defaultValue: '保存失败：{{error}}', error: String(e) }));
    }
  };

  const panel = (
    <div className='w-300px flex flex-col gap-8px rd-12px bg-[var(--color-bg-popup)] p-12px shadow-[0_8px_24px_rgba(0,0,0,0.12)] border border-solid border-[var(--color-border-2)]'>
      {msgCtx}
      <div className='flex items-center gap-6px text-12px font-600 text-[var(--color-text-1)]'>
        <Brain theme='outline' size='13' strokeWidth={3} className='line-height-0' fill='rgb(var(--primary-6))' />
        <span>{t('orchestrator.run.preconfig.modelLabel', { defaultValue: '指定模型' })}</span>
      </div>
      {hasModels ? (
        <NomiSelect value={value} onChange={(v) => void persist(v as string)} showSearch filterOption={filterByLabel} className='w-full'>
          <NomiSelect.Option value={FOLLOW_AUTO}>
            {t('orchestrator.run.preconfig.followAuto', { defaultValue: '跟随自动路由（不指定）' })}
          </NomiSelect.Option>
          {providers.map((p) => (
            <NomiSelect.OptGroup key={p.id} label={p.name || p.platform}>
              {getAvailableModels(p).map((m) => {
                const ref: TModelRef = { provider_id: p.id, model: m };
                return (
                  <NomiSelect.Option key={encodePair(ref)} value={encodePair(ref)}>
                    {formatModelLabel(p, m)}
                  </NomiSelect.Option>
                );
              })}
            </NomiSelect.OptGroup>
          ))}
        </NomiSelect>
      ) : (
        <span className='text-12px leading-18px text-[rgb(var(--warning-6))]'>
          {t('orchestrator.run.preconfig.noModels', { defaultValue: '暂无可用模型，请先在「模型」里配置 provider。' })}
        </span>
      )}
      <span className='text-11px leading-16px text-[var(--color-text-3)]'>
        {t('orchestrator.run.preconfig.modelHint', { defaultValue: '可选任意已配置的模型，不受本次编排创建时所选模型池限制。' })}
      </span>
    </div>
  );

  return (
    <Dropdown trigger='click' popupVisible={open} onVisibleChange={setOpen} droplist={panel} position='tr'>
      <Button className={`sendbox-model-btn ${className ?? ''}`} shape='round' size='small'>
        <span className='flex items-center gap-6px min-w-0'>
          <Brain theme='outline' size='14' className='shrink-0' fill={iconColors.secondary} />
          <span className='truncate max-w-[160px]'>{pillLabel}</span>
          <Down theme='outline' size='12' className='shrink-0' fill={iconColors.secondary} />
        </span>
      </Button>
    </Dropdown>
  );
};

export default NodeModelPill;
```

- [ ] **Step 2: typecheck** — `cd ui && bun run typecheck` → 0。

---

### Task 3: `NodeConfigBar`（pending 窄配置条）

**Files:**
- Create: `ui/src/renderer/pages/conversation/orchestration/NodeConfigBar.tsx`

**Interfaces:**
- Consumes: `NodeModelPill`, `NodePresetPill` (Tasks 1–2)
- Produces: `NodeConfigBar: React.FC<{ runId: string; taskId: string; task: TRunTask; onSaved: () => void | Promise<void> }>` (default export)

- [ ] **Step 1: 实现组件**（撑满 pane、底部一条 composer 造型的 bar + 上方一个空态提示）

```tsx
/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import React from 'react';
import { useTranslation } from 'react-i18next';
import { Comment } from '@icon-park/react';
import type { TRunTask } from '@/common/types/orchestrator/orchestratorTypes';
import NodeModelPill from './NodeModelPill';
import NodePresetPill from './NodePresetPill';

type NodeConfigBarProps = {
  runId: string;
  taskId: string;
  task: TRunTask;
  onSaved: () => void | Promise<void>;
};

/**
 * NodeConfigBar — the PENDING node's 启动前配置 surface. A pending node has no worker
 * conversation (hence no NomiSendBox to reuse), so we mirror the composer's bottom
 * toolbar as a slim bar carrying the SAME two controls (model + 预置要求 pills) that a
 * settled node gets inside its real composer. No text-send — nothing to chat with yet.
 */
const NodeConfigBar: React.FC<NodeConfigBarProps> = ({ runId, taskId, task, onSaved }) => {
  const { t } = useTranslation();
  return (
    <div className='flex flex-1 min-h-0 flex-col'>
      <div className='flex flex-1 min-h-0 flex-col items-center justify-center gap-10px px-20px text-center'>
        <span
          className='flex size-48px items-center justify-center rd-14px'
          style={{
            color: 'rgb(var(--primary-6))',
            background: 'color-mix(in srgb, rgb(var(--primary-6)) 12%, transparent)',
          }}
        >
          <Comment theme='outline' size='24' strokeWidth={3} />
        </span>
        <div className='text-14px font-600 text-[var(--color-text-1)]'>
          {t('orchestrator.run.transcript.notStarted', { defaultValue: '该 agent 尚未开始' })}
        </div>
        <div className='max-w-360px text-12px leading-18px text-[var(--color-text-3)]'>
          {t('orchestrator.run.preconfig.pendingHint', {
            defaultValue: '为该节点指定模型、预置要求；启动时自动生效。',
          })}
        </div>
      </div>
      {/* Composer-shaped config bar */}
      <div className='shrink-0 border-t border-solid border-[var(--color-border-2)] px-16px py-12px'>
        <div className='flex items-center justify-end gap-8px'>
          <NodeModelPill runId={runId} taskId={taskId} task={task} onSaved={onSaved} />
          <NodePresetPill runId={runId} taskId={taskId} task={task} onSaved={onSaved} />
        </div>
      </div>
    </div>
  );
};

export default NodeConfigBar;
```

- [ ] **Step 2: typecheck** — `cd ui && bun run typecheck` → 0。

---

### Task 4: `NomiSendBox` / `NomiChat` 透传 `extraRightTools`

**Files:**
- Modify: `ui/src/renderer/pages/conversation/platforms/nomi/NomiSendBox.tsx`
- Modify: `ui/src/renderer/pages/conversation/platforms/nomi/NomiChat.tsx`

**Interfaces:**
- Produces: `NomiSendBox` + `NomiChat` 均新增可选 prop `extraRightTools?: React.ReactNode`；渲染在 `rightTools` 内、`collaboratorSelectorNode` 与 `AgentModeSelector` 之间（或末尾）。

- [ ] **Step 1: NomiSendBox** — 在其 props 类型加 `extraRightTools?: React.ReactNode;`，解构它，并在 `rightTools` 的那个 `<div ...data-testid='nomi-sendbox-config-group'>` 里、`{collaboratorSelectorNode}` 之后加一行 `{extraRightTools}`：

```tsx
// props 类型里（collaboratorSelectorNode 附近）新增：
extraRightTools?: React.ReactNode;
// 解构参数里新增 extraRightTools,
// rightTools 内：
              {collaboratorSelectorNode}
              {extraRightTools}
              <AgentModeSelector
```

- [ ] **Step 2: NomiChat** — 在其 props 类型加 `extraRightTools?: React.ReactNode;`，解构，透传给 `<NomiSendBox ... extraRightTools={extraRightTools} />`：

```tsx
// props 类型（collaboratorSelectorNode 后）：
  extraRightTools?: React.ReactNode;
// 解构参数里新增 extraRightTools,
// NomiSendBox 调用处：
              collaboratorSelectorNode={collaboratorSelectorNode}
              extraRightTools={extraRightTools}
```

- [ ] **Step 3: typecheck** — `cd ui && bun run typecheck` → 0。

---

### Task 5: `ReadOnlyConversationView` 接 `nodeBinding` + `extraRightTools`（模型写透）

**Files:**
- Modify: `ui/src/renderer/pages/orchestrator/RunDetail/ReadOnlyConversationView.tsx`

**Interfaces:**
- Consumes: `NomiChat.extraRightTools`（Task 4）
- Produces: `ReadOnlyConversationView` 新增两个可选 prop：`nodeBinding?: { runId: string; taskId: string; task: TRunTask; onSaved: () => void | Promise<void> }`、`extraRightTools?: React.ReactNode`。仅 `nomi` 分支消费；其它平台忽略（保持零回归）。

- [ ] **Step 1: 增强 `NomiReadOnlyChat`**——新增 `nodeBinding`/`extraRightTools` props；`onSelectModel` 成功后若有 `nodeBinding` 则写透 override（保留 preset）；把 `extraRightTools` 传给 `NomiChat`。

```tsx
// import 增补：
import type { TRunTask } from '@/common/types/orchestrator/orchestratorTypes';

// nodeBinding 类型（文件内定义并 export 供 ProjectedWorkerView 复用）：
export type OrchestratorNodeBinding = {
  runId: string;
  taskId: string;
  task: TRunTask;
  onSaved: () => void | Promise<void>;
};

// NomiReadOnlyChat props 增加 nodeBinding?/extraRightTools?：
const NomiReadOnlyChat: React.FC<{
  conversation: NomiConversation;
  agent_name?: string;
  hideSendBox?: boolean;
  nodeBinding?: OrchestratorNodeBinding;
  extraRightTools?: React.ReactNode;
}> = ({ conversation, agent_name, hideSendBox, nodeBinding, extraRightTools }) => {
  const onSelectModel = useCallback(
    async (_provider: IProvider, modelName: string) => {
      const selected = { ..._provider, use_model: modelName } as TProviderWithModel;
      const ok = await ipcBridge.conversation.update.invoke({ id: conversation.id, updates: { model: selected } });
      if (ok) {
        void saveNomiDefaultModel(_provider.id, modelName);
        // 写透 per-node override —— 改一次模型同时定 live 会话 + 下次重跑；保留 preset。
        if (nodeBinding) {
          try {
            await ipcBridge.orchestrator.runs.setTaskConfig.invoke({
              run_id: nodeBinding.runId,
              task_id: nodeBinding.taskId,
              updates: {
                override_provider_id: _provider.id,
                override_model: modelName,
                preset_prompt: nodeBinding.task.preset_prompt ?? undefined,
              },
            });
            await nodeBinding.onSaved();
          } catch (e) {
            console.error('[NomiReadOnlyChat] write-through node override failed:', e);
          }
        }
      }
      return Boolean(ok);
    },
    [conversation.id, nodeBinding]
  );

  const modelSelection = useNomiModelSelection({ initialModel: conversation.model, onSelectModel });

  return (
    <NomiChat
      conversation_id={conversation.id}
      workspace={conversation.extra.workspace}
      modelSelection={modelSelection}
      agent_name={agent_name}
      hideSendBox={hideSendBox}
      extraRightTools={extraRightTools}
    />
  );
};
```

- [ ] **Step 2: `ReadOnlyConversationView` props + nomi 分支透传**

```tsx
type ReadOnlyConversationViewProps = {
  conversation: TChatConversation;
  hideSendBox?: boolean;
  agent_name?: string;
  nodeBinding?: OrchestratorNodeBinding;
  extraRightTools?: React.ReactNode;
};
// 解构 nodeBinding/extraRightTools；仅 'nomi' case 传入：
      case 'nomi':
        return (
          <NomiReadOnlyChat
            key={conversation.id}
            conversation={conversation as NomiConversation}
            agent_name={agent_name}
            hideSendBox={hideSendBox}
            nodeBinding={nodeBinding}
            extraRightTools={extraRightTools}
          />
        );
```

- [ ] **Step 3: typecheck** — `cd ui && bun run typecheck` → 0。

---

### Task 6: `ProjectedWorkerView` 重接线（删面板 / settled 折叠进输入框 / pending 换 bar）

**Files:**
- Modify: `ui/src/renderer/pages/conversation/orchestration/ProjectedWorkerView.tsx`

**Interfaces:**
- Consumes: `NodeConfigBar`（Task 3）、`NodePresetPill`（Task 1）、`ReadOnlyConversationView` 的 `nodeBinding`/`extraRightTools`（Task 5）。

- [ ] **Step 1: 换 import** — 删 `import NodePreconfigPanel from './NodePreconfigPanel';` 与 `SettingOne`（若不再用）；加：

```tsx
import NodeConfigBar from './NodeConfigBar';
import NodePresetPill from './NodePresetPill';
```

- [ ] **Step 2: 删除 `configOpen` state + 折叠区**——移除 `const [configOpen, setConfigOpen] = useState(false);`。构造 nodeBinding：

```tsx
const nodeBinding = useMemo(
  () => ({ runId, taskId: task.id, task, onSaved: payload.refetch }),
  [runId, task, payload.refetch]
);
```

- [ ] **Step 3: 改写 body 三分支**——pending 用 `NodeConfigBar`；settled 去掉折叠壳、给 `ReadOnlyConversationView` 传 `nodeBinding` + `extraRightTools={<NodePresetPill .../>}`；load 失败分支也换 `NodeConfigBar`：

```tsx
        <RouteErrorBoundary>
          {conversationId === undefined ? (
            canConfig ? (
              <NodeConfigBar runId={runId} taskId={task.id} task={task} onSaved={payload.refetch} />
            ) : (
              <div className={styles.center}>
                <span className={styles.emptyIcon}>
                  <Comment theme='outline' size='26' strokeWidth={3} />
                </span>
                <div className={styles.emptyTitle}>
                  {t('orchestrator.run.transcript.notStarted', { defaultValue: '该 agent 尚未开始' })}
                </div>
                <div className={styles.emptyHint}>
                  {t('orchestrator.run.transcript.noConversation', {
                    defaultValue: '该节点还没有被 worker 接手,暂无可查看的会话记录。',
                  })}
                </div>
              </div>
            )
          ) : loading ? (
            <Spin loading className='flex flex-1 items-center justify-center' />
          ) : conversation ? (
            <ReadOnlyConversationView
              conversation={conversation}
              agent_name={task.title}
              nodeBinding={canConfig ? nodeBinding : undefined}
              extraRightTools={
                canConfig ? (
                  <NodePresetPill runId={runId} taskId={task.id} task={task} onSaved={payload.refetch} />
                ) : undefined
              }
            />
          ) : canConfig ? (
            <NodeConfigBar runId={runId} taskId={task.id} task={task} onSaved={payload.refetch} />
          ) : null}
        </RouteErrorBoundary>
```

- [ ] **Step 4: typecheck** — `cd ui && bun run typecheck` → 0。确认 `SettingOne`/`Down`（若仅折叠用）等 import 不再残留未用（`bun run typecheck` 对 unused import 视 tsconfig `noUnusedLocals` 而定；若报错则删除）。

---

### Task 7: 删除 `NodePreconfigPanel` + i18n

**Files:**
- Delete: `ui/src/renderer/pages/conversation/orchestration/NodePreconfigPanel.tsx`
- Modify: `ui/src/renderer/services/i18n/locales/en-US/orchestrator.json`
- Modify: `ui/src/renderer/services/i18n/locales/zh-CN/orchestrator.json`

- [ ] **Step 1: 确认无残留引用** — `rg "NodePreconfigPanel" ui/src` 应仅剩自身；删文件后应为空。

- [ ] **Step 2: 删除文件** `NodePreconfigPanel.tsx`。

- [ ] **Step 3: i18n** — 在两个 locale 的 `orchestrator.run.preconfig` 下新增：
  - `zh-CN`: `"presetPill": "预置要求"`, `"presetPillHint": "影响该节点下次重跑/启动"`, `"pendingHint": "为该节点指定模型、预置要求；启动时自动生效。"`
  - `en-US`: 对应英文（`"presetPill": "Preset"`, `"presetPillHint": "Applies on this node's next re-run/start"`, `"pendingHint": "Set this node's model and preset requirements; applied automatically at start."`）
  - 可选清理（若确认无其它引用）：`rerunConfig`/`subtitlePending`/`subtitleSettled`/`footerPending`/`footerSettled`。保留 `title`/`modelLabel`/`followAuto`/`noModels`/`modelHint`/`presetLabel`/`presetPlaceholder`/`save`/`saving`/`savedPending`/`savedRerun`/`saveError`（仍被新组件用）。

- [ ] **Step 4: 若项目有 i18n 生成** — 跑根 `bun run gen:i18n`。

- [ ] **Step 5: typecheck** — `cd ui && bun run typecheck` → 0。

---

## 最终验收

- [ ] `rg "NodePreconfigPanel" ui/src` → 无匹配。
- [ ] `cd ui && bun run typecheck` → 退出码 0。
- [ ] 后端无改动（`git status` 中 crates/ 无本 feature 新增改动）。
- [ ] 用户真机视觉验收：
  - Settled 节点：无独立「重跑配置」面板；模型只剩输入框自带的一个选择器，改它同时影响 live 聊天与下次重跑；工具栏有「预置要求」pill，点开可编辑+保存，有内容时高亮。
  - Pending 节点：窄配置条替代大面板，模型 pill + 预置要求 pill 可用并落库，保存不再被裁。

## Self-Review

- **Spec coverage**: 删面板(Task 6/7) / 模型合一写透(Task 5) / 预置 pill(Task 1,6) / pending bar(Task 3,6) / 透传链(Task 4,5) / i18n(Task 7) —— 全覆盖。
- **Placeholder scan**: 无 TBD/TODO；每个代码步给出完整代码。
- **Type consistency**: `OrchestratorNodeBinding`（Task 5 定义）被 Task 6 消费；`extraRightTools: React.ReactNode` 贯穿 Task 4/5/6；`setTaskConfig` 三元组合并在 Task 1/2/5 一致。
- **风险**: `--color-bg-popup` 变量名需在 Task 1/2 typecheck/视觉时确认（不存在则换 `--color-bg-3`）；`noUnusedLocals` 可能因删折叠区遗留未用 import 报错（Task 6 Step 4 处理）。
