# 终端三项改进 设计文档（自动标题 + 回退 Shell + 退出清理）

> 状态：已与用户确认，进入实现。语言随上下文用中文；代码标识符用英文。

## 目标

1. **自动标题**：新建终端会话的侧边栏标题太固定，改为在用户首次交互后，自动总结为与工作内容相关的标题。
2. **回退 Shell**：claude/codex 终端会话「失去激活态」后出现乱码卡死、无法输入、进程生死不明的情况，改为可靠回退到一个干净的 shell（而非卡死页面），并允许用户持续 Ctrl+C 脱身。
3. **退出清理**：整个 App 退出时清理所有终端会话（删除全部会话行 + 滚屏），不残留到下次启动。

## 现状架构（与改动相关）

- 每个会话 = `portable-pty` 子进程（Windows ConPTY）。`TerminalService.live: DashMap<i64, Arc<PtyHandle>>` 管理活进程，`next_epoch` 区分代际，`on_exit` 回调按 epoch 守卫。子进程退出后 `last_status='exited'`，PTY 被销毁，**无自动重启**。
- `relaunch(id)`（service.rs:837）= kill 旧 PTY → 同一 id、新 epoch 用**原命令**重启 → `clear_scrollback` → `update_status('running')` → `emit_updated`。
- 标题就是 `terminal_sessions.name`。`default_name(command, backend)`（service.rs:989）机械决定：backend 标签（`Claude`/`Codex`）> `Shell`（`$SHELL` 哨兵）> 原始命令。重命名链路 `update_meta → repo.update_meta → emit terminal.updated → 前端 onUpdated → 侧边栏重渲染` **已全链路工作**。
- 终端会话**无 conversation/provider/model 关联**（DB 行、`CreateTerminalParams`、API DTO、IPC、UI 均无相关列）。
- 全部输入经 `TerminalService::input(id, data_b64)`（service.rs:743）汇聚后写入 PTY；`pty.rs::write` 不留记录。`input()` 在无活 handle 时返回 `NotFound`。
- 生命周期事件：`LifecycleKind{TurnEnd, ToolUse, Notification, SessionStart}`（lifecycle.rs）。claude 用 Stop→TurnEnd（payload 含 `last_assistant_message`），codex 同有 TurnEnd。`subscribe_lifecycle(id)` 已存在；spawn_pty 内已有一个示例消费者（service.rs:515-529）。
- 既有「prompt→短文本」LLM 路径：`nomifun_ai_agent::one_shot_completion(cfg, system, messages, max_tokens)` + `resolve_provider_config(provider_repo, encryption_key, provider_id, model, workspace)` + `user_message(text)`。`LiveKnowledgeCompleter`（knowledge_completer.rs）是「持有 provider_repo + encryption_key + workspace、`resolve_default_model()` 取第一个有效 provider/model」的拷贝模板。`provider_repo`/`encryption_key` 在 `AppServices`（services.rs:173/176），`terminal_service` 在 services.rs:440-459 接线，但二者**未**传给 `TerminalService`。
- 前端：`XtermView.tsx` 的 xterm 网格**从不**按 status 禁用（`onData` 始终转发输入）；仅 `TerminalSendBox` 在 `isExited`（`last_status!=='running'`）时 disable（TerminalSessionPage.tsx:408）。`term.clear()` 只清普通缓冲、**不能**退出 alt-screen；需 `term.reset()`。WebGL 上下文丢失（XtermView.tsx:89-95）是「失去激活态后乱码」的诱因之一。
- **WS 重连间隙**：`httpBridge.ts` 的 WS 单例会重连（退避 1s~30s），监听器按事件名存于模块级 map 故重连后存活；但服务端只做无 replay 的 `broadcast_all`，重连 open 回调**不重新拉取 scrollback** → 断线期间的重绘帧永久丢失 = 持久乱码。
- App 生命周期钩子仅在 `apps/desktop/src/main.rs`：`on_window_event`（CloseRequested 隐藏到托盘；Destroyed→exit(0)）、tray-quit（设 `QuitFlag` 后 `app.exit(0)`）、`handle_run_event`（仅 macOS Reopen）。无任何终端清理；`TerminalService`/`PtyHandle` 无 Drop。后端运行在独立 tokio runtime 线程，`DesktopServer` 暴露 runtime Handle（desktop.rs:106）但无阻塞 shutdown 方法。

