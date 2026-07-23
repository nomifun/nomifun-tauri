# 定时任务 (Cron)

NomiFun 中的一个定时任务是一个在你选择的时间触发的循环 (或一次性)
任务，它会驱动一个 AI agent 去做某件事。你可以从定时任务页面
配置它、按需运行它、给它附加一个个性化的**技能**让 agent 在
该任务下始终以正确的方式行事，并且你可以使用一个内置的 cron
技能在聊天中让任何 agent 帮你管理任务。

> 找的是应该尽快运行的一次性异步工作，而不是按时钟来的？参见
> [AutoWork & Requirements](./autowork-requirements.md)。需要一个
> 实时 shell？参见 [应用内终端](./terminal.zh.md)。

![定时任务列表](../images/cron-01-list.png)

## 一个任务做什么

`nomifun-cron` 是一个后端调度器 + 执行器：

- **调度器**用一个 5 字段 (Unix) 或 6 字段 (秒前缀) 的 cron 表达式
  为每个已启用的任务计算下一次触发时间 —— 两者都接受；
  5 字段表达式会通过在前面加 `0` 作为秒被规范化为 6 字段。
  调度也可以是一个绝对时间戳 (`At { at_ms }`) 或一个固定间隔
  (`Every { every_ms }`)。
- 每个任务的时区 (例如 `Asia/Shanghai`、`America/Los_Angeles`)
  会被尊重，所以 `0 9 * * MON` 表示**那个**时区的 09:00，而不是 UTC。
- **执行器**在定时器触发时驱动该任务的 agent。两种执行模式：
  - **`new_conversation`** —— 每次触发开启一个新会话。该任务
    携带 workspace、agent、model 和 prompt；执行器会创建会话、
    广播一个 `cron_trigger` 工件 (这样聊天 UI 会显示
    "本会话由一个定时任务发起")，然后发送 prompt。
  - **`existing`** —— 复用拥有该任务的会话。每次触发把 prompt
    作为一条新消息发送到同一个线程中。适合 "提醒我"、
    "总结今天" 或任何延续性重要的任务。
- 一个**忙碌守卫**防止同一个会话被并发进入。如果上一次运行
  在下一次触发到来时还在进行中，新的运行会被跳过 (记录为 `skipped`)。
- 一个**漏触发处理器**会在启动时和操作系统从睡眠中醒来后运行
  (`/api/cron/internal/system-resume`)。它会遍历每个 `next_run`
  在过去的已启用任务并发出一条系统消息，让你能看到漏掉一次
  触发 (例如笔记本休眠时)，然后为下一个 cron 节拍重新装定定时器。
- 每次触发会以一个状态被记录 —— `ok` / `error` / `skipped` /
  `missed` —— 并且 (在适用时) 附带一个指向所产生会话的链接，
  这样详情页可以向你展示运行历史。

## 创建一个任务

从侧边栏打开 **定时任务** (路由：`/scheduled`) 并按
**New task**。对话框涵盖四个区域。

### 频率

从一组小的预设中选择 —— `Manual` (无自动调度，仅通过 Run now 触发)、
`Hourly`、`Daily`、`Weekdays` (`MON-FRI`)、`Weekly` 或
`Custom`。预设会在 builder 中渲染出可编辑的 cron 表达式；选
**Custom** 直接键入。Builder 在你键入时进行校验。

Cron 语法速查 (5 字段 —— 秒字段会自动添加)：

```
*  *  *  *  *
│  │  │  │  └─ 星期几      (0–6 或 SUN–SAT，MON-FRI 可用)
│  │  │  └──── 月          (1–12 或 JAN–DEC)
│  │  └─────── 月中第几天   (1–31)
│  └────────── 小时         (0–23)
└───────────── 分钟         (0–59)
```

任务的时区在创建时设置 (默认是你浏览器的 IANA 时区) 并存储在
该行中；如果一个任务存储的时区因故无效，详情页会提供一键
修复到你的本地时区。

### Agent

