# 桌面伙伴直接操作终端会话（发送并执行 + 读取输出）— 设计文档

日期：2026-07-06
状态：设计已获用户确认（完整闭环范围），进入实现

## 背景与问题

用户反馈：桌面伙伴无法直接向终端会话发送消息——期望「直接发送到命令行并直接执行，不需要用户再操作」，完成会话操作能力的闭环。

## 关键现状（探索结论）

- **权限层已通**：伙伴跑在 Desktop surface + WORK profile（`team_mcp.rs` 的 `WORK_DOMAINS` 含 `"terminal"`），terminal 域全部工具对伙伴可见且放行（`nomi_terminal_write_input` 仅 `deny_on(Channel)`）。
- **缺口一（提示词）**：伙伴系统提示词（`nomifun-companion/src/companion.rs` `build_companion_system_prompt`）逐一教了会话/编排/知识库工具，但从未提及终端工具——伙伴不知道自己能操控终端。
- **缺口二（执行语义，用户所报症状的根因）**：`nomi_terminal_write_input` 是裸 base64 写 PTY。`文本+\r` 一次写入时，claude/codex/gemini 等 agent TUI 的 paste-burst 检测会吞掉回车，文字滞留输入框不执行。正确序列（bracketed-paste 包裹 → 间隔 120ms → 单独写 `\r`）已存在于 AutoWork 编排器（`nomifun-requirement/src/orchestrator.rs` `terminal_submit_chunks`/`submit_terminal_prompt`），但是 crate 私有函数。base64 编码本身对模型也是易错门槛。
- **缺口三（读输出）**：网关无任何读取终端输出的工具。`nomi_terminal_get` 丢弃了后端 `terminal_service.get()` 本已返回的 scrollback。终端侧没有 `nomi_conversation_status` 的对应品。
- **缺口四（完成感知）**：claude/codex 终端的回合结束 lifecycle 信号（`TerminalDriver::subscribe_lifecycle`，Stop hook → `TerminalLifecycleServer` 广播）只在进程内供 AutoWork 用，网关未暴露。
- **实现分歧**：「向 TUI 提交文字」现存三份独立实现——AutoWork 编排器（paste 与 CR 分两次写 + 120ms 延迟）、IDMM probe（`encode_terminal_input`：单行裸写+CR、多行 paste+CR 单次写）、前端 `TerminalSendBox`（xterm 运行时探测 bracketed-paste 决定包裹）。
- **前端零改动**：伙伴写入与前端共用同一 `TerminalService::input` → PTY → `terminal.output` 全局广播链路，终端页挂载即实时可见；未挂载时靠 scrollback 重放补齐。
- **约束**：`input()`/`write_input` 对无活 PTY 的会话返回 NotFound，exited 会话需先 relaunch。`input()` 有副作用（arm IDMM 监督 + 首行自动命名），`TerminalDriver::write_input` 无。

## 采用方案：提交原语下沉 + 新网关能力组（完整闭环）

把「向 TUI 提交文字并回车」下沉为 `nomifun-terminal` 的公共原语，网关新增「发送并执行」「读取输出」两个高层工具，伙伴提示词补终端指引，并把既有两份后端提交实现收编到共享原语上。

被否决的备选：
- *网关层自包含*：只在 `caps_terminal_ext.rs` 复制提交序列。否决——提交逻辑出现第 4 份拷贝，与既有三份继续分歧。
- *纯提示词*：教伙伴用现有 write_input 自拼 base64+`\r`。否决——无法根治 agent TUI 吞回车，且没有读输出能力，闭环闭不上。

## 设计细节

### 1. 共享提交原语（`crates/backend/nomifun-terminal`，新增 `submit.rs`）

- `pub fn encode_submit_chunks(text, backend) -> SubmitChunks`，按会话 `backend` 字段路由：
  - **agent TUI（claude/codex/gemini）**：bracketed-paste 包裹（`\x1b[200~…\x1b[201~`）→ 间隔 120ms → 单独一次写 `\r`（照搬 AutoWork 已验证序列）。
  - **shell（backend=None）**：`text + \r` 单次写（shell 无 paste-burst 问题；多行文本按 shell 语义逐行执行）。
- `pub async fn TerminalService::submit_text(id, text)`：解析会话 backend → 编码 → 经 `TerminalDriver::write_input` 写 PTY（不误触 IDMM 监督武装与首行自动命名）。
- `pub async fn TerminalService::await_turn_settle(id, timeout) -> SettleReason`，双策略完成感知：
  - 有 lifecycle hook 的终端（claude/codex 且启动期注入了 hook）：订阅 `TurnEnd` → 返回 `turn_end`。
  - 无 hook（shell/gemini）：静默判定，连续约 700ms 无新输出（经 `subscribe_output`）视为安定 → 返回 `idle`。
  - 超时 → `timeout`。返回值如实标注原因，不把 `idle` 谎报为「完成」。

