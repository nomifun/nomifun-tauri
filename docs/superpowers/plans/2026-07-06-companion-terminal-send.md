# 桌面伙伴直接操作终端会话 实现计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 让桌面伙伴能把消息/命令直接发进终端会话并执行、读回执行结果、可选等待回合结束——完成会话操作能力的闭环。

**Architecture:** 把「向 PTY 提交文字并回车」下沉为 `nomifun-terminal` 的共享原语（纯编码函数 + 服务方法），网关新增 `nomi_terminal_send`（发送并执行，可选等待+回执）与 `nomi_terminal_read_output`（读输出），伙伴系统提示词补终端操作指引，最后把 AutoWork 与 IDMM 各自的提交实现收编到共享原语。伙伴的权限面本就放行 terminal 域（Desktop surface + WORK profile），无需改门禁。

**Tech Stack:** Rust（tokio、async-trait、portable-pty、axum、schemars/serde）、gateway capability registry；前端零改动。

## Global Constraints

- **提交序列的两条铁律**（编码器必须同时满足，否则回归 bug）：
  - 单行文本（无内部 `\n`）绝不能用 bracketed-paste 包裹——包裹会「插入但不提交」，文本滞留输入框（见 `nomifun-idmm/src/probe.rs:1126-1138` 注释）。单行一律 `raw + \r` 一次写。
  - 多行文本发给 agent TUI（claude/codex/gemini）时，提交 CR 必须与 paste-end 标记分成两次写、间隔 120ms——同批写的 CR 会被 paste-burst 检测吞掉（见 `nomifun-requirement/src/orchestrator.rs:1053-1060` 注释）。
- **agent TUI 判定**用 `nomifun_terminal::resolve_agent_family(program, args, backend).is_some()`（含 gemini）；**lifecycle 完成信号判定**用 `nomifun_terminal::terminal_autowork_capable(command, args, backend)`（仅 claude/codex，gemini 无 hook）。二者不同，勿混用。
- **写入走 raw PTY 路径**（`TerminalDriver::write_input`）：伙伴的发送是刻意驱动，不武装 IDMM 监督、不触发首行自动命名（与 `TerminalService::input` 的用户活动语义区分）。
- **提交常量单一来源**：`TERMINAL_SUBMIT_DELAY = Duration::from_millis(120)` 定义在新 `submit.rs`，AutoWork 现有同名私有常量迁移引用它。
- **不放宽门禁**：新 send 能力 `DangerTier::Write` + `.deny_on(&[Surface::Channel])`（与 `nomi_terminal_write_input` 一致）；read 能力 `DangerTier::Read`。不新增审批门。
- **提交作者**：所有 commit 用 `--author="nomifun <rika00@qq.com>"`，不加 Co-Authored-By。
- **前端不改**（伙伴写入经 `terminal.output` 全局广播已实时可见）。

---

### Task 1: 共享提交编码器（`nomifun-terminal/src/submit.rs`）

**Files:**
- Create: `crates/backend/nomifun-terminal/src/submit.rs`
- Modify: `crates/backend/nomifun-terminal/src/lib.rs`（加 `pub mod submit;` 与 re-export）
- Test: 同文件 `#[cfg(test)] mod tests`

**Interfaces:**
- Produces:
  - `pub const TERMINAL_SUBMIT_DELAY: std::time::Duration`（120ms）
  - `pub const IDLE_SETTLE_WINDOW: std::time::Duration`（700ms）
  - `pub enum SubmitChunks { Single(Vec<u8>), PasteThenCr { paste: Vec<u8>, cr: Vec<u8> } }`（`#[derive(Debug, Clone, PartialEq, Eq)]`）
  - `pub fn encode_submit_chunks(text: &str, is_agent_tui: bool) -> SubmitChunks`

- [ ] **Step 1: 写失败测试**

在 `crates/backend/nomifun-terminal/src/submit.rs` 写入（含实现骨架先留空以便测试先失败——本步只写测试，实现放 Step 3）：

```rust
#[cfg(test)]
mod tests {
    use super::*;

    const PASTE_START: &[u8] = b"\x1b[200~";
    const PASTE_END: &[u8] = b"\x1b[201~";

    #[test]
    fn single_line_is_raw_plus_cr_for_any_target() {
        // 单行：shell 与 agent 都必须是 raw + CR，一次写，绝不 bracketed-paste。
        for agent in [false, true] {
            assert_eq!(
                encode_submit_chunks("git status", agent),
                SubmitChunks::Single(b"git status\r".to_vec()),
                "agent={agent}"
            );
        }
    }

    #[test]
    fn trailing_crlf_is_stripped_before_routing() {
        // 结尾换行不得把单行误判成多行 paste 路径。
        assert_eq!(
            encode_submit_chunks("ls\r\n", false),
            SubmitChunks::Single(b"ls\r".to_vec())
        );
        assert_eq!(
            encode_submit_chunks("ls\n", true),
            SubmitChunks::Single(b"ls\r".to_vec())
        );
    }

    #[test]
    fn multiline_to_agent_is_paste_then_separate_cr() {
        let out = encode_submit_chunks("line1\nline2", true);
        match out {
            SubmitChunks::PasteThenCr { paste, cr } => {
                assert!(paste.starts_with(PASTE_START));
                assert!(paste.ends_with(PASTE_END));
                assert!(paste.windows(11).any(|w| w == b"line1\nline2"));
                assert_eq!(cr, b"\r".to_vec());
            }
            other => panic!("expected PasteThenCr, got {other:?}"),
        }
    }

    #[test]
    fn multiline_to_shell_is_paste_plus_cr_one_write() {
        let out = encode_submit_chunks("a\nb", false);
        match out {
            SubmitChunks::Single(bytes) => {
                assert!(bytes.starts_with(PASTE_START));
                // paste-end 之后紧跟一个 CR，同一次写。
                assert!(bytes.ends_with(b"\x1b[201~\r"));
            }
            other => panic!("expected Single, got {other:?}"),
        }
    }
}
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cargo test -p nomifun-terminal submit::tests`
Expected: 编译失败（`encode_submit_chunks`/`SubmitChunks` 未定义）。

- [ ] **Step 3: 写最小实现**

在 `submit.rs` 顶部（tests 之上）写入：

