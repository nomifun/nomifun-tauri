# 固定式计划栏（Pinned Plan Bar）设计

- 日期：2026-06-28
- 状态：已批准，待实现
- 范围：会话页 UI（`ui/src/renderer/pages/conversation`）

## 背景与问题

会话过程中 Agent 产出的「计划 / 待办清单」（`type: 'plan'` 消息，组件
`Messages/components/MessagePlan.tsx`）目前作为普通消息内联渲染在滚动消息流里。
随着会话继续，它会被新消息推到上方、滚出可视区进入历史记录，用户无法随时看到
当前计划进度，体验不佳。

现状关键事实：

- 计划数据形态：`IMessagePlan.content.entries: Array<{ content: string;
  status: 'pending' | 'in_progress' | 'completed'; priority? }>`（见
  `common/types/platform/acpTypes.ts` 的 `PlanUpdate`）。
- 计划消息由 `transformMessage`（`common/chat/chatLib.ts`）构造，**所有平台**
  （acp / nomi / nanobot / openclaw / remote）都可能产生。
- 计划更新逻辑（`Messages/hooks.ts` 的 `composeMessageWithIndex`）：同 `msg_id`
  的新计划会替换旧内容并被移动到列表末尾，因此「列表中最后一条 plan」即为当前计划。
- 布局：每个平台 chat wrapper 结构统一为
  ```
  <div className='flex-1 flex flex-col px-20px min-h-0'>
    <FlexFullContainer><MessageList/></FlexFullContainer>   // 填满剩余高度
    {!hideSendBox && <XSendBox/>}                            // 贴底发送框
  </div>
  ```
  `FlexFullContainer` 将子节点以 `absolute size-full` 填充。

## 目标

把当前计划固定为一个**贴在输入框上方**的常驻栏：

- 默认折叠，仅显示一行进度摘要；点击展开完整清单。
- 滚动消息流不再重复显示该计划（去重）。
- 适用于所有会话类型。

非目标（YAGNI）：

- 不做计划的编辑 / 手动勾选。
- 不做多计划并存的切换器（一个会话以「最新计划」为准）。
- 不做「智能自动展开 / 完成后自动折叠」（用户选择默认折叠 + 手动切换）。
- 不改动后端 / `hooks.ts` 的计划合并与排序逻辑。

## 用户已确认的决策

1. **位置**：紧贴输入框上方（消息区与发送框之间）。
2. **折叠默认态**：默认折叠，显示进度摘要，点击展开。
3. **消息流去重**：计划只在固定栏显示，从滚动消息流中移除。

## 方案

采用「独立 `PinnedPlan` 组件 + 各平台 wrapper 插入 + `MessageList` 过滤」的方案
（隔离性好、对滚动组件零侵入；代价是 5 处一行相同插入）。

### 1. 新组件 `Messages/components/PinnedPlan.tsx`

职责：渲染当前计划的固定栏。自包含，无 plan 时不渲染。

- 数据来源：`useMessageList()`（组件位于 `MessageListProvider` 内，各平台 chat
  wrapper 均被 `HOC.Wrapper(MessageListProvider, ...)` 包裹，满足条件）。
- **纯逻辑抽离**：把「从消息列表派生固定栏数据」抽成纯函数 `derivePinnedPlan(list)`，
  返回 `{ entries, done, total } | null`，与组件解耦以便单测（见「测试」）。建议放在
  同目录 `pinnedPlanModel.ts`。
  - 选取当前计划：从列表末尾向前找第一条 `type === 'plan'` 的消息，断言为
    `IMessagePlan`。
  - 隐藏条件：无 plan，或 `entries.length === 0` → 返回 `null`。
  - 进度计算：`done = entries.filter(e => e.status === 'completed').length`，
    `total = entries.length`。
- 组件消费 `derivePinnedPlan(useMessageList())`，为 `null` 时返回 `null`（隐藏）。
- 本地状态：`expanded`，**初始 `false`（折叠）**。
- 折叠态（一行，整行可点击切换）：
  - 「待办列表」徽标（复用 `messages.planTodoList`）
  - 进度文本（`messages.planProgress`，含 `done` / `total`）
  - 一条细进度条（`done/total` 宽度）
  - 展开 / 折叠箭头（`IconRight` / `IconDown`）
- 展开态：在摘要行下方渲染完整 `entries` 列表，条目图标按状态区分：
  - `completed`：实心对勾（沿用现 `MessagePlan` 的 `IconCheckCircle` 绿色）
  - `in_progress`：进行中样式（高亮 / 半填充圈，与 pending 区分）
  - `pending`：空心圈
  - 列表容器 `max-h-[30vh] overflow-y-auto`，避免长清单把输入框顶出屏幕。
