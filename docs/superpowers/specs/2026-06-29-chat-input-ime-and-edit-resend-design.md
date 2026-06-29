# 会话输入框交互优化：Enter 发送防误触 + 暂停后编辑重发

- 日期：2026-06-29
- 范围：UI（`ui/src`）+ Rust 后端（`crates/`，仅 Nomi 原生引擎）
- 背景：解决两个会话页面交互问题
  1. 输入框一感知到 Enter 就发送，输入法（IME）上屏候选词的 Enter 会被误当成发送，体验差。
  2. 已发出的消息在暂停后无法编辑重新提交。

---

## 问题一：Enter 发送

### 目标

1. **核心 bug**：彻底堵住"输入法上屏的 Enter 被误判为发送"。
2. **可配置发送键**：新增用户偏好，可选「Enter 发送 / Shift+Enter 换行」（默认）或「Ctrl/⌘+Enter 发送 / Enter 换行」。

### 根因

`ui/src/renderer/hooks/chat/useCompositionInput.ts` 仅用 `compositionstart/end` 维护一个 `isComposing` ref。存在经典竞态：部分输入法/浏览器在"上屏候选词"时 `compositionend` 会**先于** Enter 的 `keydown` 触发，此时 ref 已被置 `false`，于是该次 Enter 落入发送分支（`useCompositionInput.ts:26`）。缺少 `nativeEvent.isComposing` / `keyCode===229` / 时间窗兜底。该 hook 同时被会话框 `SendBox`（`index.tsx:984,1714`）与引导页 `GuidInputCard`（`GuidInputCard.tsx:83`）使用，集中修复即可一并受益。

### 设计

#### A. 健壮的 IME 守卫（`useCompositionInput.ts`）

在 hook 内新增多重判定，任一为真即视为"输入法占用中"，跳过发送：

- `isComposing.current === true`（现有，compositionstart→true / compositionend→false）。
- 新增 `justComposedRef`：`compositionend` 时置 `true`，并在下一帧 `requestAnimationFrame` 清回 `false`。用于覆盖"`compositionend` 同 tick 先于 Enter `keydown`"的浏览器（同一物理按键，间隔≈0ms；一帧后清除，保证之后用户**主动再按** Enter 仍能正常发送）。
- `e.nativeEvent?.isComposing === true`（W3C 原生属性）。
- `(e as any).keyCode === 229`（IME 处理中的 keydown）。

对外暴露 `isImeActive(e): boolean` 供其它自定义 keydown（`GuidPage.handleInputKeyDown`、`GuidInputCard`）复用，替换它们裸用 `isComposing.current` 的判断。

#### B. 发送键偏好

- **配置项**：在 `ui/src/common/config/configKeys.ts` 的 `ConfigKeyMap` 新增
  `'chat.sendKey': 'enter' | 'mod-enter' | undefined;`（缺省按 `'enter'` 处理）。
  读写经现有单例 `configService`（`GET/PUT /api/settings/client`，对新 key 透明），读取用 `useConfig('chat.sendKey')`。
- **生效点**：`createKeyDownHandler(onSubmit, intercept?, sendKey?)` 增加 `sendKey` 参数；IME 守卫与 `intercept` 之后判定提交手势：
  - `'enter'`：`Enter && !shift && !meta && !ctrl && !alt` → 提交；`Shift+Enter` → 换行（默认行为不变）。
  - `'mod-enter'`：`Enter && (meta||ctrl) && !shift` → 提交；裸 `Enter` → 换行（不拦截，交由 textarea 插入换行）。
  - 调用方（`SendBox`、`GuidInputCard`/`GuidPage`）从 `useConfig` 取值传入；hook 自身不依赖 config，便于测试与复用。