```rust
//! Shared PTY submit encoding: turn a block of text into the ordered writes that
//! make a terminal actually EXECUTE it (as if a human typed it + Enter).
//!
//! Two hard rules this encoder enforces (both are fixes for real bugs):
//!   1. A single logical line is NEVER wrapped in bracketed paste — wrapping it
//!      inserts the text without submitting (it sits unrun in the input box).
//!   2. For multi-line text sent to an agent TUI (claude/codex/gemini) the submit
//!      CR is a SEPARATE write after `TERMINAL_SUBMIT_DELAY` — a CR riding in the
//!      same write as the paste-end marker is swallowed by the CLI's paste-burst
//!      detection. A plain shell has no such detector, so its CR rides along.

use std::time::Duration;

/// Beat between writing the bracketed-paste body and the lone submit CR when the
/// target is an agent TUI. See rule 2 above.
pub const TERMINAL_SUBMIT_DELAY: Duration = Duration::from_millis(120);

/// Output-quiescence window used to presume a non-lifecycle terminal (shell /
/// gemini) has finished a turn: this long with no new PTY output → settled.
pub const IDLE_SETTLE_WINDOW: Duration = Duration::from_millis(700);

/// The ordered PTY writes that submit a block of text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SubmitChunks {
    /// One write of exactly these bytes (raw text + CR, or paste-wrapped + CR).
    Single(Vec<u8>),
    /// Two writes: `paste` first, then `cr` after `TERMINAL_SUBMIT_DELAY`.
    PasteThenCr { paste: Vec<u8>, cr: Vec<u8> },
}

/// Encode `text` into submit chunks. `is_agent_tui` must be true when the target
/// CLI has paste-burst detection (claude/codex/gemini) — resolve it with
/// `resolve_agent_family(..).is_some()`.
pub fn encode_submit_chunks(text: &str, is_agent_tui: bool) -> SubmitChunks {
    let trimmed = text.trim_end_matches(|c| c == '\r' || c == '\n');
    if trimmed.contains('\n') {
        let mut paste = Vec::with_capacity(trimmed.len() + 13);
        paste.extend_from_slice(b"\x1b[200~");
        paste.extend_from_slice(trimmed.as_bytes());
        paste.extend_from_slice(b"\x1b[201~");
        if is_agent_tui {
            SubmitChunks::PasteThenCr { paste, cr: vec![b'\r'] }
        } else {
            paste.push(b'\r');
            SubmitChunks::Single(paste)
        }
    } else {
        let mut bytes = Vec::with_capacity(trimmed.len() + 1);
        bytes.extend_from_slice(trimmed.as_bytes());
        bytes.push(b'\r');
        SubmitChunks::Single(bytes)
    }
}
```

在 `crates/backend/nomifun-terminal/src/lib.rs` 的 `pub mod` 块加一行（紧跟 `pub mod state;` 之后按字母序）：

```rust
pub mod submit;
```

并在 re-export 区（`pub use` 块）加：

```rust
pub use submit::{encode_submit_chunks, SubmitChunks, IDLE_SETTLE_WINDOW, TERMINAL_SUBMIT_DELAY};
```

- [ ] **Step 4: 运行测试确认通过**

Run: `cargo test -p nomifun-terminal submit::tests`
Expected: 4 个测试全 PASS。

- [ ] **Step 5: 提交**

```bash
git add crates/backend/nomifun-terminal/src/submit.rs crates/backend/nomifun-terminal/src/lib.rs
git commit --author="nomifun <rika00@qq.com>" -m "feat(terminal): 共享 PTY 提交编码器（bracketed-paste + 分离CR）"
```

---

### Task 2: `TerminalService::submit_text`

**Files:**
- Modify: `crates/backend/nomifun-terminal/src/service.rs`（新增 inherent method；在 tests 模块加测试）

**Interfaces:**
- Consumes: `encode_submit_chunks` / `SubmitChunks` / `TERMINAL_SUBMIT_DELAY`（Task 1）；`crate::types::resolve_command`、`crate::enhance::resolve_agent_family`（已存在）；`self.write_input`（trait 已实现于 service.rs:1291）。
- Produces: `pub async fn TerminalService::submit_text(&self, id: i64, text: &str) -> Result<(), TerminalError>`

- [ ] **Step 1: 写失败测试**

在 `service.rs` 的 `#[cfg(test)] mod tests` 内新增（`req`/`service`/`collect_output`/`BASE64` 等辅助已存在于该模块）：

```rust
    #[tokio::test]
    async fn submit_text_single_line_executes_via_cat_echo() {
        // cat 会回显收到的字节。shell 后端(None) → 单行 raw+CR 一次写。
        let (svc, _bc) = service();
        let id = svc.create("u", req("cat", &[])).await.unwrap().id;
        svc.submit_text(id, "hello-world").await.unwrap();

        // 等 cat 把 "hello-world\r" 回显进 live scrollback。
        let mut seen = false;
        for _ in 0..40 {
            if let Ok(resp) = svc.get(id).await {
                if let Some(b64) = resp.scrollback_b64 {
                    let s = String::from_utf8_lossy(&BASE64.decode(b64).unwrap()).to_string();
                    if s.contains("hello-world") {
                        seen = true;
                        break;
                    }
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        assert!(seen, "cat should echo the submitted single line");
        svc.delete(id).await.ok();
    }

    #[tokio::test]
    async fn submit_text_not_found_when_not_live() {
        let (svc, _bc) = service();
        assert!(matches!(
            svc.submit_text(999_999, "x").await.unwrap_err(),
            TerminalError::NotFound(_)
        ));
    }
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cargo test -p nomifun-terminal submit_text_`
Expected: 编译失败（`submit_text` 未定义）。

- [ ] **Step 3: 写最小实现**

在 `service.rs` 的 `impl TerminalService { ... }` 块内（`input` 方法附近，`pub async fn` 区）新增：

```rust
    /// Submit a block of text to a terminal so it EXECUTES (as if typed + Enter).
    /// Resolves the target's agent family from its stored command/args/backend to
    /// choose the correct submit sequence (bracketed-paste + separated CR for
    /// agent TUIs, raw + CR for single lines / shells). Uses the raw PTY write
    /// path — this is deliberate driving, so it does NOT arm IDMM supervision or
    /// auto-title the way `input` (user typing) does. `Err(NotFound)` if not live.
    pub async fn submit_text(&self, id: i64, text: &str) -> Result<(), TerminalError> {
        if !self.live.contains_key(&id) {
            return Err(TerminalError::NotFound(id.to_string()));
        }
        let is_agent = match self.describe(id).await? {
            Some(d) => {
                let (program, prog_args) = crate::types::resolve_command(&d.command, &d.args);
                crate::enhance::resolve_agent_family(&program, &prog_args, d.backend.as_deref()).is_some()
            }
            None => false,
        };
        match crate::submit::encode_submit_chunks(text, is_agent) {
            crate::submit::SubmitChunks::Single(bytes) => self.write_input(id, &bytes).await,
            crate::submit::SubmitChunks::PasteThenCr { paste, cr } => {
                self.write_input(id, &paste).await?;
                tokio::time::sleep(crate::submit::TERMINAL_SUBMIT_DELAY).await;
                self.write_input(id, &cr).await
            }
        }
    }
```

