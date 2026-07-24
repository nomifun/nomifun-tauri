# 架构总览

NomiFun 围绕一个核心原则构建：**一份 Rust 后端、两种宿主形态、一份前端**。无论你启动桌面产品 **NomiFun**，还是自托管 Web 服务器，同一个 `axum` HTTP/WS 服务器（`nomifun-app`，二进制 `nomicore`）都在宿主进程中执行。`ui/` 下的 React 19 SPA 是唯一客户端，它始终通过普通的 HTTP 与 WebSocket 通信 —— 没有 Electron preload，也没有 Tauri 自定义协议。

本文档是这张地图的总图。配套文档分别深入介绍各个部分：

- [`backend-crates.md`](backend-crates.zh.md) —— 32 个 `nomifun-*` crate。
- [`agent-engine.md`](agent-engine.zh.md) —— 15 个 `nomi-*` crate（AI 引擎）。
- [`agent-execution.zh.md`](agent-execution.zh.md) —— 统一的持久化 AgentExecution 模型。
- [`frontend.md`](frontend.zh.md) —— React SPA、适配层、路由。
- [`communication.md`](communication.zh.md) —— HTTP / WebSocket / Tauri IPC / ACP / MCP。
- [`data-and-storage.md`](data-and-storage.zh.md) —— SQLite、工作区、运行时。
- [`id-system.md`](id-system.zh.md) —— v3 技术主键、业务 ID、内部技术行与逻辑关联契约。

## 双宿主模型

```
                        ┌─────────────────────────────────────┐
                        │  ui/   React 19 SPA (Vite build)    │
                        │  HashRouter · SWR · Arco · UnoCSS   │
                        │  http://127.0.0.1:<port>/api  +  /ws│
                        └─────────────────────────────────────┘
                                   ▲                 ▲
                          HTTP/REST│        WebSocket│  /ws
                                   │                 │
   ┌───────────────── desktop ─────┴────┐  ┌─────── web ───────┴──────┐
   │ apps/desktop  (nomifun-desktop)    │  │ apps/web  (nomifun-web)  │
   │  Tauri 2 shell · WebView2/WKWebKit │  │ standalone axum server   │
   │  ─ thread "nomifun-backend"        │  │  serves /api  +  /ws     │
   │    └ tokio · nomifun_app embedded  │  │  + ServeDir(ui/dist) SPA │
   │  picks free localhost port,        │  │  port 8787 (default)     │
   │  injects window.__backendPort      │  │  authenticated by default│
   │  injects window.__nomiLocalTrust   │  │  --insecure-no-auth opts │
   │  AuthPolicy::TrustLocalToken       │  │  into no-auth mode       │
   │  Tauri command: check_for_updates  │  │  serves SPA as fallback  │
   └────────────────────────────────────┘  └──────────────────────────┘
                                   │                 │
                                   ▼                 ▼
                        ┌─────────────────────────────────────┐
                        │  nomifun-app  (binary nomicore)     │
                        │  composition root · axum router     │
                        │  bootstrap → data layer → services  │
                        │  /api · /ws · Routes from 32 crates │
                        └─────────────────────────────────────┘
                          │                       │
                          ▼                       ▼
              ┌─────────────────────┐   ┌─────────────────────┐
              │  nomifun-* (32)     │   │  nomi-* (15)         │
              │  backend crates     │◀─▶│  agent engine crates │
              │  data, auth, MCP,   │   │  via the SEAM:       │
              │  conversation, etc. │   │  nomifun-ai-agent     │
              └─────────────────────┘   └─────────────────────┘
                          │
                          ├─▶ SQLite (sqlx)         see data-and-storage.md
                          ├─▶ ACP agent CLIs         see agent-engine.md
                          ├─▶ MCP stdio bridges      see communication.md
                          └─▶ bundled bun runtime    see data-and-storage.md
```

## 一次请求的流转

一个典型的用户消息 ——“向会话 X 中的 Claude agent 发送一条聊天” —— 会穿过图中的每一层。下方追踪过程列出了真实参与的类型与文件。

```
1. UI keypress → React handler
   ui/src/renderer/pages/conversation/...
   calls ipcBridge.conversation.sendMessage.invoke(...)
   (a thin wrapper produced by the adapter factory in ui/src/common/adapter)
2. httpBridge → fetch
   ui/src/common/adapter/httpBridge.ts
   POST http://127.0.0.1:<port>/api/conversations/{id}/messages
   In WebUI mode, the CSRF cookie is echoed into x-csrf-token (double-submit).
3. axum router (composition root)
   crates/backend/nomifun-app/src/router/  — assembled in create_router()
   middlewares: trace, body-limit, CORS, auth, CSRF, rate-limit, response wrapper
4. Conversation service
   crates/backend/nomifun-conversation/src/service.rs
   persists the message, looks up the conversation's bound agent
5. Agent seam
   crates/backend/nomifun-ai-agent  — the only backend crate that sees nomi-*
   AgentRegistry 解析 Agent 类型；AgentRuntimeRegistry 复用该 Conversation 的 runtime
6. Agent turn
   nomi-agent  drives the engine: providers (anthropic/openai/bedrock/vertex),
   tools (bash/read/write/...), MCP servers, skills, plan/confirm/output sinks
   For ACP-protocol agents (Claude Code, Codex, Gemini CLI, ...), the backend
   speaks ACP over stdio to a child process spawned with the bundled runtime
7. Streaming back to the UI
   nomifun-realtime  broadcasts each token as a WS event over /ws
   ui/src/common/adapter/httpBridge.ts ensureWs() routes events to listeners
8. UI renders the streaming reply (react-markdown + KaTeX + mermaid)
```

