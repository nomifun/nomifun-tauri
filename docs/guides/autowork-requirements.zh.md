# AutoWork 与 Requirements

AutoWork 是 NomiFun 的旗舰自动化能力：一块 **需求看板**（requirements board）加上每个目标各自的 **执行循环**，由它驱动 AI 智能体（或运行在终端中的 agent CLI）逐条处理这些需求，无需你全程盯着。

你登记需求，按 tag 分组，把 tag 绑定到一个会话（对话或终端），AutoWork 循环就会按顺序认领、执行并完结它们。当某条需求进入终态时，可以触发 **完成通知**（Lark/飞书 webhook），让你的团队第一时间知道结果。

这里描述的所有内容都是 **后端权威** 的：AutoWork 在进程启动时自动恢复，无论你是否打开 UI 都会运行。

![AutoWork tag-sessions 总览](../images/autowork-01-tag-sessions.png)

## 概念

| 术语                  | 含义                                                                                                                                                |
| --------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------- |
| **Requirement**       | 一个工作单元：标题、内容（实际指令）、tag、`order_key`（按字典序比较的字符串）以及状态。存储在 SQLite 中。                                                |
| **Tag**               | 任意字符串，用来把需求归入一个队列。绑定关系、看板列以及 webhook 路由都以 tag 为键。                                                                          |
| **Status**            | `pending` → `in_progress` → `done`（或 `failed` / `cancelled`）。看板视图每个状态对应一列。                                                            |
| **Claim & lease**     | AutoWork 循环原子地把某 tag 中 `order_key` 最小的 `pending` 需求转为 `in_progress`，并写入一份带过期时间的租约（lease）。                                          |
| **Lease sweeper**     | 一个后台任务（每 60 秒一次），会把租约已过期、且持有它的会话已不在的 `in_progress` 行重置回 `pending`——这样崩溃永远不会让任务孤立。                            |
| **AutoWork 循环**    | 每个目标对应一个循环：认领 → 注入 → 等待 → 完结 → 重复。每个绑定的会话有一个循环。它是常驻的：队列空了就空闲等待，不会退出。                                  |
| **Target**            | 实际执行工作的对象。两种：**会话**（一个 AI 智能体），或者 **终端**（通过 PTY 运行的真实 CLI 智能体）。                                                       |
| **回合完成** | 一轮如何宣告"完成"。对智能体目标来说，是该智能体结束本轮回复（或调用 Nomi 专属工具）；对终端目标来说，是终端输出静默下来（干净收尾）。                                       |
| **Completion notifier** | 当需求进入 `done`/`failed`/`cancelled` 时触发的 Lark/飞书 webhook。按 tag 绑定。                                                                  |
| **IDMM**              | 智能决策模式（Intelligent Decision-Making Mode）——一个会话级监督器，能在 provider 故障和决策卡顿时让目标继续存活。可与 AutoWork 叠加使用。                       |

## 单条需求的生命周期

```
pending  ──claim_next()──▶  in_progress (lease)  ──injection──▶  agent / CLI runs
                                  │                                   │
                                  ▼                                   ▼
                       sweeper re-pends if lease         Finish event / quiescence
                       expires & loop is gone                        │
                                                                     ▼
                                                            done | failed | cancelled
                                                                     │
                                                                     ▼
                                                       CompletionNotifier fires (best-effort)
```

当 tag 为空时 AutoWork 循环 **不会** 退出。它会等待唤醒通知（外加一个 10 秒兜底轮询），并永久持续认领，因此向已绑定的 tag 新提交的需求几乎是即时被拾取。

它仅在以下情况退出：

- 你对该目标关闭了 AutoWork；
- 绑定触达了 `max_requirements` 上限（此时配置会被持久化为已禁用，使该上限在重启后依然生效）；或
- 某个终端目标对应的行被删除（仅仅是 PTY 退出的终端会进入空闲并等待重新启动——它并不会停止循环）。

## 三种视图

AutoWork 在每个视图中的数据完全相同，视图只是不同的"镜头"。