选择每次触发运行的 agent。选择器中会显示三种类型：

- **CLI agent** —— `claude` / `codex` / `gemini` (后端在 `PATH`
  上检测到的任何一个)。该任务记录后端标签并端到端使用 ACP。
- **Nomi (内置)** —— 使用 Nomi 自有引擎以及你选择的
  provider/model。
- **设定** —— 可复用的启动配置；任务记录设定 id、revision 与解析快照。

**Advanced** 部分让你覆盖 workspace (agent 的工作目录)、model
以及任意的 `config_options` 键值对，它们会被转发给 agent 工厂。
Workspace 路径不能包含空白片段 —— 这一点在服务端被强制；
表单会把错误显式呈现出来。

### 执行模式

选择 `new_conversation` 或 `existing` (在 UI 中当你同时选择具体
是哪一个时被称为 "specified conversation")。详情页之后会向你
展示得到的会话。

### Prompt + 名称

**Prompt** 是每次发送给 agent 的内容。请把它写成一个**自包含的
指令** —— agent 看不到你原本 "我想要这个" 的框架，只看到这个
prompt。诸如下面这些模式：

- `Reply with a short weekly meeting reminder that includes the current date and time.`
- `Search for the latest AI news from this week and produce a concise bullet-point summary report.`
- `Run the weekly database health check and post the results back here.`

…比重述用户愿望要好。**Name** 只是一个标签。

![创建定时任务对话框](../images/cron-02-create-dialog.png)

## 运行、暂停、删除

列表视图 (`/scheduled`) 显示每一个任务、它的下一次触发，以及
一个启用开关。在详情页 (`/scheduled/:job_id`) 你可以：

- **Run now** —— 立即触发该任务，无视调度。忙碌守卫仍然适用。
- **Pause / Resume** —— 停止后续触发但不删除该行。
- **Edit** —— 与创建相同的对话框，处于编辑模式。
- **Delete** —— 删除该任务及其按任务生成的技能目录。此前运行创建的
  会话会保留在会话列表中，可按需单独删除。

详情页还会列出本任务创建的会话，按活跃度排序 —— 当任务
以 `new_conversation` 模式运行并为每次触发各分出一个线程时
非常有用。

![定时任务详情](../images/cron-03-detail.png)

## 保持唤醒

只有当宿主进程在运行时，cron 任务才会触发。列表页有一个
**NomiFun 运行时保持系统唤醒**开关，它会请求 OS 抑制睡眠
(Windows：`SetThreadExecutionState`，macOS：`caffeinate`，
Linux 上若可用为 `systemd-inhibit`)，这样你在笔记本上设置的
任务不会在合上盖子那一刻悄悄漏触发。

如果一次触发还是因为系统进入睡眠 (或 NomiFun 没在运行) 而漏掉，
下一次启动/唤醒时的漏触发处理器会记录一次 `missed` 运行并
向受影响的会话中投送一条系统消息，然后为下一次正常触发
重新装定定时器。

## 附加到任务的技能

一个**技能**是一个 `SKILL.md` 文件，agent 会在加入会话时读取它
—— 与 NomiFun 其他地方使用的相同机制，但作用域是按任务的。
你可以在详情页编写/编辑该技能；在幕后该文件会被写入数据
目录下的 `cron/skills/<cron_job_id>/SKILL.md`，执行器会在每次
触发时把它注入到 agent 的会话中。

用例：

- 任务输出的一致**人格** (风格、语气、格式)。
- **工具/MCP** 偏好 (启用哪些服务器、忽略哪些)。
- 工作区特定的约定 (commit 信息风格、目录布局、部署细节)。

任务有自己的技能目录（直接以无前缀的标准 UUIDv7 `cron_job_id`
命名），所以共享同一 workspace 的两个任务可以承载不同的行为而不冲突。删除任务会
移除其技能目录。

还有一个自动的**技能建议**检测器，它会在运行期间观察 agent 的
输出；当它产出一个干净的候选技能时 (符合预期格式且不只是占位
模板)，检测器会在会话中创建一个 `skill_suggest` 工件，让你
可以审查并一键将其保存为该任务的技能。