### 2. 新网关能力（`caps_terminal_ext.rs` 追加注册，域 `terminal`）

- **`nomi_terminal_send`**（DangerTier::Write，`deny_on(Channel)`，与 write_input 同门禁）：
  - 参数 `{ id, text（纯文本，无 base64）, wait?=false, timeout_secs?=300 }`。
  - 行为：会话须 running（exited 报错并提示先 `nomi_terminal_relaunch`）→ `submit_text` → `wait=true` 时 `await_turn_settle` 后附带输出尾巴。
  - 返回 `{ submitted, id, settle_reason?, output_tail?, note? }`（fire-and-forget 带 note 提示用 read_output 查看；wait=true 带 settle_reason + output_tail）。
- **`nomi_terminal_read_output`**（DangerTier::Read）：
  - 参数 `{ id, max_bytes?（默认尾部 16KiB）}`。
  - 行为：透传后端已有的 scrollback，解码 + 剥离 ANSI 转义，返回 `{ text, truncated, status }`——终端版的 `nomi_conversation_status`。
- 现有 `nomi_terminal_write_input` 保留（控制键如 Ctrl-C 仍需要它），描述文案追加「发送文本/命令请优先用 nomi_terminal_send」。

### 3. 伙伴提示词教学（`companion.rs` `build_companion_system_prompt`）

增补终端操作指引一段：工具清单（create/list/send/read_output）、典型流程（列出 → 发送 → 等待/读结果）、exited 会话先 relaunch、破坏性操作（kill/delete）会要求确认、用户在终端页能实时看到伙伴的操作。中英文措辞与该提示词现有段落风格一致。

### 4. 收编分歧实现（针对性重构，行为不回退）

- `nomifun-requirement/orchestrator.rs` 的 `terminal_submit_chunks`/`submit_terminal_prompt` → 改调共享原语（agent TUI 路径就是从它照搬，行为不变）。
- `nomifun-idmm/probe.rs` 的 `encode_terminal_input` → 改调共享编码器（对 agent 终端更稳：其现状单行裸写在快速注入下仍有被当 paste-burst 的风险）。
- 前端 `TerminalSendBox` 不动（xterm 运行时 bracketed-paste 探测是不同且合理的机制）。

### 5. 保持不动（不可破坏）

- `nomi_terminal_write_input` 的裸字节语义与现有调用方。
- AutoWork 的完成裁决体系（requirement_complete/needs_review 状态机）。
- 前端输入/输出链路与事件管线。
- Surface 门禁矩阵（不放宽渠道限制，不新增审批门）。

## 明确不做（YAGNI）

- 编排层（nomifun-orchestrator）的终端任务节点。
- 前端「发送到终端」的用户 UI 入口。
- 写入审批门/外部 agent 写入标识（Desktop 危险矩阵已放行 Write 级操作，维持一致）。

## 受影响文件（预估）

新增：
- `crates/backend/nomifun-terminal/src/submit.rs`

修改：
- `crates/backend/nomifun-terminal/src/service.rs`（`submit_text`/`await_turn_settle`）与 `lib.rs`（导出）
- `crates/backend/nomifun-gateway/src/caps_terminal_ext.rs`（两个新能力 + write_input 描述）
- `crates/backend/nomifun-companion/src/companion.rs`（提示词）
- `crates/backend/nomifun-requirement/src/orchestrator.rs`（改调共享原语）
- `crates/backend/nomifun-idmm/src/probe.rs`（改调共享编码器）

## 验收标准

1. 伙伴聊天里说「在 xx 终端里跑 `git status` 并告诉我结果」，伙伴完成发送 → 执行 → 读回结果全程，无需用户碰终端；对 shell 与 claude/codex 终端均可靠（回车不被吞）。
2. `nomi_terminal_send` 对 exited 会话给出可行动的报错；`wait=true` 时按 `turn_end`/`idle`/`timeout` 如实返回。
3. `nomi_terminal_read_output` 返回剥离 ANSI 后的 scrollback 尾部，超长截断标注。
4. AutoWork 与 IDMM 既有行为回归通过（`cargo test` terminal/gateway/requirement/idmm + 既有 e2e）。
5. 前端无改动，`bun run build` 通过。