> 注：`self.write_input` 是 `TerminalDriver` trait 方法，`impl TerminalDriver for TerminalService` 已提供（service.rs:1291）。在 inherent method 内调用 trait 方法需 trait 在作用域内——`use crate::driver::TerminalDriver;` 通常已在 service.rs 顶部导入（AutoWork 集成使然）。若编译报未导入，在 `submit_text` 前加 `use crate::driver::TerminalDriver as _;` 或复用 `self.live.get(&id)...handle.write(bytes)` 直写（与 write_input 同体）。

- [ ] **Step 4: 运行测试确认通过**

Run: `cargo test -p nomifun-terminal submit_text_`
Expected: 2 个测试 PASS。

- [ ] **Step 5: 提交**

```bash
git add crates/backend/nomifun-terminal/src/service.rs
git commit --author="nomifun <rika00@qq.com>" -m "feat(terminal): TerminalService::submit_text 按 agent 家族路由提交序列"
```

---

### Task 3: `SettleReason` + `TerminalService::await_turn_settle`

**Files:**
- Modify: `crates/backend/nomifun-terminal/src/submit.rs`（加 `SettleReason` 枚举 + re-export）
- Modify: `crates/backend/nomifun-terminal/src/lib.rs`（re-export `SettleReason`）
- Modify: `crates/backend/nomifun-terminal/src/service.rs`（新增 `await_turn_settle` + 测试）

**Interfaces:**
- Consumes: `IDLE_SETTLE_WINDOW`（Task 1）；`crate::enhance::terminal_autowork_capable`、`self.subscribe_lifecycle`、`self.subscribe_output`、`crate::lifecycle::LifecycleKind`（均已存在）。
- Produces:
  - `pub enum SettleReason { TurnEnd, Idle, Timeout, Exited }`（`#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]`, `#[serde(rename_all = "snake_case")]`）
  - `pub async fn TerminalService::await_turn_settle(&self, id: i64, timeout: std::time::Duration) -> SettleReason`

- [ ] **Step 1: 写 SettleReason 与失败测试**

在 `submit.rs`（`encode_submit_chunks` 下方、tests 之上）加：

```rust
/// Why `await_turn_settle` returned. `Idle` is a heuristic (output went quiet),
/// never a definitive "the agent declared done" — reported honestly as such.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SettleReason {
    /// A structured `TurnEnd` lifecycle event arrived (claude/codex).
    TurnEnd,
    /// No lifecycle signal (shell/gemini); PTY output was quiet for the window.
    Idle,
    /// Neither TurnEnd nor an idle window before the caller's timeout.
    Timeout,
    /// The PTY exited during the wait.
    Exited,
}
```

在 `lib.rs` 的 submit re-export 行补 `SettleReason`：

```rust
pub use submit::{encode_submit_chunks, SubmitChunks, SettleReason, IDLE_SETTLE_WINDOW, TERMINAL_SUBMIT_DELAY};
```

在 `service.rs` tests 模块加：

```rust
    #[tokio::test]
    async fn await_turn_settle_idle_when_shell_goes_quiet() {
        // cat(shell,无 lifecycle) 回显后即安静 → Idle。
        let (svc, _bc) = service();
        let id = svc.create("u", req("cat", &[])).await.unwrap().id;
        svc.submit_text(id, "ping").await.unwrap();
        let reason = svc
            .await_turn_settle(id, std::time::Duration::from_secs(5))
            .await;
        assert_eq!(reason, SettleReason::Idle);
        svc.delete(id).await.ok();
    }

    #[tokio::test]
    async fn await_turn_settle_timeout_when_output_never_quiets() {
        // yes 持续刷输出 → 永不 idle；超时短于 idle window → Timeout。
        let (svc, _bc) = service();
        let id = svc.create("u", req("yes", &[])).await.unwrap().id;
        let reason = svc
            .await_turn_settle(id, std::time::Duration::from_millis(400))
            .await;
        assert_eq!(reason, SettleReason::Timeout);
        svc.delete(id).await.ok();
    }

    #[tokio::test]
    async fn await_turn_settle_turn_end_via_lifecycle_for_agent_backend() {
        use crate::lifecycle::TerminalLifecycleServer;
        let (svc, _bc) = service();
        let srv = std::sync::Arc::new(TerminalLifecycleServer::start().await.unwrap());
        svc.with_terminal_lifecycle(srv.clone(), "nomicore".into());

        // 进程用 cat，但声明 backend=claude → 被判为 lifecycle-capable，走 TurnEnd 路径。
        let request = nomifun_api_types::CreateTerminalRequest {
            name: None,
            cwd: std::env::temp_dir().to_string_lossy().into_owned(),
            command: "cat".into(),
            args: vec![],
            env: None,
            backend: Some("claude".into()),
            mode: Some("default".into()),
            cols: 80,
            rows: 24,
            defer_spawn: false,
            knowledge_base_ids: None,
        };
        let id = svc.create("u", request).await.unwrap().id;

        // settle future 与 POST future 用 tokio::join! 同任务并发（svc 非 Clone，
        // 借用即可）。settle 先被 poll → 内部 subscribe_lifecycle 建立订阅；post
        // 延迟 150ms 再发 turn_end hook，事件不会漏。
        let url = format!("http://127.0.0.1:{}/hook", srv.http_port());
        let token = srv.auth_token().to_owned();
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        let body = serde_json::json!({"terminal_id": id, "kind": "turn_end", "payload": {}});
        let settle = svc.await_turn_settle(id, std::time::Duration::from_secs(5));
        let post = async {
            tokio::time::sleep(std::time::Duration::from_millis(150)).await;
            client.post(&url).json(&body).bearer_auth(&token).send().await.unwrap();
        };
        let (reason, _) = tokio::join!(settle, post);
        assert_eq!(reason, SettleReason::TurnEnd);
        svc.delete(id).await.ok();
    }
```

> `await_turn_settle` 内部在 lifecycle 分支先 `subscribe_lifecycle`，`tokio::join!` 里 settle 先 poll 建立订阅，post 延迟 150ms 再发，事件不会漏。

- [ ] **Step 2: 运行测试确认失败**

Run: `cargo test -p nomifun-terminal await_turn_settle_`
Expected: 编译失败（`await_turn_settle` 未定义）。

- [ ] **Step 3: 写最小实现**

在 `service.rs` 的 `impl TerminalService` 块内（`submit_text` 附近）新增：

