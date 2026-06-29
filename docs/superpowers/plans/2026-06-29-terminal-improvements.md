# 终端三项改进 实现计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax.

**Goal:** 终端会话自动标题、claude/codex 卡死后可靠回退 shell、App 退出时清理全部会话。

**Architecture:** 后端按功能分三块改 `nomifun-terminal`（service/routes/新 title.rs）、`nomifun-db`（repo delete_all）、`nomifun-app`（services 接线 + desktop shutdown 桥）、`apps/desktop`（RunEvent 钩子）；前端集中在 `ui/.../terminal/*` + `ipcBridge/httpBridge` + i18n。复用既有 `relaunch`/`update_meta`/`one_shot_completion`/`LiveKnowledgeCompleter`/`SHELL_SENTINEL` 模式。

**Tech Stack:** Rust（axum、sqlx 运行时查询、tokio、async-trait、dashmap）、React + TypeScript（xterm.js、Arco、bun:test）。

## Global Constraints

- 注释/文案随上下文用中文；交流用中文。提交署名 `nomifun <rika00@qq.com>`，**不**加 `Co-Authored-By`：`git -c user.name=nomifun -c user.email=rika00@qq.com commit --author="nomifun <rika00@qq.com>" -m ...`。
- 分支：`feat/terminal-improvements`（已建，设计文档已提交）。
- Rust 测试：`cargo test -p <crate> <filter>`。sqlx 用运行时查询（`sqlx::query`/`query_as`），占位符匿名 `?` 顺序 `.bind`。
- 前端测试 `bun:test`：`cd ui && bun test <path>`，`import { describe, expect, test } from 'bun:test'`。
- `ITerminalRepository` 是 trait + SqliteImpl + 测试 MemRepo（service.rs 内）双实现 —— 新增 trait 方法必须同时改 **3 处**。
- 终端会话**无 conversation 关联**；标题用 App 默认 provider/model（首个 enabled），失败/无模型兜底取用户输入前 N 字。

---

## Part C — 功能③ 退出清理（先做，最自包含）

### Task C1：repo `delete_all`（trait + sqlite + MemRepo）

**Files:**
- Modify: `crates/backend/nomifun-db/src/repository/terminal.rs`（trait 末尾加方法）
- Modify: `crates/backend/nomifun-db/src/repository/sqlite_terminal.rs`（impl + 测试）
- Modify: `crates/backend/nomifun-terminal/src/service.rs`（MemRepo impl）

**Interfaces:**
- Produces: `async fn delete_all(&self) -> Result<u64, DbError>`（删除全部 terminal_sessions 行，scrollback 经 FK CASCADE 删除；返回删除行数）。

- [ ] **Step 1**：trait 加 `async fn delete_all(&self) -> Result<u64, DbError>;`（文档注释说明 CASCADE 清 scrollback、整表删除、无 NotFound）。
- [ ] **Step 2**：sqlite 实现 `DELETE FROM terminal_sessions`（无 WHERE），返回 `result.rows_affected()`。
- [ ] **Step 3**：MemRepo（service.rs tests）实现：清空 `rows` 与 `scrollback`，返回清空前 rows 数。
- [ ] **Step 4**：sqlite 测试 `delete_all_clears_rows_and_scrollback_cascade`：建 2 行 + save_scrollback → delete_all 返回 2 → list 为空 + load_scrollback None。
- [ ] **Step 5**：`cargo test -p nomifun-db sqlite_terminal`。
- [ ] **Step 6**：提交。

### Task C2：`TerminalService::shutdown_cleanup`

**Files:** Modify `crates/backend/nomifun-terminal/src/service.rs`

**Interfaces:**
- Consumes: `repo.delete_all()`。
- Produces: `pub async fn shutdown_cleanup(&self) -> Result<u64, TerminalError>`（kill 全部 live PTY、清 pending_spawn、`repo.delete_all()`）。