---

## 功能① 自动标题

**与原始需求的偏差（已确认）**：终端无 conversation 关联，无法「取对应对话的 provider/model」。改为：用 **App 默认有效 provider/model**（首个有效 provider 的首个有效 model，复用 knowledge 自动生成模式）做 LLM 总结；**模型未配置或调用失败 → 兜底取用户输入内容前 N 字**。

### 触发与范围
- **仅一次**，首次交互时填充；**含 shell**。
- **claude/codex**：订阅生命周期，首个 `TurnEnd` 事件读 `payload.last_assistant_message`，结合已捕获的首行用户输入，调 `one_shot_completion` 总结为短标题（≤ ~30 字）。
- **shell / 非 agent / 无模型 / LLM 失败**：取首行用户输入前 N 字（默认 N=40，去除控制字符）作为标题。
- **捕获首行输入**：在 `TerminalService::input()` 内累积该会话首个输入直到首个 `\r`/`\n`，存内存。

### 一次性 + 不覆盖手动改名
- 写入前判定 `当前 name == default_name(command, backend)`；不等说明用户已改名或已自动命名过 → 跳过（自身幂等）。
- 另加内存 `titled: DashMap<i64, ()>` 防止首批按键竞态重复触发。
- **无需 migration**。

### 落库
- 统一走现有 `update_meta(id, Some(title), None)`（trim/校验/持久化/emit `terminal.updated`），**前端零改动**。

### 后端接线
- 新增 `LiveTerminalTitleCompleter { provider_repo, encryption_key, workspace }`（仿 `LiveKnowledgeCompleter`），late-wire 注入 `TerminalService`（新增 `with_title_completer`）。`None` 时只走兜底截断，绝不阻塞。
- `crates/backend/nomifun-terminal/Cargo.toml` 加 `nomifun-ai-agent` 依赖。
- `services.rs:440-459` 旁注入，复用 `provider_repo.clone()` / `encryption_key` / `data_dir.clone()`。

### 主要文件
- `crates/backend/nomifun-terminal/src/{service.rs, title.rs(新), Cargo.toml}`
- `crates/backend/nomifun-app/src/services.rs`

---

## 功能② 回退 Shell（修复乱码卡死 + 持续 Ctrl+C 脱身）

三个独立缺陷，一套协同修复：

1. **后端 `relaunch_as_shell(id)`**：克隆 `relaunch()` 逻辑，但用 `SHELL_SENTINEL`+`[]` 替代 `row.command`/`row.args` 来 spawn。先 tree-kill 卡住的 agent → 同一 id、新 epoch 重启干净登录 shell → 持久化 `command=$SHELL, args=[], backend=None`（这样退出/重启后续仍是 shell，且 `default_name` 变 `Shell`）→ `update_status('running')` → `emit_updated`（前端 `onUpdated` 自动恢复送信框）。
   - 新路由 `POST /api/terminals/{id}/relaunch-shell`（独立于现有 relaunch，语义清晰）。
   - IPC：`ipcBridge.terminal.relaunchShell(id)`。