```rust
    /// Wait for a terminal turn to settle after a submit. Agent CLIs with
    /// lifecycle hooks (claude/codex) resolve via the structured `TurnEnd` event;
    /// shells and gemini fall back to an output-quiescence window
    /// (`IDLE_SETTLE_WINDOW`). Never dresses `Idle` up as definitive completion.
    pub async fn await_turn_settle(
        &self,
        id: i64,
        timeout: std::time::Duration,
    ) -> crate::submit::SettleReason {
        use crate::submit::SettleReason;

        let desc = match self.describe(id).await {
            Ok(Some(d)) => d,
            _ => return SettleReason::Exited,
        };
        let lifecycle_capable = crate::enhance::terminal_autowork_capable(
            &desc.command,
            &desc.args,
            desc.backend.as_deref(),
        );

        if lifecycle_capable {
            if let Some(mut rx) = self.subscribe_lifecycle(id) {
                let fut = async {
                    loop {
                        match rx.recv().await {
                            Ok(ev) if ev.kind == crate::lifecycle::LifecycleKind::TurnEnd => {
                                return SettleReason::TurnEnd;
                            }
                            Ok(_) => continue,
                            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                            Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                                return SettleReason::Exited;
                            }
                        }
                    }
                };
                return tokio::time::timeout(timeout, fut)
                    .await
                    .unwrap_or(SettleReason::Timeout);
            }
        }

        // Idle-quiescence fallback: reset a short quiet-timer on every output
        // chunk; if it elapses, presume settled.
        let Some(mut out_rx) = self.subscribe_output(id) else {
            return SettleReason::Exited;
        };
        let overall = tokio::time::sleep(timeout);
        tokio::pin!(overall);
        loop {
            let quiet = tokio::time::sleep(crate::submit::IDLE_SETTLE_WINDOW);
            tokio::select! {
                _ = &mut overall => return SettleReason::Timeout,
                _ = quiet => return SettleReason::Idle,
                r = out_rx.recv() => match r {
                    Ok(_) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                        return SettleReason::Exited;
                    }
                }
            }
        }
    }
```

- [ ] **Step 4: 运行测试确认通过**

Run: `cargo test -p nomifun-terminal await_turn_settle_`
Expected: 3 个测试 PASS（若 CI 无 `yes` 命令，timeout 测试可改用 `req("cat", &[])` 并在 settle 前后台持续 `write_input` 制造输出——见备选）。

> 备选（无 `yes` 时的 timeout 测试）：改为 spawn `cat`，起一个后台 task 每 100ms `svc.write_input(id, b"x")`，settle timeout 设 400ms，断言 `Timeout`。

- [ ] **Step 5: 提交**

```bash
git add crates/backend/nomifun-terminal/src/submit.rs crates/backend/nomifun-terminal/src/lib.rs crates/backend/nomifun-terminal/src/service.rs
git commit --author="nomifun <rika00@qq.com>" -m "feat(terminal): await_turn_settle（lifecycle TurnEnd / idle 静默 双策略）"
```

---

### Task 4: `TerminalOutputTail` + `TerminalService::read_output_tail`

**Files:**
- Modify: `crates/backend/nomifun-terminal/src/service.rs`（新增 struct + method + 私有 tail helper + 测试）
- Modify: `crates/backend/nomifun-terminal/src/lib.rs`（re-export `TerminalOutputTail`）

**Interfaces:**
- Consumes: `self.get`（service.rs:763，返回带 `scrollback_b64`）；`crate::ansi::strip_ansi`（已存在）；`BASE64`（service.rs 已导入）。
- Produces:
  - `pub struct TerminalOutputTail { pub text: String, pub truncated: bool, pub status: String }`
  - `pub async fn TerminalService::read_output_tail(&self, id: i64, max_bytes: usize) -> Result<TerminalOutputTail, TerminalError>`

- [ ] **Step 1: 写失败测试**

在 `service.rs` tests 模块加：

```rust
    #[tokio::test]
    async fn read_output_tail_strips_ansi_and_tails() {
        let (svc, _bc) = service();
        let id = svc.create("u", req("cat", &[])).await.unwrap().id;
        svc.submit_text(id, "marker-xyz").await.unwrap();

        // 等回显落入 scrollback。
        let mut ok = false;
        for _ in 0..40 {
            let out = svc.read_output_tail(id, 65536).await.unwrap();
            if out.text.contains("marker-xyz") {
                assert!(!out.truncated);
                assert_eq!(out.status, "running");
                // strip_ansi 已去除裸控制符：不应含 ESC。
                assert!(!out.text.contains('\u{1b}'));
                ok = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        assert!(ok, "tail should contain the echoed marker");

        // 极小上界 → 截断标记为真。
        let tiny = svc.read_output_tail(id, 4).await.unwrap();
        assert!(tiny.truncated);
        assert!(tiny.text.len() <= 4 + 3); // 允许 char-boundary 前移少量
        svc.delete(id).await.ok();
    }
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cargo test -p nomifun-terminal read_output_tail_`
Expected: 编译失败（`read_output_tail` 未定义）。

- [ ] **Step 3: 写最小实现**

在 `service.rs` 顶部合适位置（其它 `pub struct` 附近，或紧邻 method 定义前）加结构体：

```rust
/// ANSI-stripped tail of a terminal's scrollback, for read-back by agents.
#[derive(Debug, Clone)]
pub struct TerminalOutputTail {
    /// Human-readable text (control/escape sequences removed).
    pub text: String,
    /// True when older output was dropped to fit `max_bytes`.
    pub truncated: bool,
    /// The session's status: "running" | "exited" | "error".
    pub status: String,
}
```

在 `impl TerminalService` 块内加方法：

```rust
    /// Read the terminal's scrollback as ANSI-stripped text, keeping at most
    /// `max_bytes` from the TAIL. The terminal analogue of a conversation's
    /// transcript read-back — what an agent uses to SEE a command's result.
    pub async fn read_output_tail(
        &self,
        id: i64,
        max_bytes: usize,
    ) -> Result<TerminalOutputTail, TerminalError> {
        let resp = self.get(id).await?;
        let raw = resp
            .scrollback_b64
            .and_then(|b64| BASE64.decode(b64).ok())
            .unwrap_or_default();
        let text = crate::ansi::strip_ansi(&raw);
        let (tail, truncated) = tail_on_char_boundary(&text, max_bytes);
        Ok(TerminalOutputTail {
            text: tail,
            truncated,
            status: resp.last_status,
        })
    }
```

在 `service.rs` 的自由函数区（文件底部、非 impl 内，紧邻 `default_name` 等 helper）加：