- **与既有 Mod+Enter「steer」共存**（`SendBox/index.tsx:1718-1731`）：
  - `'enter'` 模式：保持现状——Enter 提交，Mod+Enter 在 `steerAvailable` 且 turn 运行时执行 steer。
  - `'mod-enter'` 模式：Mod+Enter 即主提交手势；**键盘 steer 快捷键在此模式下不挂载**（steer 仍可经 steer 按钮触发）。即 SendBox 中处理 Mod+Enter→steer 的 intercept 分支仅在 `sendKey==='enter'` 时生效。
- **设置 UI**：在 `SettingsModal/contents/SystemModalContent/index.tsx` 的 `preferenceItems` 增一行，复用 `PreferenceRow` + `NomiSelect`（两项）。仿 `language`/`keepAwake` 的 `useState` + 启动 `configService.get(...) ?? 'enter'` + change 乐观写入/失败 `setLocal` 回滚样板。
- **i18n**：`settings.json`（en-US + zh-CN）新增 `sendKey` / `sendKeyDesc` / `sendKeyEnter` / `sendKeyModEnter`。

### 受影响文件（问题一）

- `ui/src/renderer/hooks/chat/useCompositionInput.ts`（IME 守卫 + `sendKey` 提交判定 + `isImeActive`）
- `ui/src/renderer/components/chat/SendBox/index.tsx`（传入 `sendKey`；steer intercept 仅 `'enter'` 模式）
- `ui/src/renderer/pages/guid/components/GuidInputCard.tsx` + `pages/guid/GuidPage.tsx`（复用 `isImeActive`，最终 Enter→send 分支按 `sendKey` 判定）
- `ui/src/common/config/configKeys.ts`（新 key）
- `ui/src/renderer/components/settings/SettingsModal/contents/SystemModalContent/index.tsx`（设置行）
- `ui/src/renderer/services/i18n/locales/{en-US,zh-CN}/settings.json`（文案）

---

## 问题二：暂停后编辑重发（仅 Nomi、仅最近一条、回填输入框）

### 锁定的范围决策

- 语义：**截断重跑**（编辑后删除该消息及其后全部消息并重新生成）。
- 可编辑对象：**仅最近一条用户消息**（最后一个用户 turn）。
- 平台：**仅 Nomi 原生引擎**。
- 编辑交互：**回填输入框**——点"编辑"把原文本（含附件）放回 `SendBox`，输入框进入"编辑模式"，提交即截断重跑。

### 关键架构约束（决定为何只做"最近一条"）

- Nomi 引擎的模型上下文是内存 `AgentEngine.messages: Vec<Message>`，**与 DB `messages` 表解耦**（DB 仅供 UI 展示/持久化）。引擎自持文件型 session 持久化，从不回读 DB 构建上下文。
- 引擎 transcript 里 tool 结果、steering 注入、目标续跑都以 `Role::User` 入栈（`engine.rs:1075,927,939`），且会被 microcompaction 整体重写（`engine.rs:1158`）。因此"DB 某条消息 ↔ transcript 某下标"无稳定映射；`thinking` 签名、tool_use 配对不持久化，中间点**无法忠实重建**。
- 暂停（mid-stream cancel）时引擎**保留**该 turn 起始 push 的用户消息（`engine.rs:670`），但**不 push** 助手回复（`engine.rs:848-859`）。
- 结论：只有"最后一个用户 turn"可被干净地从内存 transcript 弹出而保全之前的完整上下文。

### 数据流