## 在聊天中管理任务 —— 内置的 `cron` 技能

NomiFun 附带一个名为 `cron` 的内置自动注入技能，任何 agent 都可以
在你让它"设置一个提醒"、"每周一安排 X" 等时加载它。
然后会话中间件会观察 agent 的回复中以下的指令块，并通过 cron
服务运行它们：

| 指令                  | 含义                                                    |
| --------------------- | ------------------------------------------------------- |
| `[CRON_LIST]`         | 列出当前会话作用域内的 cron 任务。                      |
| `[CRON_CREATE]…[/CRON_CREATE]` | 创建一个任务 (字段：`name`、`schedule`、`schedule_description`、`message`)。 |
| `[CRON_UPDATE: <id>]…[/CRON_UPDATE]` | 原地更新一个已有任务。                          |
| `[CRON_DELETE: <id>]` | 按 id 删除一个任务。                                    |

中间件会从用户看到的内容中**剥离**这些块，并把系统响应
(`Created cron job 'X'`、`No scheduled tasks` 等) 投回到会话中。
所以在聊天里看起来像是正常的来回；幕后是 agent 发出了一个
指令，平台执行了它。

该技能在设计上被限制为**每个会话一个任务** —— 这让循环保持
简单 ("查询，然后行动") 并避免了你重新询问时重复任务堆积。
要一次管理多个任务，请直接使用定时任务页面。

## 路由 & API

| 内容                            | 位置                                                              |
| ------------------------------- | ----------------------------------------------------------------- |
| 列表页面                        | `/scheduled`                                                      |
| 详情页面                        | `/scheduled/:job_id`                                              |
| 列出 / 创建任务                 | `GET /api/cron/jobs`，`POST /api/cron/jobs`                       |
| 获取 / 更新 / 删除              | `GET|PUT|DELETE /api/cron/jobs/:id`                               |
| 立即运行                        | `POST /api/cron/jobs/:id/run`                                     |
| 列出某任务的会话                | `GET /api/cron/jobs/:id/conversations`                            |
| 每任务技能                      | `GET|POST|DELETE /api/cron/jobs/:id/skill`                        |
| 系统恢复 (内部)                 | `POST /api/cron/internal/system-resume` (需要内部 header)         |

UI 订阅的实时事件：`cron.job-created`、`cron.job-updated`、
`cron.job-removed` 和 `cron.job-executed`。漏触发会作为一次
`cron.job-executed` 事件上报，payload 中的状态是 `missed`。

## 故障排查

- **任务没有按时触发。** 那时宿主在运行且处于唤醒状态吗？
  如果你合上了笔记本或关闭了应用，请查看唤醒后的下一条会话
  条目 —— 漏触发处理器会投送一条 `missed` 通知并重新装定
  定时器。
- **我的 cron 表达式被拒绝了。** 5 字段 (`m h dom mon dow`)
  和 6 字段 (`s m h dom mon dow`) 形式都是合法的。在本地用
  [crontab.guru](https://crontab.guru/) 或对话框中的 builder
  校验。
- **任务运行了但 agent 做错了事。** 重新阅读 prompt，假设你没有
  其他上下文。它必须告诉 agent 准确要产出什么。然后考虑附加
  一个技能以锁定行为。
- **两次定时触发碰撞了。** 忙碌守卫会在 `existing` 模式下跳过
  重叠的运行 (该运行被记录为 `skipped`)。如果你预期触发会
  长时间运行，请把任务切换到 `new_conversation` 模式，让每次
  触发各得一个线程。
- **聊天中的 `cron` 指令什么也没做。** 如果 cron 服务没有接入
  (例如某些测试 harness)，中间件会 no-op；在正常 app build 中
  它总是接入的。如果指令格式错误 (缺失闭合标签、缺失
  `schedule`)，它会被静默丢弃 —— 用更干净的输入重新提示
  agent。