## 三大 crate 分组

Cargo 工作区（根 [`Cargo.toml`](../../Cargo.toml)，`resolver = "3"`，`edition = "2024"`）按三个文件夹分组，使边界不仅在包名中可见，在磁盘上也可见：

| 目录 | 用途 | Crate 前缀 | 数量 |
| --- | --- | --- | --- |
| `crates/agent/` | AI 引擎 —— providers、tools、sessions、MCP、skills、browser/computer-use | `nomi-*` | 15 |
| `crates/backend/` | HTTP/WS 服务器、数据、认证、各项功能 | `nomifun-*` | 32 |
| `crates/shared/` | 真正跨层共享工具 | mixed | 3 |

agent 分组是**基本自包含的** —— `nomi-*` crate 不引用 `nomifun-*` crate、工作区根目录或 Tauri / sqlx / axum 等后端框架。反向依赖默认通过 `nomifun-ai-agent` 这条接缝汇集，它再导出 `nomi_config`、`nomi_types` 和 `RequirementSink`。当前 `nomifun-app` 与 `nomifun-gateway` 为 browser/computer-use bridge 存在 feature-gated 直接依赖例外；新增例外必须有明确 feature gate 和文档说明。

## 各部分的位置

```
nomifun-tauri/
├─ apps/
│   ├─ desktop/   nomifun-desktop  (Tauri 2 shell, this is "NomiFun" the product)
│   └─ web/       nomifun-web      (standalone server: /api + SPA on one port)
├─ crates/
│   ├─ agent/     15 nomi-*  crates  → see agent-engine.md
│   ├─ backend/   32 nomifun-* crates → see backend-crates.md
│   └─ shared/    3 shared crates
├─ ui/            React 19 + Vite 6 + Arco + UnoCSS  → see frontend.md
└─ docs/
    ├─ architecture/   (this folder)
    └─ specs/          dated engineering design specs
```

## 品牌与标识

- **NomiFun** —— 桌面产品和项目 / 品牌字标（驼峰式书写，N 与 F 大写）。在散文中使用此写法。
- 小写的 `nomifun` 仅保留给技术标识符 —— npm/JS 包 id、Rust crate 前缀 `nomifun-*`、Tauri bundle 标识符 `com.nomifun.desktop`、环境变量 `NOMIFUN_*`，以及仓库 / 目录名。

## 宿主一览

| 维度 | 桌面（`nomifun-desktop`） | Web（`nomifun-web`） |
| --- | --- | --- |
| 二进制 | `nomifun-desktop`（Tauri 外壳） | `nomifun-web`（axum 服务器） |
| 后端 | 进程内嵌入（独立线程 + tokio runtime） | 进程内嵌入 |
| 认证模式 | `TrustLocalToken`：仅信任带本次启动 secret 的 WebView 请求 | 默认要求认证；可通过 `--insecure-no-auth` 关闭 |
| 端口 | 启动时选取的空闲 localhost 端口（`bind 127.0.0.1:0`） | `127.0.0.1:8787`（可通过 `--host`/`--port` 配置） |
| 后端端口如何送达 SPA | 初始化脚本 `window.__backendPort = <p>` | 同源（`/api` 和 `/ws` 与 SPA 在同一端口提供） |
| 静态 SPA | 打包进 Tauri 应用（`tauri.conf.json` 的 distDir） | 由 `tower_http::services::ServeDir` 从 `ui/dist` 提供 |
| 操作系统外壳特性 | 窗口控制、深链接、自动更新、开机启动、对话框、通知、单实例 | 无 —— 浏览器即宿主 |
| Tauri 命令 | 更新检查、WebUI 状态/启停、companion 同步、keep-awake、托盘标签等桌面能力 | 不适用 |

桌面二进制的 `main.rs`（[`apps/desktop/src/main.rs`](../../apps/desktop/src/main.rs)）有意保持精简 —— 大部分逻辑都在 `nomifun_app::run_embedded_server` 中。Web 二进制（[`apps/web/src/main.rs`](../../apps/web/src/main.rs)）复用同样的引导辅助函数（`init_environment`、`init_data_layer`、`AppServices::from_config`、`create_router`），并补充了 SPA 回退以及首次运行管理员预置（`ensure_admin_credentials`）。