```
用户在某条最近的用户消息（position='right', type='text'）上点「编辑」(仅 Nomi、仅最近一条、仅 idle)
  → MessageText 触发 emitter 事件 'sendbox.edit' { msgId, createdAt, content }
  → SendBox 进入"编辑模式"：回填文本(经 parseFileMarker 拆出纯文本与附件) + 顶部"编辑中"提示条(可取消)
  → 用户改完点提交
  → NomiSendBox.handleEditResubmit(msgId, input, files):
       ipcBridge.conversation.editResubmit.invoke({ conversation_id, msg_id, input, files })
  → 后端 service.edit_and_resubmit:
       1. 鉴权 + 校验 msg_id 属于该会话且为最近一条用户消息
       2. cancel 任何在飞 turn（防御）
       3. 引擎 rewind_last_turn()：把内存 transcript 截断到该 turn 起始锚点（保全之前上下文）
       4. repo.delete_messages_from(conv_id, created_at, id)：DB 删除该条(含)及其后所有行
       5. 复用 send_message 正常流程发送新内容 → 新 turn 流式回来
  → 前端：调用前先本地移除 ≥ 该 createdAt 的消息(snappy)，再 emit 'chat.history.refresh' 对齐 DB；流式渲染新回复
```

### 后端设计（Rust，仅 Nomi）

- **Repo**（`crates/backend/nomifun-db`）
  - `IConversationRepository::delete_messages_from(conversation_id, created_at, id) -> Result<u64>`（`repository/conversation.rs`）+ SQLite 实现（`repository/sqlite_conversation.rs`）：
    `DELETE FROM messages WHERE conversation_id=?1 AND (created_at>?2 OR (created_at=?2 AND id>=?3))`，命中 keyset 索引 `idx_messages_conv_created_id`。
- **Engine**（`crates/agent/nomi-agent/src/engine.rs`）
  - 新增字段 `last_turn_start_len: Option<usize>`，在 `run_inner` push 用户消息前（约 `:670`）记录 `self.messages.len()`；持久化进 session（restart 后 resume 可用）。
  - microcompaction 重写 transcript 时（`:1158`）置 `last_turn_start_len = None`（失效）。
  - `pub fn rewind_last_turn(&mut self) -> bool`：若锚点存在且 `start <= messages.len()` 且 `messages[start]` 为 `Role::User` 文本（sanity），则 `self.messages.truncate(start)` + 清锚点 + `save_session()`，返回 `true`；否则 `false`。
- **Manager**（`nomifun-ai-agent` 的 `NomiAgentManager`）
  - 暴露 `rewind_last_turn()` 透传到引擎（仿 `clear_context` 的"先 request_stop 再操作"模式）。
- **Service**（`crates/backend/nomifun-conversation/src/service.rs`）
  - `edit_and_resubmit(conversation_id, msg_id, input, files) -> Result<{ msg_id }>`：
    鉴权 → 校验该 `msg_id` 是该会话**最近一条** `position='right'` 文本消息（否则 4xx）→ `cancel` 在飞 turn → 取该消息 `(created_at,id)` → `agent.rewind_last_turn()`（失败则回退：返回可读错误，提示"上下文已压缩，无法精确回退"）→ `repo.delete_messages_from(...)` → 复用 `send_message` 发送新内容并返回新 `msg_id`。
  - 仅当会话 agent 类型为 Nomi 时可用，其它类型返回 4xx（UI 不会触发）。
- **Route**（`routes.rs`）：`POST /api/conversations/{id}/messages/{messageId}/edit-resubmit`。

### 前端设计

- **emitter**（`utils/emitter.ts`）：新增 `'sendbox.edit': [{ msgId: string; createdAt: number; content: string }]`。
- **MessageText.tsx**：用户消息（`isUserMessage && type==='text'`）的悬浮工具行（`:232-245`，桌面端）在 `copyButton` 旁加「编辑」图标按钮，复用其样式。显示条件：会话 `type==='nomi'` && 非运行中 && **该消息是最近一条用户消息**。点击 emit `'sendbox.edit'`。
  - 移动端：当前无 per-message 工具行。最近一条用户气泡长按 → 轻量动作菜单（复制/编辑）。此为次优先项，可在实现期决定是否随首版交付。