```rust
/// Return the last `max_bytes` of `s` on a UTF-8 char boundary, plus whether it
/// was truncated. Cheap and allocation-light for the common (no-truncation) path.
fn tail_on_char_boundary(s: &str, max_bytes: usize) -> (String, bool) {
    if s.len() <= max_bytes {
        return (s.to_owned(), false);
    }
    let mut start = s.len() - max_bytes;
    while start < s.len() && !s.is_char_boundary(start) {
        start += 1;
    }
    (s[start..].to_owned(), true)
}
```

在 `lib.rs` 的 service re-export 行补 `TerminalOutputTail`：

```rust
pub use service::{TerminalService, TerminalSupervisionHook, TerminalOutputTail};
```

- [ ] **Step 4: 运行测试确认通过**

Run: `cargo test -p nomifun-terminal read_output_tail_`
Expected: PASS。

- [ ] **Step 5: 提交**

```bash
git add crates/backend/nomifun-terminal/src/service.rs crates/backend/nomifun-terminal/src/lib.rs
git commit --author="nomifun <rika00@qq.com>" -m "feat(terminal): read_output_tail（去 ANSI 的 scrollback 尾部读回）"
```

---

### Task 5: 网关能力 `nomi_terminal_send`

**Files:**
- Modify: `crates/backend/nomifun-gateway/src/caps_terminal_ext.rs`（params + handler + register + `nomi_terminal_write_input` 描述补注 + 测试）

**Interfaces:**
- Consumes: `deps.terminal_service.submit_text` / `await_turn_settle` / `read_output_tail`（Task 2/3/4）；`nomifun_terminal::SettleReason`（Serialize）。
- Produces: 已注册的 `nomi_terminal_send` 能力（domain `terminal`, `DangerTier::Write`, `.deny_on(&[Surface::Channel])`）。

- [ ] **Step 1: 写失败测试（参数反序列化）**

在 `caps_terminal_ext.rs` 的 `#[cfg(test)] mod tests` 内（若无则新建，`use super::*; use serde_json::json;`）加：

```rust
    #[test]
    fn submit_params_plain_text_no_base64() {
        let p: SubmitTerminalParams =
            serde_json::from_value(json!({"id": 7, "text": "git status"})).unwrap();
        assert_eq!(p.id, 7);
        assert_eq!(p.text, "git status");
        assert!(!p.wait);
        assert_eq!(p.timeout_secs, None);

        let p2: SubmitTerminalParams = serde_json::from_value(
            json!({"id": 1, "text": "run", "wait": true, "timeout_secs": 60}),
        )
        .unwrap();
        assert!(p2.wait);
        assert_eq!(p2.timeout_secs, Some(60));
    }
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cargo test -p nomifun-gateway submit_params_plain_text_no_base64`
Expected: 编译失败（`SubmitTerminalParams` 未定义）。

- [ ] **Step 3: 写实现**

在 `caps_terminal_ext.rs` 的 Params 区加：

```rust
/// Parameters for submitting text to a terminal so it EXECUTES (the high-level
/// "type it and press Enter" op — no base64, no manual newline).
#[derive(Deserialize, JsonSchema)]
struct SubmitTerminalParams {
    /// The terminal session id (from nomi_list_terminals).
    id: i64,
    /// Plain UTF-8 text/command to type into the terminal and RUN. Do NOT
    /// base64-encode and do NOT append a newline — submission (Enter) is handled
    /// for you, including the bracketed-paste sequence agent CLIs (claude/codex/
    /// gemini) need so the text actually executes instead of sitting unrun.
    text: String,
    /// Wait for the turn to settle and return an output tail. Default false
    /// (fire-and-forget). true → also returns settle_reason + output_tail.
    #[serde(default)]
    wait: bool,
    /// Max seconds to wait when `wait` is true (default 300, capped 1800).
    #[serde(default)]
    timeout_secs: Option<u64>,
}
```

在 Handlers 区加：

```rust
async fn submit_terminal(deps: Arc<GatewayDeps>, ctx: CallerCtx, p: SubmitTerminalParams) -> Value {
    if ctx.user_id.is_empty() {
        return json!({"error": "missing caller user identity (NOMI_GW_MCP_USER_ID)"});
    }
    if let Err(e) = deps.terminal_service.submit_text(p.id, &p.text).await {
        // A not-live session is the common, actionable failure — point at relaunch.
        return json!({
            "error": e.to_string(),
            "hint": "if the session has exited, call nomi_terminal_relaunch first, then retry"
        });
    }
    if !p.wait {
        return ok(json!({"submitted": true, "id": p.id, "note": "text submitted; use nomi_terminal_read_output to see the result"}));
    }
    let secs = p.timeout_secs.unwrap_or(300).min(1800);
    let reason = deps
        .terminal_service
        .await_turn_settle(p.id, std::time::Duration::from_secs(secs))
        .await;
    let tail = deps
        .terminal_service
        .read_output_tail(p.id, 4096)
        .await
        .map(|t| t.text)
        .unwrap_or_default();
    ok(json!({
        "submitted": true,
        "id": p.id,
        "settle_reason": reason,
        "output_tail": tail,
    }))
}
```

在 `register` 函数内 `out.push(...)` 区加（放在 `nomi_terminal_write_input` 之后）：

```rust
    out.push(Capability::new::<SubmitTerminalParams, _, _>(
        CapabilityMeta::new(
            "nomi_terminal_send",
            "terminal",
            "Type text/a command into a terminal and RUN it (plain text, no base64, no manual newline — Enter and the agent-CLI paste sequence are handled). Optional wait=true returns settle_reason + output_tail. Preferred over nomi_terminal_write_input for sending commands.",
            DangerTier::Write,
        )
        .deny_on(&[Surface::Channel]),
        |deps, ctx, p| submit_terminal(deps, ctx, p),
    ));
```

把现有 `nomi_terminal_write_input` 的描述串结尾追加一句（Modify 其 `CapabilityMeta::new` 的 summary）：

```
"...Powerful: can execute arbitrary commands in the running shell. For sending a command/prompt to run, prefer nomi_terminal_send (handles Enter + agent-CLI paste); use this for raw control bytes like Ctrl-C."
```

- [ ] **Step 4: 运行测试确认通过**

Run: `cargo test -p nomifun-gateway submit_params_plain_text_no_base64`
Expected: PASS。

- [ ] **Step 5: 提交**

```bash
git add crates/backend/nomifun-gateway/src/caps_terminal_ext.rs
git commit --author="nomifun <rika00@qq.com>" -m "feat(gateway): nomi_terminal_send 能力（发送并执行，可选等待回执）"
```

---

### Task 6: 网关能力 `nomi_terminal_read_output` + 可见性守卫测试

**Files:**
- Modify: `crates/backend/nomifun-gateway/src/caps_terminal_ext.rs`（params + handler + register + 测试）