- [ ] **Step 1**：实现：`for e in self.live.iter() { let _ = e.value().kill(); }`，`self.live.clear()`，`self.pending_spawn.clear()`，`let n = self.repo.delete_all().await?;` `Ok(n)`。注释强调仅 real-quit 调用。
- [ ] **Step 2**：测试 `shutdown_cleanup_kills_and_deletes_all`：建 2 个 `cat` 会话 → shutdown_cleanup → `list("u")` 为空。
- [ ] **Step 3**：`cargo test -p nomifun-terminal shutdown_cleanup`。
- [ ] **Step 4**：提交。

### Task C3：`DesktopServer` 持有 terminal_service + 阻塞清理桥

**Files:** Modify `crates/backend/nomifun-app/src/desktop.rs`

**Interfaces:**
- Consumes: `AppServices.terminal_service: Arc<TerminalService>`（pub）、`self.runtime: Handle`。
- Produces: `pub fn shutdown_terminals_blocking(&self)`（在 backend runtime 上 spawn `shutdown_cleanup` 并用 std mpsc `recv_timeout(3s)` 阻塞等待；超时/错误仅 warn）。

- [ ] **Step 1**：`DesktopServer` 加字段 `terminal_service: Arc<nomifun_terminal::TerminalService>`；`start()` 构造时 `terminal_service: services.terminal_service.clone()`（在 move 进 keep_alive 前）。
- [ ] **Step 2**：实现 `shutdown_terminals_blocking`：`let ts = self.terminal_service.clone(); let (tx, rx) = std::sync::mpsc::channel(); self.runtime.spawn(async move { let r = tokio::time::timeout(Duration::from_secs(3), ts.shutdown_cleanup()).await; let _ = tx.send(r); });` 然后 `match rx.recv_timeout(Duration::from_secs(4)) { ... }` 仅 log。
- [ ] **Step 3**：`cargo build -p nomifun-app`。
- [ ] **Step 4**：提交。

### Task C4：`apps/desktop` RunEvent::ExitRequested 钩子

**Files:** Modify `apps/desktop/src/main.rs`

- [ ] **Step 1**：重写 `handle_run_event`：保留 macOS Reopen 分支；新增（全平台）`tauri::RunEvent::ExitRequested { .. } => { if let Some(server) = app.try_state::<Arc<DesktopServer>>() { server.shutdown_terminals_blocking(); } }`。注释说明：ExitRequested 只在真正退出时触发（close-to-tray 走 prevent_close 不触发），故无需 QuitFlag 守卫即安全。删除 `#[cfg(not(macos))] let _=(app,event)`（现在两平台都用到）。
- [ ] **Step 2**：`cargo build -p nomifun-desktop`（或 workspace build）。
- [ ] **Step 3**：提交。

---

## Part B — 功能② 回退 Shell（后端）

### Task B1：`TerminalService::relaunch_as_shell`

**Files:** Modify `crates/backend/nomifun-terminal/src/service.rs`

**Interfaces:**
- Produces: `pub async fn relaunch_as_shell(&self, id: i64) -> Result<TerminalSessionResponse, TerminalError>`。

- [ ] **Step 1**：克隆 `relaunch` 逻辑，但：① 先 `repo.update_meta` 之外——改为先把行的 command/args/backend 改写为 shell：新增 repo 无关的做法是直接传 `SHELL_SENTINEL` 给 `spawn_pty`；② 持久化 shell 身份，使重启/重连后仍是 shell 且标题变 `Shell`。实现：
  - 读 row；`self.live.remove(&id)` 后 `kill()`；`self.pending_spawn.remove(&id)`；
  - `self.repo.update_command(id, SHELL_SENTINEL, "[]", None)`（见 Step 2 新 repo 方法）；重新 `get_by_id` 得 row2；
  - `spawn_pty(id, SHELL_SENTINEL, &[], &row2.cwd, env=None, cols, rows, vec![], None)`；失败则 `update_status(exited)` 返回 Err；
  - `clear_scrollback`；`update_status("running", None)`；`emit_updated`；`arm_supervision`。