- **SendBox/index.tsx**（通用、平台无关）：
  - 新增可选 prop `onEditResubmit?: (msgId: string, message: string) => Promise<void>`。
  - 监听 `'sendbox.edit'`：设 `editingMessage={msgId}`，回填文本、还原附件（经平台 `setUploadFile`，仿 `handleEditQueuedCommand`）、显示"编辑中"提示条（复用 `replyQuote` 预览卡样式）+ 取消按钮（取消恢复原草稿）。
  - 编辑模式下提交：调用 `onEditResubmit(editingMessage, finalMessage)` 而非 `onSend`，完成后清除编辑态。发送按钮图标/提示切换为"保存并重发"。
  - 仅当宿主提供 `onEditResubmit` 时进入编辑模式（即仅 Nomi）。
- **NomiSendBox.tsx**：提供 `onEditResubmit` → `handleEditResubmit`：先本地移除 ≥ 该消息的行（新增 `useRemoveMessagesFrom(createdAt)` 助手，仿 `useRemoveMessageByMsgId`），调用 `ipcBridge.conversation.editResubmit`，再 emit `'chat.history.refresh'`；进入编辑态时还原附件。
- **ipcBridge.ts**：`conversation.editResubmit.invoke({ conversation_id, msg_id, input, files? }) -> { msg_id }`，映射上面的 HTTP 路由。
- **i18n**：复用 `common.edit`；新增 `conversation.editMessage.{banner,cancel,save}` 等（en-US + zh-CN）。

### 受影响文件（问题二）

后端：`repository/conversation.rs`、`repository/sqlite_conversation.rs`、`agent/nomi-agent/src/engine.rs`、`nomifun-ai-agent`（manager）、`nomifun-conversation/src/{service.rs,routes.rs}`。
前端：`utils/emitter.ts`、`Messages/components/MessageText.tsx`、`components/chat/SendBox/index.tsx`、`platforms/nomi/NomiSendBox.tsx`、`pages/conversation/Messages/hooks.ts`（`useRemoveMessagesFrom`）、`common/adapter/ipcBridge.ts`、`locales/{en-US,zh-CN}/conversation.json`。

---

## 错误处理与边界

- **编辑入口仅在 idle 显示**：turn 运行中不显示编辑按钮（feature 命名即"暂停后"）。
- **"最近一条"判定**：前端按消息列表里最后一个 `position==='right'` 文本消息判断；后端二次校验，防止竞态/伪造。
- **锚点失效（已压缩）**：`rewind_last_turn` 返回 `false` 时，service 返回可读错误，前端 toast 提示并保留输入内容，不破坏 DB。
- **附件还原**：文本必还原；附件路径尽力还原（来自 `parseFileMarker` 的展示路径，可能有损），实现期评估。
- **artifacts**：MVP 不删除截断点之后产生的 artifacts（属工作区文件，重跑可能覆盖）。后续可加 `delete_artifacts_from`。
- **多端一致性**：DB 删除 + 引擎 transcript + 文件 session 三处需在 service 内顺序保证；任一步失败需返回明确错误且不留中间态（DB 删除应在引擎 rewind 成功之后执行）。

## 测试

- **问题一**：`useCompositionInput` 单测——模拟 `compositionend` 先于 Enter `keydown`、`keyCode===229`、`nativeEvent.isComposing`、`'mod-enter'` 模式下裸 Enter 不发送/Mod+Enter 发送、`'enter'` 模式回归。
- **问题二**：
  - Rust：`delete_messages_from` 删除区间正确（含/不含边界）；`rewind_last_turn` 截断到锚点且 sanity 失败返回 false；`edit_and_resubmit` 非最近一条/非 Nomi 返回 4xx；端到端"暂停→编辑→重发"上下文不含旧消息但含更早历史。
  - 前端：SendBox 编辑模式进入/取消/提交走 `onEditResubmit`；MessageText 编辑按钮显示条件。

## 不在本次范围

- 编辑中间任意消息（受架构约束，需忠实重建，本质不可行）。
- 非 Nomi 平台的编辑重发。
- 助手消息的"重新生成"按钮。
- 截断点之后 artifacts 的清理。