### 需求列表 — `/requirements`

扁平表格。可按 tag、状态或全文搜索过滤。可批量删除选中行。点击行可以打开详情抽屉；**编辑** 路径是 `/requirements/:id/edit`，**新建需求** 通过 `/requirements?new=1` 打开，旧的 `/requirements/new` 会重定向到这里。

![需求列表](../images/autowork-02-list.png)

### 看板 — `/requirements?view=board`

针对所选 tag，每个状态一列。这里有意 **不** 通过拖拽来改状态；请使用详情抽屉。看板会在每次 `requirements.*` 实时事件触发时重取数据，因此能跟随 AutoWork 循环实时变化。

![需求看板](../images/autowork-03-kanban.png)

### Tag sessions — `需求平台 → 扩展能力 → 自动执行`

AutoWork 的管理面板（`/requirements/extensions?tab=autowork`）。列出所有 tag、所有绑定（哪些会话和终端绑定到哪个 tag）、每条绑定的实时运行状态（`Idle`，或正在执行某轮时为 `Active`）。每个 tag 的完成 webhook 现在在旁边的 **通知** tab（`/requirements/extensions?tab=notify`）。

这里是你"巡视舰队"的地方。要在某条绑定上 **启动** AutoWork，请打开会话本身并在那里切换 AutoWork 开关——那才是绑定 tag、设置 `max_requirements` 和持久化配置的标准位置。

![Tag sessions 管理面板](../images/autowork-01-tag-sessions.png)

## 提交一条需求

在列表页点击 **新建需求**（或访问 `/requirements?new=1`）。表单包含：

- **标题**：简短的标签。
- **Tag**：选择已有 tag 或键入一个新值。tag 在首次使用时会被创建。
- **内容**：交给智能体 / CLI 的实际指令。当作 ticket 来写：上下文足够让智能体不必反问就能开始，并附上清晰的"完成定义"。
- **Order key**：用于队列排序的字符串。按字典序排列，因此常见模式如 `1.0`、`1.1`、`1.2.0` 等等。值越小越早。
- **状态**：默认是 `pending`。你也可以在这里手动把某行标记为 `done` 或 `cancelled`。

提交后该行进入队列。如果已有会话绑定到该 tag，它会立刻被唤醒并开始处理这条需求（前提是没有别的需求排在它前面）。

## 绑定会话：智能体 vs 终端

一条绑定形如 `(target_kind, target_id, tag, max_requirements?)`。target kind 只有两种。

### 智能体目标（一个会话）

打开任意会话。头部有一个 **AutoWork** 控件。选择 tag，可选地设置完成上限，然后启用。

每一轮中发生的事：

1. AutoWork 循环认领该 tag 中下一条 `pending` 需求。
2. 它构造一段注入 prompt，点名该需求并告知智能体如何上报完成状态。具体协议是 **engine-aware** 的：
   - 仅在 Nomi-engine 会话上，智能体会注册 `requirement_complete` / `requirement_update_status` 工具，并由 prompt 要求模型调用它们。
   - 在所有其他 engine（ACP / Codex / Gemini / Openclaw / Nanobot / Remote）上，智能体不会注册任何 requirement 工具，因此 prompt 使用 **无工具协议**：把工作做完，以一段纯文本完成说明结束本轮，平台会在本轮干净结束后自动记为 `done`。失败通过纯文本上报（prompt 要求模型把最后一行以 `Requirement failed:` 起头，紧跟原因）。
3. 注入消息会从用户可见的对话记录中隐藏。
4. AutoWork 循环订阅该智能体的流，等待 `Finish`（干净）或 `Error`/超时（重置回 pending 或 fail）。同时它会把智能体的文本输出捕获到一份 tail-bounded 的 **completion note**，存到该需求上；在无工具协议的 engine 上，这份 note 就是发到下游的报告。
5. 当本轮干净结束时，`finalize_if_needed` 把该行记为 `done` 并触发通知器。

### 终端目标（运行在 PTY 中的 agent CLI）

