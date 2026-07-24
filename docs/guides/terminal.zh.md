# 应用内终端

Nomi 在应用内附带了一个真正的终端。每个终端都是一个由后端管理的
PTY 会话，你可以从浏览器/桌面窗口中以交互方式驱动它 —— 当你把它
绑定到一个 tag 上时，AutoWork 也可以代你来驱动它。

> 需要自动化指南？参见 [AutoWork & Requirements](./autowork-requirements.md)。
> 需要按计划运行 agent？参见 [定时任务](./scheduled-tasks.zh.md)。

![Nomi 应用内终端](../images/terminal-01-session.png)

## 应用内终端是什么

当你创建一个终端时，后端 (`nomifun-terminal`) 会通过
[`portable-pty`] 派生一个连接到真实伪终端的子进程。该会话由三部分组成：

- **持久化元数据** —— id、名称、工作目录、命令 + 参数、env、
  preset/backend、权限模式、当前尺寸 (列 × 行)、pinned 标记、
  退出状态。存储在 SQLite 中，所以会话条目在重启后仍然存在。
- **一个活跃的 PTY** (仅在子进程运行时存在) —— OS 伪终端、
  其字节流输出，以及后端为后加入者保留的回滚缓冲区。
- **WebSocket 总线上的实时事件** —— PTY 输出的每一块都会被
  base64 编码并以 `terminal.output` 广播。生命周期事件
  (`terminal.created`、`terminal.updated`、`terminal.exit`、`terminal.removed`)
  也走同一条总线。渲染进程中的 xterm.js 视图订阅并渲染这条流。

PTY 子进程不能被暂停或在进程间迁移：当子进程退出时，
列表行保留，但活跃的 PTY 没了。重新启动是原地进行的 —— 同一个会话 id
会附上一个全新的进程，所以你不会每次重启 CLI 都得到一个新的侧边栏
条目。

[`portable-pty`]: https://crates.io/crates/portable-pty

## 创建终端

打开终端创建页面 (终端侧边栏区段中的 **+** 按钮，或导航到
`/terminal-new`)。你需要选择五样东西：

1. **Workspace** —— 子进程将在其中派生的工作目录。
   最近使用过的 workspace 会被记住。
2. **Preset** —— `Shell`、`Claude Code`、`Codex` 或 `Gemini`。shell
   preset 会在启动时解析为你平台的 login shell (Windows：
   PowerShell/`cmd`，macOS/Linux：`$SHELL`)；agent preset 会启动
   对应的 CLI 二进制，该二进制必须已安装并在 `PATH` 上。
3. **权限模式** (仅 agent preset) —— `Default` (交互式审批)
   或 `Full Auto` (会附加该 CLI 自身的非交互式 flag)：

   | Preset       | Full-auto flag                              |
   | ------------ | ------------------------------------------- |
   | `claude`     | `--dangerously-skip-permissions`            |
   | `codex`      | `--dangerously-bypass-approvals-and-sandbox`|
   | `gemini`     | `--yolo`                                    |

   这些 flag 会绕过 CLI 的交互式审批提示 —— 这是 AutoWork 在没有
   人按回车的情况下端到端驱动一轮所必需的，但同样的 flag 也赋予了
   CLI 在你机器上的广泛能力。请把 full-auto 终端当作已登录的 shell 来对待。
4. **启动命令** —— 对话框会把解析后的 `command + args` 渲染到
   一个可编辑字段中。在按下 **Launch** 之前可以自由调整 (额外
   flag、替代入口点等)。
5. **知识库** (可选) —— 多选一个或多个知识库绑定到本会话。绑定的库
   会在子进程派生前挂载到 `{workspace}/.nomi/knowledge/`，并生成一份
   `README.md` (检索协议 + 各库梗概 + TOC + 回写规则)；`claude`
   preset 还会额外附加一条指向该 README 的 `--append-system-prompt`
   指针。改绑在下次重新启动时生效。(网关工具 `nomi_create_terminal`
   通过 `knowledge_base_ids` 支持同样的绑定。)

![终端创建页面](../images/terminal-02-create-page.png)

后端会持久化该行并派生子进程。页面会跳转到
`/terminal/<id>`，然后你开始接收实时输出。

## 驱动终端

会话页面是与实时流相连的 xterm.js：

- **键入** 把击键发送给 PTY。发送框也接受带 bracketed-paste 标记
  的粘贴，所以多行文本会变成一次粘贴而不是一连串的回车。
- **调整大小** 调整面板大小，后端会相应调整 PTY 尺寸 (会向子进程
  发送 `SIGWINCH`)。新的尺寸会被持久化。
- **重新启动** 在子进程退出后：单个按钮会杀掉同一 id 的任何残留
  PTY，使用存储的命令 + cwd + env 派生一个新的进程，清空视图，
  同样的 `terminal.<id>` 订阅会接管新的输出。你保留同一个侧边栏条目。