- [ ] **Step 2**：repo 加 `async fn update_command(&self, id, command: &str, args: &str, backend: Option<&str>) -> Result<(), DbError>`（trait + sqlite `UPDATE ... SET command=?, args=?, backend=?, updated_at=?` + MemRepo）。
- [ ] **Step 3**：测试 `relaunch_as_shell_swaps_command_and_emits_updated`：建 `cat` 会话 → relaunch_as_shell → row.command == `$SHELL`、last_status==running、捕获到 `terminal.updated`。
- [ ] **Step 4**：`cargo test -p nomifun-terminal relaunch_as_shell`。
- [ ] **Step 5**：提交。

### Task B2：路由 `POST /api/terminals/{id}/relaunch-shell`

**Files:** Modify `crates/backend/nomifun-terminal/src/routes.rs`

- [ ] **Step 1**：加 route + handler `relaunch_shell_terminal`（仿 `relaunch_terminal`，调 `relaunch_as_shell`）。
- [ ] **Step 2**：`cargo build -p nomifun-terminal`。
- [ ] **Step 3**：提交。

---

## Part A — 功能① 自动标题

### Task A1：`title.rs`（提示词 + completer trait + Live 实现）

**Files:**
- Create: `crates/backend/nomifun-terminal/src/title.rs`
- Modify: `crates/backend/nomifun-terminal/src/lib.rs`（`mod title;` + re-export）
- Modify: `crates/backend/nomifun-terminal/Cargo.toml`（加 `nomifun-ai-agent`）

**Interfaces:**
- Produces:
  - `pub trait TerminalTitleCompleter: Send + Sync { async fn summarize(&self, content: &str) -> Result<String, AppError>; }`
  - `pub struct LiveTerminalTitleCompleter { provider_repo, encryption_key:[u8;32], workspace:PathBuf }`（仿 `LiveKnowledgeCompleter`：`resolve_default_model` + `one_shot_completion(TITLE_SYSTEM, content, 64)`）。
  - `pub fn fallback_title(input: &str, n: usize) -> String`（取首行、去控制字符、截断 N 字，trim）。
  - `const TITLE_SYSTEM`：要求只输出 ≤6 词/≤20 字的工作内容短标题，无标点结尾、无引号、无解释。

- [ ] **Step 1**：写 `fallback_title` 失败测试（首行截断、去 `\r\n`、去 ANSI/控制符、空输入返回空）。
- [ ] **Step 2**：实现 `fallback_title` + `TITLE_SYSTEM` + trait + `LiveTerminalTitleCompleter`（依 knowledge_completer.rs 拷贝 resolve/complete）。
- [ ] **Step 3**：Cargo.toml 加依赖；lib.rs `mod title; pub use title::{...};`。
- [ ] **Step 4**：`cargo test -p nomifun-terminal title::`。
- [ ] **Step 5**：提交。

### Task A2：service 接入触发（首行捕获 + TurnEnd + 一次性守卫）

**Files:** Modify `crates/backend/nomifun-terminal/src/service.rs`

**Interfaces:**
- Consumes: `title::{TerminalTitleCompleter, fallback_title}`、`default_name`、`update_meta`。
- Produces:
  - 字段 `title_completer: Arc<RwLock<Option<Arc<dyn TerminalTitleCompleter>>>>`、`titled: Arc<DashMap<i64,()>>`、`first_input: Arc<DashMap<i64,String>>`。
  - `pub fn with_title_completer(&self, c: Arc<dyn TerminalTitleCompleter>)`。
  - `fn capture_first_input(&self, id, bytes)`（input() 内调用，累积首行到换行，封顶 ~200 字）。
  - `async fn maybe_autotitle(&self, id, llm_source: Option<String>)`：`titled` 守卫 + `name==default_name` 守卫 → 有 completer 且有 llm_source 则 `summarize`，否则/失败 `fallback_title(first_input)`；非空才 `update_meta`。