**Interfaces:**
- Consumes: `deps.terminal_service.read_output_tail`（Task 4）。
- Produces: 已注册的 `nomi_terminal_read_output`（domain `terminal`, `DangerTier::Read`）。

- [ ] **Step 1: 写失败测试**

在 `caps_terminal_ext.rs` tests 模块加：

```rust
    #[test]
    fn read_output_params_defaults() {
        let p: ReadTerminalOutputParams = serde_json::from_value(json!({"id": 3})).unwrap();
        assert_eq!(p.id, 3);
        assert_eq!(p.max_bytes, None);
    }

    #[test]
    fn send_and_read_are_registered_and_desktop_visible_but_channel_denied() {
        use crate::registry::Registry;
        let reg = Registry::global();
        for name in ["nomi_terminal_send", "nomi_terminal_read_output"] {
            assert!(reg.contains(name), "{name} must be registered");
            assert!(
                reg.tool_visible(crate::registry::Surface::Desktop, name),
                "{name} must be visible to the Desktop companion"
            );
        }
        // send 写类：渠道面必须拒绝。read 只读：渠道面可见（不放大攻击面，仅只读）。
        assert!(
            !reg.tool_visible(crate::registry::Surface::Channel, "nomi_terminal_send"),
            "nomi_terminal_send must be denied on Channel"
        );
    }
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cargo test -p nomifun-gateway read_output_params_defaults`
Expected: 编译失败（`ReadTerminalOutputParams` 未定义）。

- [ ] **Step 3: 写实现**

在 Params 区加：

```rust
/// Parameters for reading a terminal's recent output (ANSI-stripped scrollback tail).
#[derive(Deserialize, JsonSchema)]
struct ReadTerminalOutputParams {
    /// The terminal session id.
    id: i64,
    /// Max bytes of the scrollback TAIL to return after ANSI stripping
    /// (default 16384, capped 65536).
    #[serde(default)]
    max_bytes: Option<usize>,
}
```

在 Handlers 区加：

```rust
async fn read_terminal_output(deps: Arc<GatewayDeps>, ctx: CallerCtx, p: ReadTerminalOutputParams) -> Value {
    if ctx.user_id.is_empty() {
        return json!({"error": "missing caller user identity (NOMI_GW_MCP_USER_ID)"});
    }
    let cap = p.max_bytes.unwrap_or(16_384).min(65_536);
    match deps.terminal_service.read_output_tail(p.id, cap).await {
        Ok(t) => ok(json!({
            "id": p.id,
            "text": t.text,
            "truncated": t.truncated,
            "status": t.status,
        })),
        Err(e) => json!({"error": e.to_string()}),
    }
}
```

在 `register` 区加（放在 `nomi_terminal_send` 之后）：

```rust
    out.push(Capability::new::<ReadTerminalOutputParams, _, _>(
        CapabilityMeta::new(
            "nomi_terminal_read_output",
            "terminal",
            "Read a terminal's recent output (ANSI-stripped scrollback tail) to see a command's result or diagnose. The terminal analogue of nomi_conversation_status.",
            DangerTier::Read,
        ),
        |deps, ctx, p| read_terminal_output(deps, ctx, p),
    ));
```

- [ ] **Step 4: 运行测试确认通过**

Run: `cargo test -p nomifun-gateway -- read_output_params_defaults send_and_read_are_registered`
Expected: 2 个测试 PASS。

- [ ] **Step 5: 提交**

```bash
git add crates/backend/nomifun-gateway/src/caps_terminal_ext.rs
git commit --author="nomifun <rika00@qq.com>" -m "feat(gateway): nomi_terminal_read_output 能力 + 伙伴可见性守卫测试"
```

---

### Task 7: 伙伴系统提示词补终端操作指引

**Files:**
- Modify: `crates/backend/nomifun-companion/src/companion.rs`（`build_companion_system_prompt`，本地分支追加一段 + 测试）

**Interfaces:**
- Consumes: 无新接口（纯提示词字符串）。
- Produces: 本地（非 remote）伙伴提示词包含终端工具指引。

- [ ] **Step 1: 写失败测试**

在 `companion.rs` 的 `#[cfg(test)] mod tests`（或新建）加。注意 `build_companion_system_prompt` 需 `store`/`profile`——参照该文件已有 prompt 测试的构造方式；若无现成 helper，用最小 `CompanionProfileConfig`/`CompanionStore`（内存）。断言核心是「本地含 nomi_terminal_send，远程不含」：

```rust
    #[tokio::test]
    async fn local_prompt_teaches_terminal_tools_remote_does_not() {
        let store = test_store().await;              // 复用本模块既有测试 helper
        let profile = test_profile();                // 同上
        let local = build_companion_system_prompt(&store, &profile, None, false).await;
        assert!(local.contains("nomi_terminal_send"), "local prompt must teach terminal send");
        assert!(local.contains("nomi_terminal_read_output"));

        let remote = build_companion_system_prompt(&store, &profile, Some("wecom"), false).await;
        assert!(!remote.contains("nomi_terminal_send"), "remote (IM) prompt must not teach terminal tools");
    }
```

> 若本模块尚无 `test_store()/test_profile()` helper，则新增最小版本：`CompanionProfileConfig` 用 `Default`/字段直填，`CompanionStore` 用其现有测试构造（跟随本文件既有测试模式；参考 `companion.rs` 中已有对 `store`/`profile` 的测试用例）。

- [ ] **Step 2: 运行测试确认失败**

Run: `cargo test -p nomifun-companion local_prompt_teaches_terminal_tools`
Expected: FAIL（提示词不含 `nomi_terminal_send`）。

- [ ] **Step 3: 写实现**

在 `build_companion_system_prompt` 中、知识库段 `push_str` 之后、`smart_orchestration` 段之前（约 companion.rs:147 后），加本地专属段（复用已有 `remote` 布尔——只在本地注入，因为渠道面 PROFILE_LITE 根本没有 terminal 域）：

```rust
    // 终端操作能力（本地会话；远程 IM 走 PROFILE_LITE，无 terminal 域，不注入）。
    if !remote {
        system.push_str(
            "\n\n## 操作终端会话\n\
             主人电脑上还有「终端会话」(PTY，跑 shell 或 claude/codex/gemini 等 CLI)，你可以直接驱动：\n\
             - nomi_list_terminals 看有哪些终端及状态(running/exited)；nomi_create_terminal 新建(preset: shell|claude|codex|gemini)。\n\
             - nomi_terminal_send(id, text) 把命令或一段话发进去并【直接执行】——不用自己补回车、不用 base64，agent CLI 的粘贴提交也已处理好；\
             要等它跑完并拿结果时带 wait=true(可选 timeout_secs)，会回执 settle_reason 与输出尾巴。\n\
             - nomi_terminal_read_output(id) 读终端最近输出(已去除控制符)，用来查看命令结果或排查。\n\
             - 目标终端已退出(exited)时先 nomi_terminal_relaunch 再发送；kill/delete 这类破坏性操作会要你确认后再做。\n\
             主人在终端页能实时看到你的输入与执行，放心大胆地用。",
        );
    }
```