- **重命名 / 置顶** 从会话头进行 (重命名会作为
  `terminal.updated` 广播；置顶的终端会浮到侧边栏顶部)。
- **Kill** 停止子进程但保留行 (它会转换为 `exited` 并可重新启动)。
  **Delete** 杀掉子进程并完全移除该行。

![驱动一个终端会话](../images/terminal-01-session.png)

## 流模型

输出走单个 WebSocket。当你正在查看一个会话时，你的客户端
会接收到该 id 的 `terminal.output` 事件并渲染它们。在 PTY 活跃期间，
后端在内存中保留一个回滚缓冲区：当你打开一个已经在运行的
终端时，GET 响应会包含一个 base64 编码的 `scrollback_b64` 快照，
所以 xterm 会先回放历史记录，然后实时事件再流入。

客户端到服务器的输入走另一个方向，通过一个小的 REST 端点
(base64 编码的字节)。后端会把这些字节直接写到 PTY 的 stdin。

## 终端作为自动化目标

驱动 UI 的同一个内存中的 PTY 映射通过 `TerminalDriver` trait
与 `nomifun-requirement` 中的 **AutoWork 执行循环** 共享。该
trait 让 AutoWork：

- 订阅终端实时输出的副本 (它会监视完成标记并检测静默 ——
  契约见 AutoWork 指南)。
- 向 PTY 写入输入字节 (它把 requirement prompt 包装在
  bracketed-paste 中注入，使得多行指令会作为单次粘贴落地)。
- 检查存活性，读取该行的元数据 (user、backend、mode)，并读取或
  写入每个终端的 `autowork` 配置 blob。

换句话说：**你在这里创建的终端可被 AutoWork 自动化**。
在会话头的 AutoWork 工具栏上绑定一个 tag，AutoWork 循环
就会开始认领 requirement 并把它们喂给运行在该终端中的 CLI。
只有 agent-CLI 终端 (`claude`、`codex`、`gemini`) 才符合条件 ——
普通的 shell 可以手动驱动但不是 AutoWork 目标。AutoWork 循环也
推荐使用 Full Auto 模式，因为一轮如果撞上交互式审批提示
会一直阻塞直到超时。

如果工作区挂载了知识库 (存在 `{cwd}/.nomi/knowledge/`)，AutoWork 与
cron 驱动注入的 prompt 会自动前置一行提示，让 CLI 先阅读挂载目录里的
`README.md` 再开工。

如果在 AutoWork 仍绑定时 PTY 退出，循环不会停止 —— 它会
空转并等待你重新启动该终端，然后从中断处继续认领。
如果你删除该行，循环会彻底停止。

## IDMM (决策停滞监督)

长时间运行的 CLI 会话有时会停滞：provider 掉线，模型在某个工具
调用上空转，CLI 打印了一个无人回答的确认提示。IDMM
(Intelligent Decision-Making Mode) supervisor 会监视会话并介入 ——
先用基于规则的轻推 (无 LLM)，然后调用一个 sidecar 备用模型 ——
这样这一轮会到达一个终态，而不是挂起到 AutoWork 超时触发。

你可以在同一个会话头 (AutoWork 旁边的 **IDMM** 控件) 中按终端
启用 IDMM。无论 AutoWork 是否同时绑定它都能工作；当两者都开启
时，AutoWork 会确保 IDMM 在每一轮的全程都在监督。

## 路由 & API

| 内容                     | 位置                                          |
| ------------------------ | --------------------------------------------- |
| 创建页面                 | `/terminal-new`                               |
| 会话页面                 | `/terminal/:id`                               |
| 列出 / 创建              | `GET /api/terminals`，`POST /api/terminals`   |
| 获取 / 更新 / 删除       | `GET|PATCH|DELETE /api/terminals/:id`         |
| 发送输入                 | `POST /api/terminals/:id/input`               |
| 调整大小                 | `POST /api/terminals/:id/resize`              |
| 杀掉子进程               | `POST /api/terminals/:id/kill`                |
| 原地重新启动             | `POST /api/terminals/:id/relaunch`            |
| 实时输出 / 生命周期      | WebSocket 事件 `terminal.*`                    |

## 故障排查

- **找不到 CLI。** Agent preset 直接调用 `claude`、`codex` 或
  `gemini` —— 它们必须在运行后端的账户的 `PATH` 上。要么全局安装
  CLI，要么在启动前编辑启动命令使用绝对路径。
- **AutoWork 绑定是灰色的。** 当前只有 `claude`/`codex` 终端才是
  AutoWork 目标。普通 shell preset 不能被绑定；Gemini 终端 AutoWork
  还没有接入后端的完成契约。
- **重新启动一直复用同一个 env / cwd。** 这是有意为之 —— 会话
  行存储着它们。要修改它们，请用想要的设置创建一个新的终端。
- **调整大小后输出乱了。** 一些 TUI 在 `SIGWINCH` 时需要重绘。
  按 `Ctrl-L` (或你 CLI 的重绘快捷键)。