- [ ] **Step 1**：`input()` 内（写 PTY 后）调 `capture_first_input`；shell 路径：首行收齐后 `tokio::spawn` 调 `maybe_autotitle(id, None)`。
- [ ] **Step 2**：`spawn_pty` 的 lifecycle 消费者（515-529）：收到首个 `TurnEnd` 时取 `payload["last_assistant_message"]`，`spawn` 调 `maybe_autotitle(id, Some(msg))`。
- [ ] **Step 3**：守卫实现：先查 `titled.insert(id,()).is_none()` 抢占（已存在则 return）；reload row，比对 `row.name == default_name(row.command, row.backend)`，不等则 return（用户改过名或已命名）。
- [ ] **Step 4**：测试：①无 completer 时 shell 首行 `echo hi` → 标题变 `hi`（fallback）；② 注入 fake completer → maybe_autotitle 用其结果；③ 已改名（name!=default）→ 不覆盖；④ 触发两次只生效一次。
- [ ] **Step 5**：`cargo test -p nomifun-terminal autotitle`。
- [ ] **Step 6**：提交。

### Task A3：services.rs 注入 Live completer

**Files:** Modify `crates/backend/nomifun-app/src/services.rs`

- [ ] **Step 1**：在 terminal 接线块（440-459）后：`terminal_service.with_title_completer(Arc::new(LiveTerminalTitleCompleter { provider_repo: provider_repo.clone(), encryption_key, workspace: data_dir.clone() }));`（`nomifun_terminal::LiveTerminalTitleCompleter`）。
- [ ] **Step 2**：`cargo build -p nomifun-app`。
- [ ] **Step 3**：提交。

---

## Part D — 功能② 前端（reset / 回退按钮 / 连续 Ctrl+C / 重连重放）

### Task D1：IPC + WS 桥

**Files:** Modify `ui/src/common/adapter/ipcBridge.ts`、`ui/src/common/adapter/httpBridge.ts`

- [ ] **Step 1**：`ipcBridge.terminal` 加 `relaunchShell: httpPost<TerminalSession, void>('/api/terminals/{id}/relaunch-shell')`（仿 `relaunch`，按现有写法填 id）。
- [ ] **Step 2**：`httpBridge.ts` WS `open` 处理：若为重连（attempt>0 / 曾断开），向监听器派发内部事件（如新增 `wsEmitter.emitReconnected()` 或复用一个 `terminal.__ws_reconnected` 名）。导出供 XtermView 订阅。
- [ ] **Step 3**：`tsc` 通过；提交。

### Task D2：XtermView reset + 重连重放 + 连续 Ctrl+C

**Files:** Modify `ui/src/renderer/pages/terminal/XtermView.tsx`（+ 纯函数测试文件）

- [ ] **Step 1**：`XtermViewHandle` 加 `reset: () => void`，实现 `term.reset()`。
- [ ] **Step 2**：订阅 WS 重连事件：回调里 `term.reset()` 后重新 `ipcBridge.terminal.get(sessionId)`，用现有 streaming decoder 重放 `scrollback_b64`。
- [ ] **Step 3**：连续 Ctrl+C 检测纯函数 `bumpCtrlC(state, nowMs, windowMs, threshold)` + 测试；onData 里识别 `\x03`，达阈值回调 `onEscalateShell`。
- [ ] **Step 4**：`cd ui && bun test`（纯函数）；`tsc`；提交。

### Task D3：TerminalSessionPage 回退 Shell 入口 + 接线

**Files:** Modify `ui/src/renderer/pages/terminal/TerminalSessionPage.tsx`、i18n `terminal.json`（zh-CN/en-US）

- [ ] **Step 1**：会话头加「回退 Shell」按钮（**不**受 `isExited` 限制），点击 `ipcBridge.terminal.relaunchShell(id)` 后 `xtermApi.current?.reset()`。
- [ ] **Step 2**：连接 XtermView `onEscalateShell` → 同上回退逻辑 + 提示条 i18n `terminal.fallbackShellHint`。
- [ ] **Step 3**：i18n 加 `fallbackShell`/`fallbackShellHint`（中英）。
- [ ] **Step 4**：`tsc`；提交。

---

## 自检（写完计划后）

- 覆盖：①标题（A1-A3）②回退（B1-B2 后端 + D1-D3 前端）③清理（C1-C4）—— 三需求均有任务。
- 类型一致：`delete_all`/`shutdown_cleanup`/`relaunch_as_shell`/`with_title_completer`/`relaunchShell` 跨任务命名一致。
- 兜底：标题无模型/失败 → `fallback_title(first_input)`，已在 A2 覆盖。