- [ ] **Step 4: 运行测试确认通过**

Run: `cargo test -p nomifun-companion local_prompt_teaches_terminal_tools`
Expected: PASS。

- [ ] **Step 5: 提交**

```bash
git add crates/backend/nomifun-companion/src/companion.rs
git commit --author="nomifun <rika00@qq.com>" -m "feat(companion): 伙伴提示词补终端操作指引（本地会话）"
```

---

### Task 8: 收编 AutoWork 提交实现到共享原语

**Files:**
- Modify: `crates/backend/nomifun-requirement/src/orchestrator.rs`（`terminal_submit_chunks` 删除、`submit_terminal_prompt` 改调共享编码器、`TERMINAL_SUBMIT_DELAY` 常量迁移引用 + 特征测试）

**Interfaces:**
- Consumes: `nomifun_terminal::{encode_submit_chunks, SubmitChunks, TERMINAL_SUBMIT_DELAY}`（Task 1）。
- Produces: 行为等价（多行需求提示词 → agent TUI → paste + 分离CR + 120ms 延迟）。

- [ ] **Step 1: 写特征测试（钉住新行为）**

在 `orchestrator.rs` 的 tests 模块加（验证共享编码器对 AutoWork 的多行 agent 场景产出 `PasteThenCr`，且 paste 含 bracketed-paste 包裹、cr 为单独 `\r`）：

```rust
    #[test]
    fn autowork_multiline_prompt_uses_paste_then_separate_cr() {
        use nomifun_terminal::{encode_submit_chunks, SubmitChunks};
        let prompt = "requirement #1\ndo the thing\ncall requirement_complete when done";
        match encode_submit_chunks(prompt, true) {
            SubmitChunks::PasteThenCr { paste, cr } => {
                assert!(paste.starts_with(b"\x1b[200~"));
                assert!(paste.ends_with(b"\x1b[201~"));
                assert_eq!(cr, b"\r".to_vec());
            }
            other => panic!("expected PasteThenCr, got {other:?}"),
        }
    }
```

- [ ] **Step 2: 运行测试确认失败/通过基线**

Run: `cargo test -p nomifun-requirement autowork_multiline_prompt_uses_paste`
Expected: 依赖 `nomifun-terminal` 的 re-export 存在则 PASS（本测试即基线）。若 `nomifun-requirement` 未 re-export 到位报编译错，说明 Task 1 的 re-export 未生效——回查 lib.rs。

- [ ] **Step 3: 改调共享原语**

在 `orchestrator.rs`：
1. 删除私有 `fn terminal_submit_chunks(...)`（约 :1045-1051）。
2. 把常量 `const TERMINAL_SUBMIT_DELAY: Duration = Duration::from_millis(120);`（:34）改为引用共享常量：删除本地定义，改在使用处用 `nomifun_terminal::TERMINAL_SUBMIT_DELAY`（或文件顶部 `use nomifun_terminal::TERMINAL_SUBMIT_DELAY;`）。
3. 改写 `submit_terminal_prompt`：

```rust
async fn submit_terminal_prompt(
    driver: &Arc<dyn TerminalDriver>,
    terminal_id: i64,
    prompt: &str,
) -> Result<(), AppError> {
    // AutoWork 只驱动 lifecycle-capable 的 agent CLI（claude/codex），故 is_agent_tui=true。
    match nomifun_terminal::encode_submit_chunks(prompt, true) {
        nomifun_terminal::SubmitChunks::PasteThenCr { paste, cr } => {
            driver.write_input(terminal_id, &paste).await?;
            sleep(nomifun_terminal::TERMINAL_SUBMIT_DELAY).await;
            driver.write_input(terminal_id, &cr).await?;
        }
        nomifun_terminal::SubmitChunks::Single(bytes) => {
            driver.write_input(terminal_id, &bytes).await?;
        }
    }
    Ok(())
}
```

> 保留 `submit_terminal_prompt` 上方解释 paste-burst 的注释（仍准确）。若 `sleep` 的 `use` 因常量迁移变为未用，按编译器提示清理。

- [ ] **Step 4: 运行测试确认通过**

Run: `cargo test -p nomifun-requirement`
Expected: 全绿（含既有 AutoWork 测试）。

- [ ] **Step 5: 提交**

```bash
git add crates/backend/nomifun-requirement/src/orchestrator.rs
git commit --author="nomifun <rika00@qq.com>" -m "refactor(requirement): AutoWork 终端提交改用共享编码器（行为不变）"
```

---

### Task 9: 收编 IDMM 提交实现到共享原语

**Files:**
- Modify: `crates/backend/nomifun-idmm/src/probe.rs`（`encode_terminal_input` 删除、`inject` 改用共享编码器并处理 `PasteThenCr` + 测试）

**Interfaces:**
- Consumes: `nomifun_terminal::{encode_submit_chunks, SubmitChunks, TERMINAL_SUBMIT_DELAY}`；`self.driver.describe`（拿 backend）。
- Produces: 单行注入行为不变（raw+CR 一次写）；多行→agent 注入改为 paste + 分离CR（bug 修复）。

- [ ] **Step 1: 写测试（钉住单行不变 + 多行 agent 走两写）**

在 `probe.rs` tests 模块加：

```rust
    #[test]
    fn idmm_single_line_stays_raw_plus_cr() {
        use nomifun_terminal::{encode_submit_chunks, SubmitChunks};
        // 单行答复（option label / continue）必须 raw+CR、一次写，绝不 bracketed-paste。
        assert_eq!(
            encode_submit_chunks("2) 方案B", false),
            SubmitChunks::Single("2) 方案B\r".as_bytes().to_vec())
        );
        assert_eq!(
            encode_submit_chunks("continue", true),
            SubmitChunks::Single(b"continue\r".to_vec())
        );
    }
```

- [ ] **Step 2: 运行测试确认基线**

Run: `cargo test -p nomifun-idmm idmm_single_line_stays_raw_plus_cr`
Expected: PASS（验证共享编码器满足 IDMM 单行铁律）。

- [ ] **Step 3: 改调共享原语**