打开预设为 `claude` 或 `codex` 的终端（普通 shell 不符合条件）。
Gemini 终端可以手动运行，但后端目前不会接受它作为终端 AutoWork 目标：
它的回合生命周期和完成契约还没有接入 AutoWork 循环。符合条件的终端头部会显示同一个
**AutoWork** 控件；绑定一个 tag 并启用即可。

每一轮中发生的事：

1. AutoWork 循环在注入 **之前** 订阅终端的实时输出流（这样不会漏任何字节）。
2. 它向 PTY 写入需求 prompt，外面包了一对 bracketed-paste 标记（`ESC [200~ … ESC [201~`），后跟 `CR`，使多行文本作为单次粘贴落入 CLI 的编辑器，并由 Enter 实际提交。
3. prompt 只要求 agent 把活干完、**结束本轮回复**——不需要打印任何标记。从交互式 TUI 里抓协议字符串被证明不可靠(光标重绘输出、没有干净的换行、模型抄错 code),所以完成判定改为基于回合本身。
4. 当输出 **静默**(在最少 3 秒后≥10 秒无输出,且 PTY 还活着),说明 agent 已干完并回到空闲——本轮记为 `done`,与无工具的对话 agent 用的是同一套「干净收尾即完成」契约。
5. 如果 agent 无法完成,会被要求用纯文本明确说明(例如最后一行以 `Requirement failed:` 开头);这类回合在平台层面仍记为 `done`,拿不准时请回看对话。
6. 中途 PTY 死亡 → 重置回 pending。整轮硬超时是 1 小时。