2. **前端 `term.reset()`**：给 `XtermViewHandle` 增 `reset:()=>term.reset()`（退出 alt-screen、复位模式、清屏）。在「回退 Shell」与重连重放时调用。
3. **常驻「回退 Shell」入口（不受 isExited 限制）**：会话头部加按钮；点击 = `relaunchShell` + `term.reset()`。即便会话仍 `running` 卡死也能脱身。
4. **持续 Ctrl+C 升级**：`XtermView` 检测短时间内连续 N 次 Ctrl+C（`\x03`，默认 3 次 / 1.5s）→ 显示提示条「再次 Ctrl+C 回退到 Shell」并在阈值后触发 `relaunchShell`。Ctrl+C 仍原样转发（不拦截单次）。
5. **WS 重连重放（乱码主因修复）**：`httpBridge.ts` WS open 时若为重连，通知监听者（新增 `terminal.__reconnected` 内部事件或重连回调）；`XtermView` 收到后 `term.reset()` 然后重新 `ipcBridge.terminal.get(id)` 用同一 decoder 重放当前 scrollback。

### 主要文件
- `crates/backend/nomifun-terminal/src/{service.rs, routes.rs}`
- `ui/src/renderer/pages/terminal/{XtermView.tsx, TerminalSessionPage.tsx, TerminalSendBox.tsx}`
- `ui/src/common/adapter/{ipcBridge.ts, httpBridge.ts}`
- i18n：`ui/.../locales/{zh-CN,en-US}/terminal.json`

---

## 功能③ 退出清理（删除全部会话行 + 滚屏）

1. **repo**：`ITerminalRepository` 加 `delete_all(&self) -> Result<u64>`；SQLite 实现 `DELETE FROM terminal_sessions`（`terminal_scrollback` 经 FK CASCADE 自动删）。MemRepo 同步实现（供测试）。
2. **service**：`TerminalService::shutdown_cleanup()`：遍历 `self.live` 逐个 `kill()`，清 `pending_spawn`，再 `repo.delete_all()`。
3. **desktop**：`DesktopServer::shutdown_terminals()`：把 `shutdown_cleanup()` 编排到后端 runtime 上**阻塞执行（带超时上限，如 3s）**，供 Tauri 主线程在 `app.exit(0)` 前同步调用。
4. **main.rs**：仅在 **real-quit 路径**调用：
   - tray-quit handler（`QuitFlag` 已置位，main.rs:697-702）`app.exit(0)` 之前；
   - 新增 `RunEvent::ExitRequested`/`Exit` 分支（`handle_run_event`，仅当 `QuitFlag` 置位时）。
   - **绝不在 close-to-tray（隐藏窗口）路径调用**。

### 主要文件
- `crates/backend/nomifun-terminal/src/service.rs`
- `crates/backend/nomifun-db/src/repository/{terminal.rs, sqlite_terminal.rs}`
- `crates/backend/nomifun-app/src/desktop.rs`
- `apps/desktop/src/main.rs`

---

## 测试策略

- **①** `default_name` 一致性守卫的幂等（改名后不再触发）；无 completer 注入时的截断兜底；注入 fake completer 时 TurnEnd→update_meta 链路；首行输入捕获。
- **②** `relaunch_as_shell` 单测：同一 id、`command` 变为 `$SHELL`、status 回 `running`、发 `terminal.updated`。前端 reset / 重连重放 / 连续 Ctrl+C 升级以手动 + 单元（纯函数）验证。
- **③** `delete_all` 删行 + scrollback（CASCADE）；`shutdown_cleanup` kill 活进程后行清空；close-to-tray 不触发（QuitFlag 守卫，逻辑断言）。

## 风险与缓解

- **退出清理的破坏性**：用户已确认要删全部；严格只在 QuitFlag 真退出路径执行，close-to-tray 绝不删。阻塞清理带 3s 超时上限，避免退出卡住。
- **回退 shell 改变语义**：原 agent 退出→可重启项；改 shell 后命令被改写为 `$SHELL`。这是用户明确要的「回退到 shell」，且仅在用户主动点按/连续 Ctrl+C 触发，不做静默自动改写。
- **标题 LLM 成本/噪声**：仅首次、仅一次、`name==default` 守卫；agent 才调 LLM，shell 走零成本截断。
- **Windows 进程组**：`kill()` 在 Windows 无进程组 SIGKILL，孙进程可能残留——沿用既有 `kill()` 能力，不在本次扩大范围。