在 `probe.rs`：
1. 删除私有 `fn encode_terminal_input(text: &str) -> Vec<u8>`（约 :1139-1151）。
2. 改写 `inject`（:1064-1083）——先解析目标是否 agent TUI（用 describe 的 backend；probe 无 command/args 时退化为按 backend 判定）：

```rust
    async fn inject(&self, action: &WakeAction) -> Result<(), AppError> {
        let text = match action {
            WakeAction::Retry => "continue".to_string(),
            WakeAction::Failover => "continue".to_string(),
            WakeAction::SendText(s) => s.clone(),
            WakeAction::AnswerChoice(s) => s.clone(),
            WakeAction::Wait(_) | WakeAction::Stop(_) | WakeAction::Confirm { .. } => return Ok(()),
        };
        // agent TUI 判定：探测目标 backend（probe 仅持有 backend，非完整 command/args）。
        let is_agent = self
            .driver
            .describe(self.terminal_id)
            .await
            .ok()
            .flatten()
            .and_then(|d| d.backend)
            .map(|b| nomifun_terminal::enhance::resolve_agent_family(&b, &[], Some(&b)).is_some())
            .unwrap_or(false);
        // Track the payload's lines so the CLI's echo of them isn't re-detected.
        self.note_injection(&text);
        match nomifun_terminal::encode_submit_chunks(&text, is_agent) {
            nomifun_terminal::SubmitChunks::Single(bytes) => self
                .driver
                .write_input(self.terminal_id, &bytes)
                .await
                .map_err(|e| AppError::Internal(format!("terminal inject failed: {e}"))),
            nomifun_terminal::SubmitChunks::PasteThenCr { paste, cr } => {
                self.driver
                    .write_input(self.terminal_id, &paste)
                    .await
                    .map_err(|e| AppError::Internal(format!("terminal inject failed: {e}")))?;
                tokio::time::sleep(nomifun_terminal::TERMINAL_SUBMIT_DELAY).await;
                self.driver
                    .write_input(self.terminal_id, &cr)
                    .await
                    .map_err(|e| AppError::Internal(format!("terminal inject failed: {e}")))
            }
        }
    }
```

> `resolve_agent_family` 需从 `nomifun_terminal::enhance` 可达——它已在 `enhance.rs` `pub fn` 且 lib.rs `pub mod enhance`（`pub use enhance::{...resolve_agent_family...}`）已 re-export，故 `nomifun_terminal::resolve_agent_family` 亦可用；二者取其一，编译不过则改用 re-export 路径 `nomifun_terminal::resolve_agent_family`。

- [ ] **Step 4: 运行测试确认通过**

Run: `cargo test -p nomifun-idmm`
Expected: 全绿（含既有 probe 测试；`recent_injections` 回显抑制按 text 行匹配，两次写不影响）。

- [ ] **Step 5: 提交**

```bash
git add crates/backend/nomifun-idmm/src/probe.rs
git commit --author="nomifun <rika00@qq.com>" -m "refactor(idmm): 终端注入改用共享编码器（单行不变，多行→agent 修正为分离CR）"
```

---

### Task 10: 全量验证与收尾

**Files:**
- 无代码改动（验证 + 可选 CHANGELOG/release note）。

- [ ] **Step 1: 后端全量测试**

Run: `cargo test -p nomifun-terminal -p nomifun-gateway -p nomifun-requirement -p nomifun-idmm -p nomifun-companion`
Expected: 全绿。若某 crate 名不符，用 `cargo test --workspace` 兜底（较慢）。

- [ ] **Step 2: 注册表不变量测试**

Run: `cargo test -p nomifun-gateway registry`
Expected: `registry_builds_and_names_fit_mcp_limit`（新工具名 `nomi_terminal_send` 26 字、`nomi_terminal_read_output` 25 字，均 ≤ 42 预算且 ≤ 64 硬顶）、`registry_capability_count_floor`（新增 2 个能力，仍 ≥135）、`all_caps_modules_are_mod_declared_and_registered` 全 PASS。

- [ ] **Step 3: 前端构建验证（无改动，确认未连带破坏类型）**

Run: `cd ui && bun run build`
Expected: 构建成功（前端未改，验证 IPC 类型面未受影响）。

- [ ] **Step 4: 人工验收（手动，勾选记录）**

启动应用，在伙伴聊天里说「在某终端里跑 `git status` 并把结果告诉我」，确认：伙伴 `nomi_list_terminals` → `nomi_terminal_send(wait=true)` → 回读结果，全程无需用户碰终端；shell 与 claude 终端回车均不被吞；exited 终端伙伴会先提示 relaunch。

- [ ] **Step 5: 收尾提交（若有 CHANGELOG/release note）**

```bash
git add -A
git commit --author="nomifun <rika00@qq.com>" -m "chore: 伙伴终端操作闭环 验证收尾"
```

---

## Self-Review

**Spec coverage:**
- §1 共享提交原语 → Task 1（编码器）+ Task 2（submit_text）+ Task 3（await_turn_settle）。✓
- §2 网关能力（send / read_output / write_input 描述）→ Task 5 + Task 6。✓
- §3 伙伴提示词 → Task 7。✓
- §4 收编分歧实现（AutoWork / IDMM；前端不动）→ Task 8 + Task 9。✓
- §5 保持不动（write_input 语义、AutoWork 裁决体系、前端链路、门禁）→ 未改动这些；Task 5 保留 write_input、Task 8/9 行为等价/单行不变。✓
- 验收标准 1-5 → Task 10 Steps 1-4。✓

**Placeholder scan:** 无 TODO/TBD；每个 code step 给出完整代码；测试均含具体断言。Task 7 的 `test_store()/test_profile()` 明确指示复用本模块既有测试构造，并给了退化方案。✓

**Type consistency:** `SubmitChunks`（`Single`/`PasteThenCr{paste,cr}`）、`SettleReason`（`TurnEnd`/`Idle`/`Timeout`/`Exited`）、`TerminalOutputTail{text,truncated,status}`、`encode_submit_chunks(text,is_agent_tui)`、`submit_text(id,text)`、`await_turn_settle(id,timeout)`、`read_output_tail(id,max_bytes)`、`SubmitTerminalParams{id,text,wait,timeout_secs}`、`ReadTerminalOutputParams{id,max_bytes}` 在各 Task 间引用一致。✓

**已知风险与缓解：**
- IDMM 多行→agent 注入行为由「一次写」变为「两次写+120ms」——这是与 AutoWork 一致的修复，单行（IDMM 99% 场景）零变化；Task 9 Step 1 钉住单行不变。
- AutoWork 提交若提示词以换行结尾，共享编码器会 strip 末尾换行（paste 内不再含尾换行）——无害（提交靠分离 CR），Task 8 特征测试接受新字节形态。