> **强烈推荐 Full Auto。** 一旦本轮撞到交互式批准提示，会一直阻塞到超时。每个 agent CLI 都有一个非交互式开关，终端的 "Full Auto" 模式会替你加上（参见 [Terminals → Creating a terminal](./terminal.zh.md#creating-a-terminal)）。

被绑定但 PTY 已退出的终端会让循环以空闲方式存活：当你重新启动该终端时，AutoWork 会从中断处继续——无需先关闭再开启绑定。

## 启动恢复——它在你不在场时也会运行

AutoWork 循环的活跃集合存放在内存中，但每条绑定的 `enabled`、`tag`、`max_requirements` 都已持久化（在会话的 `extra.autowork` 或终端的 `autowork` 列中）。进程启动时后端会列出每个用户、遍历每条 tag 绑定，并 **自行启动** 这些循环。要让 AutoWork 工作你不必去打开会话页面；UI 只是把已经在跑的状态展示给你看。

这就是为什么"AutoWork 只在我开着标签页时才工作"是一个 bug 而不是 feature。如果你观察到这种现象，去检查 AutoWork 循环日志中是否有该用户/目标的 resume 失败记录。

## 完成通知（Lark / 飞书）

当需求进入终态时，会调用 `CompletionNotifier`。今天它做的事：

1. 查找该需求 tag 的 **per-tag 设置**——如果该 tag 没有设置或没有绑定 webhook，通知器静默 no-op。
2. 按 id 查找绑定的 webhook；如果它处于禁用状态，no-op。
3. 构造一张 Lark 互动卡片，字段如下：
   `需求id` · `需求名` · `需求内容`（截断到 500 字符） ·
   `完成状态`（`done`/`failed`/`cancelled`） ·
   `完成记录(报告)`（本轮中捕获的 completion note，截断到 500 字符）。
4. POST 到 webhook URL。如果该 webhook 配置了 secret，请求会按 Lark 自定义机器人的标准方案签名（`HMAC-SHA256(key="{ts}\n{secret}", msg="")`，base64）。
5. 失败会以 `warn` 记录并吞掉——一个不稳定的 webhook 永远不会影响需求状态。

### 配置步骤

1. 进入 **需求平台 → 扩展能力 → 通知**（`/requirements/extensions?tab=notify`）并 **Create webhook**：填写名称、Lark 自定义机器人 URL，以及（可选的）匹配 secret。点 **Test** 发一张卡片，验证机器人可达。
2. 在同一个 **通知** tab 里找到该 tag，从 per-tag 下拉框中挑选 webhook。设置按 tag 保存。

你可以随时改变某个 tag 指向哪个 webhook，包括清空绑定以静音该 tag 的通知。

![Per-tag webhook 路由](../images/autowork-05-webhook-binding.png)

## IDMM——让卡顿中的本轮继续存活

IDMM 是一个独立、可选的监督器（`nomifun-idmm`）。它监视会话，并在检测到卡顿时介入：

- **规则层（无 LLM）**：provider 报错、反复重试、模型在工具调用上转圈等等——以确定性策略处理。
- **Sidecar 层**：调用一个轻量备用模型来下达下一步决策，避免会话挂死。

当 AutoWork 启动一轮时，它会请求 IDMM（如果已对接）在本轮持续期间 **保证监督** 该目标。两个特性可以组合：AutoWork 推动前进，IDMM 让每一轮不至于卡死，从而真正进入终态而不是超时。IDMM 与 AutoWork 在同一处切换（会话头部）。

每层策略的细节和介入日志 API 见 `crates/backend/nomifun-idmm/`。

> 想看完整全貌 —— 规则层、旁路模型、会话保活，以及何时开启 —— 参见专门的
> [智能决策（IDMM）](intelligent-decision.zh.md)指南。

## 路由与 API

| 用途                              | 位置                                                              |
| --------------------------------- | ----------------------------------------------------------------- |
| 需求列表                          | `/requirements`                                                  |
| 看板（按 tag）                    | `/requirements?view=board`                                      |
| Tag sessions 管理                 | `/requirements/extensions?tab=autowork`                         |
| 通知配置                          | `/requirements/extensions?tab=notify`                           |
| 新建 / 编辑                       | `/requirements?new=1`、`/requirements/:id/edit`                 |
| 旧版 `/autowork`、`/requirements/tag-sessions` | 重定向到 `/requirements/extensions?tab=autowork`    |
| 旧版 `/requirements/new`、`/requirements/kanban` | 重定向到当前 query-param 路由                       |
| 列出 / 创建需求                   | `GET /api/requirements`、`POST /api/requirements`                |
| Tags                              | `GET /api/requirements/tags`                                     |
| Tag 绑定（管理）                  | `GET /api/requirements/tag-bindings`                             |
| Per-tag 看板                      | `GET /api/requirements/board?tag=…`                              |
| 获取 / 更新 / 删除                | `GET|PUT|DELETE /api/requirements/:id`                           |
| 状态 / 完成 / 认领                | `POST /api/requirements/:id/status`、`…/complete`、`…/claim`     |
| AutoWork 开关 / 状态              | `POST /api/requirements/autowork`、`GET …/autowork/:kind/:tid`   |
| Webhooks                          | `GET|POST /api/webhooks`、`…/{id}`、`…/{id}/test`                 |
| Per-tag webhook                   | `GET|PUT /api/tags/:tag/settings`                                |

## 实现注记（写给好奇的你）

- `requirements.conversation_id` 有意 **不带外键** 指向 conversations 表。一条需求一旦创建，会随着重置回 pending 在多个会话之间轮转；用引用完整性把它绑死到单个 conversation 会让清理逻辑变得别扭，而且并不会带来真正的安全保障。请把该列视为参考性字段。
- AutoWork 循环的 `wake` Notify 与 `RequirementService` 共用；任何会重置回 pending 或创建工作的状态变更都会触发它，循环也会在每次 `claim_next()` 调用前后用 armed-then-await 的方式包起来，因此在"claim 返回 None"和"await"之间到达的唤醒永远不会丢。
- 终端注入仍用 bracketed-paste 标记把多行 prompt 作为单次粘贴落入，再在一拍之后单独写一个 `CR` 提交（与 paste 同批写入的 CR 会被现代 agent TUI 的 paste-burst 检测吞掉）。
- 无工具 engine 的 completion note 有上限（`MAX_NOTE_CHARS = 4000`）且 **偏向尾部**——智能体倾向于在末尾做总结，因此当需要截断时我们保留尾部。
