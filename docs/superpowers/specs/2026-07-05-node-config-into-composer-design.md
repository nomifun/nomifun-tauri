# 编排节点配置折叠进底部对话输入框

**日期**: 2026-07-05
**状态**: 设计已批准，待实现
**表面**: 会话原生编排 / 节点投影视图（`ProjectedWorkerView`）

## 问题

在「查看某个 DAG 节点」的投影视图里，节点配置（模型覆盖 + 预置要求）以一个独立大面板呈现（`NodePreconfigPanel`），对 settled（已运行）节点它是折叠在 transcript 上方的「重跑配置（模型/预置要求）」。实测三个 UI 问题：

1. **保存按钮太靠下**：settled 折叠区是 `max-h-340px overflow-y-auto`，模型选择器 + hint + 预置要求 textarea 之后的「保存配置」按钮被裁到折叠区底部，很多用户看不到。
2. **两个模型选择器不联动、冗余**：面板的「指定模型」写 `override_model`（下次重跑用），底部输入框自带的模型 pill 写 `conversation.model`（当前 worker 会话下一轮），两个入口互不关联。
3. **两个文本框重叠**：面板的「预置要求」textarea 与底部对话输入框在视觉/心智上重合。

## 关键语义澄清（非显而易见）

面板与底部输入框是**两套机制**，不是纯重复：

| | 「重跑配置」面板 | 底部对话输入框（NomiSendBox） |
|---|---|---|
| 模型 | `override_provider_id`+`override_model`：下次「重跑」用的模型（任意 provider，不受 fleet 池限制） | `conversation.model`：当前 worker 会话下一轮聊天用的模型 |
| 文本 | `preset_prompt`：下次「重跑」时以「用户预置要求」段追加进 worker brief | 一条即时聊天消息（续用现有 worker 会话，之后可「采用为该节点产出」） |
| 生效时机 | 点「重跑」/ 首次派发（全新 worker） | 立即（续用现有会话） |

后端（已存在，本次不改）：
- `engine.rs` 派发时若 `override_provider_id`+`override_model` 均非空，覆写解析出成员的 provider/model（pending 派发 / rerun / loop 统一走这里）。
- `compose_brief` 若 `preset_prompt` 非空，追加「用户预置要求(请优先遵守)」段。
- `setTaskConfig`（`orchestrator.runs.setTaskConfig`）为全量替换：清空模型选择即恢复自动路由。

**可合并的点**：底部输入框的模型选择器本来就列**全部**已配置模型，范围与面板「指定模型」一致（都满足"不受 fleet 池限制"）。因此模型可真正合一。

## 方案（已选：预置要求折叠成输入框内 pill）

删掉独立大面板，配置融进底部输入框：**单一模型入口、单一主文本框、外加一个「预置要求」pill**。后端零改动。

### Settled 节点（有 nomi worker 会话）

worker 恒为 `nomi` 会话（`worker.rs` 确认），底部输入框永远是 `NomiSendBox`。

1. **删除**折叠式「重跑配置」头 + 内嵌的 `NodePreconfigPanel`（`ProjectedWorkerView.tsx`）。
2. **模型合一**：增强 `ReadOnlyConversationView` → `NomiReadOnlyChat` 的 `onSelectModel`——当传入了 orchestrator 节点绑定时，改模型除写 `conversation.update({ model })` 外，**同时** `setTaskConfig({ override_provider_id, override_model })`。→ 只剩输入框自带的一个模型选择器，改一次即同时定 live 会话模型 + 下次重跑模型。
3. **预置要求 → pill**：在 `NomiSendBox` 工具栏 `rightTools`（紧挨模型/权限 pill）注入一个「预置要求」pill（`NodePresetPill`）。点开为一个 popover：textarea + 内联「保存」（`setTaskConfig({ preset_prompt })`）。有预置内容时 pill 呈高亮/激活态以保证可发现性。popover 内一行说明"影响下次重跑"。

### Pending 节点（无会话）

pending 没有会话故无底部输入框。把原 `NodePreconfigPanel` body 换成一条**输入框造型的窄配置条**（`NodeConfigBar`），承载：单选模型 pill（`NodeModelPill`，写 override）+ 同一个 `NodePresetPill` + 一行提示"该节点启动时自动应用此配置"。无发送动作（尚无 worker 可聊）。→ pending 与 settled 视觉一致，大面板彻底消失。

## 组件拆分

- **`NodePresetPill`**（新）：pill + popover（textarea + 保存）。settled、pending 共用。props: `{ runId, taskId, task, onSaved }`。
- **`NodeModelPill`**（新）：单选模型 pill（`useModelRange` 的 providers/getAvailableModels/formatModelLabel + encodePair/decodePair + `FOLLOW_AUTO` 哨兵），写 override。**仅 pending** 用；settled 复用增强后的 `NomiModelSelector`。
- **`NodeConfigBar`**（新）：pending 的窄配置条，组合 `NodeModelPill` + `NodePresetPill` + 提示。
- **删除** `NodePreconfigPanel.tsx`。

### prop 透传链（settled）

- `ProjectedWorkerView` 传可选 `nodeBinding = { runId, taskId, onSaved }` 给 `ReadOnlyConversationView`。
- `ReadOnlyConversationView` → `NomiReadOnlyChat`：接收 `nodeBinding`，在 `onSelectModel` 里写透 override；并把 `<NodePresetPill .../>` 作为 `extraRightTools`（新增可选 prop，仿 `collaboratorSelectorNode`）透传给 `NomiChat` → `NomiSendBox`（`rightTools` 内追加）。
- 所有新 prop **可选**，不影响 `ReadOnlyConversationView` 的其它用途（如 transcript 镜像）。

## i18n

复用 `orchestrator.run.preconfig.*`：`title`/`presetLabel`/`presetPlaceholder`/`save`/`saving`/`savedPending`/`savedRerun`/`saveError`/`modelLabel`/`followAuto`/`noModels`/`modelHint`。新增 pill 短标签（如 `presetPill`「预置要求」、`presetPillHint`「影响下次重跑」）。弃用 `rerunConfig`/`subtitlePending`/`subtitleSettled`/`footerPending`/`footerSettled`。en-US + zh-CN 同步。改 locale 后按惯例跑根 `bun run gen:i18n`（如涉及生成）。

## 边界与取舍

- **非 nomi worker**：不存在（worker 恒为 nomi）。若将来出现，pill 仅在 nomi 分支注入，其它分支自然不显示（可接受）。
- **模型写透的时序**：输入框显示 worker 实际用的 `conversation.model`；改它会连带写 override，以最新一次改动为准。若此前 override 被单独预设成别的值，会被覆盖——可接受，popover/说明里点明。
- **视觉硬门槛**：pill/popover 沿用现有 `sendbox-model-btn` + composer popover 视觉语言（`orchestratorComposer.module.css` / arco Dropdown），必须漂亮、与既有输入框语言对齐。
- **无前端单测**：验收＝`ui` 目录 `bun run typecheck` 退出码 0 + 用户真机视觉验收。

## 验收标准

1. Settled 节点投影视图：无独立「重跑配置」面板；模型只剩一个（输入框自带）选择器，改它同时影响 live 聊天与下次重跑；工具栏出现「预置要求」pill，点开可编辑+保存，有内容时高亮。
2. Pending 节点：窄配置条替代大面板，模型 pill + 预置要求 pill 可用并落库，保存按钮不再被裁。
3. `NodePreconfigPanel.tsx` 删除，无残留引用。
4. `bun run typecheck` = 0；后端无改动。