- 样式：顶部分隔线 + 轻背景（`--color-fill-1` 等主题变量，过 `check:theme`），
  宽度与消息列 / 发送框一致（`md:max-w-780px mx-auto`），`shrink-0`。

### 2. `MessageList.tsx` 去重

在 `processedList` 构建循环顶部（与 `available_commands` 跳过同处）加入：

```ts
if (message.type === 'plan') continue;
```

计划不再进入渲染流。原始 `list`（含 plan）保持不变，`PinnedPlan` 仍可读取。
`useAutoScroll`（keyed by `messages: list`、`itemCount: processedList.length`）
不再因计划移动到末尾而抖动。

### 3. 清理内联计划组件

内联渲染移除后 `MessagePlan.tsx` 不再被使用：

- 将其条目渲染逻辑并入 `PinnedPlan`（展开态）。
- 删除 `Messages/components/MessagePlan.tsx`。
- 移除 `MessageList.tsx` 中 `MessagePlan` 的 import 与 `MessageItem` 的
  `case 'plan'` 分支（由 step 2 的过滤兜底，计划永不到达 `renderItem`，无死代码）。

`hooks.ts` 中计划合并 / 移到末尾的逻辑**保持不变**（仅变为不可见，固定栏读取
其最新内容）。

### 4. 插入点

在以下 5 个 wrapper 中，于 `</FlexFullContainer>` 之后、发送框之前插入
`<PinnedPlan />`：

- `platforms/acp/AcpChat.tsx`
- `platforms/nomi/NomiChat.tsx`
- `platforms/nanobot/NanobotChat.tsx`
- `platforms/openclaw/OpenClawChat.tsx`
- `platforms/remote/RemoteChat.tsx`

`PinnedPlan` 无条件渲染（不受 `hideSendBox` 影响）；无 plan 时自身返回 `null`，
不占布局。

### 5. i18n

- 复用：`messages.planTodoList`。
- 新增：`messages.planProgress`
  - en-US：`"{{done}}/{{total}} done"`
  - zh-CN：`"已完成 {{done}}/{{total}}"`
- 同步 en-US 与 zh-CN 两套 `messages.json`，运行 `bun run gen:i18n` 更新类型，
  过 `check:i18n`。

## 数据流

```
backend update ──► transformMessage ──► addOrUpdateMessage
        │                                      │
        │                                      ▼
        │                            useMessageList() 列表（含最新 plan，
        │                            同 msg_id 计划被移到末尾）
        │                                      │
        ├──────────────► MessageList: processedList 过滤掉 plan（不内联）
        │                                      │
        └──────────────► PinnedPlan: 取末尾最新 plan → 固定栏渲染
```

## 边界情形

- 无 plan / `entries` 为空：固定栏隐藏（`null`）。
- 窗口化历史（nomi，初始仅加载最新窗口）：若计划早于已加载窗口则暂不显示；
  活跃计划必在近窗口内，可接受。
- `hideSendBox` 锁定 / 嵌入面板：固定栏仍显示（只读信息）。
- 长清单：展开态 `max-h-[30vh]` 内部滚动，不挤压输入框。

## 测试

项目 UI 测试约定为 **`bun:test` + 纯逻辑测试**（无 testing-library / jsdom，
现有 `.test.tsx` 也仅测纯函数、不挂载 DOM）。因此对 `derivePinnedPlan` 纯函数
做单测（`pinnedPlanModel.test.ts`，`import { describe, expect, test } from 'bun:test'`）：

- 列表无 plan → 返回 `null`。
- `entries` 为空 → 返回 `null`。
- 单条 plan → `done` / `total` 计数正确（含 `in_progress` 不计入 done）。
- 多条 plan → 取最后一条（最新）。

折叠 / 展开等交互行为无 DOM 测试框架支撑，**通过手动验证**（运行应用观察固定栏
默认折叠、点击展开 / 折叠、计划更新时进度刷新、无 plan 时隐藏）。

回归校验：`bun run check`（typecheck + i18n + theme）通过；`bun test` 现有用例
不回归。

## 改动清单

- 新增：`Messages/components/PinnedPlan.tsx`
- 新增：`Messages/components/pinnedPlanModel.ts`（纯函数 `derivePinnedPlan`）
- 新增：`Messages/components/pinnedPlanModel.test.ts`（`bun:test`）
- 修改：`Messages/MessageList.tsx`（过滤 plan、移除 MessagePlan import/case）
- 修改：5 个平台 wrapper（各插入一行 `<PinnedPlan />`）
- 修改：`locales/en-US/messages.json`、`locales/zh-CN/messages.json`
- 删除：`Messages/components/MessagePlan.tsx`
- 生成：i18n 类型（`gen:i18n`）

规模较小，在主流程直接实现，无需 Workflow 编排。
