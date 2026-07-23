# 后端 Crates

[`crates/backend/`](../../crates/backend/) 下的 32 个 `nomifun-*` crate 共同构成 HTTP/WS 服务器。它们一起编译进 `nomifun-app` 库 crate，并通过 `nomifun-app/src/main.rs` 生成 **`nomicore`** 二进制。两个宿主应用（`nomifun-desktop` 与 `nomifun-web`）直接链接 `nomifun-app`，并自行调用 `run_embedded_server` 或组合 `create_router`。

下方分组反映了 crate 在工作区清单（[`Cargo.toml`](../../Cargo.toml)）中相互依赖的方式。这并非严格的分层 DAG —— 部分功能 crate 之间存在依赖 —— 但它提供了一张与请求穿越服务器的路径相吻合的认知地图。

## Agent 层依赖规则

正常的产品接缝是 [`nomifun-ai-agent`](../../crates/backend/nomifun-ai-agent/)。需要 agent 概念的功能 crate 应尽量通过 `nomifun_ai_agent::{nomi_config, nomi_types, RequirementSink}` 来消费它们。

存在有意为之、由 feature 控制的直接依赖例外：

- [`nomifun-app`](../../crates/backend/nomifun-app/) 为 `mcp-computer-stdio` 与 `mcp-browser-stdio` 桥接子命令，可选依赖 `nomi-computer`、`nomi-browser`、`nomi-config`、`nomi-tools`、`nomi-types`。
- [`nomifun-gateway`](../../crates/backend/nomifun-gateway/) 为平台 Gateway 的 browser/computer 注册表，可选依赖 `nomi-browser`、`nomi-computer`、`nomi-config`、`nomi-tools`、`nomi-types`。

不要在未说明“为何无法走正常接缝或上述桥接面”的情况下，新增其他直接的 `nomi-*` 依赖。

## 核心、数据、实时、运行时

### v3 数据与 ID 契约

所有后端 crate 遵循统一的 v3 数据契约：

- 每张 NomiFun 产品表都有 `id INTEGER PRIMARY KEY AUTOINCREMENT`；
- 需要跨数据集稳定识别的实体使用 `user_id`、`conversation_id`、
  `message_id` 等具名裸标准 UUIDv7；
- 纯内部行只把表 `id` 作为 repository 实现细节，关系通过 owner UUIDv7、
  sequence、自然键或复合条件表达；
- 一条关系只保留一个引用字段，不存在 `*_row_id` 双轨字段；
- Repository/Service 维护带索引的逻辑关联；产品 DDL 不包含物理
  `FOREIGN KEY`、`REFERENCES`、`ON DELETE` 或 `ON UPDATE`；
- v3 启动时发现不兼容的受管数据集，会整体 reset/quarantine，而不是迁移
  历史行。

技术 `id` 只属于当前数据集，不得当作 API 或跨表身份。稳定 UUIDv7 在边界上
是字符串，外部协议标识保持不透明。

| Crate | 职责 |
| --- | --- |
| [`nomifun-common`](../../crates/backend/nomifun-common/) | `AppError`、错误链、各类枚举（`AgentType`、`ConversationStatus`、`MessageType`、`McpServerStatus` 等）、稳定业务 ID 的裸 UUIDv7 生成/校验、数据集 reset 辅助、AES-GCM `encrypt_string` / `decrypt_string`、`TimestampMs`、分页辅助、`constants::DEFAULT_HOST/DEFAULT_PORT/BODY_LIMIT/CSRF_*`。 |
| [`nomifun-api-types`](../../crates/backend/nomifun-api-types/) | 每个 HTTP 请求 / 响应 DTO，`WebSocketMessage` 信封，ACP / Nomi / OpenClaw / Remote 等扩展。前端 TypeScript 类型镜像该 crate。 |
| [`nomifun-db`](../../crates/backend/nomifun-db/) | 通过 `sqlx` 操作 v3 SQLite baseline，维护 schema contract 与逻辑关联 registry，并为用户、会话、MCP、需求、cron、ACP 会话、设定、终端会话、伙伴令牌、知识库、渠道、连接器凭据、IDMM 介入、远程 agent、webhook 等提供仓储 trait 与 Sqlite 实现。持有 `Database` 句柄并负责 v3 baseline 初始化。 |
| [`nomifun-realtime`](../../crates/backend/nomifun-realtime/) | `WebSocketManager`、`BroadcastEventBus`，带 token 校验的 `/ws` 升级处理器，消息路由 trait，心跳计时，每连接缓冲常量。 |
| [`nomifun-runtime`](../../crates/backend/nomifun-runtime/) | 内嵌 Bun 的解压、缓存、命令发现与启动期 `PATH` 增强。子进程所有权统一属于 shared 层的 `nomi-process-runtime`。 |
| [`nomifun-assets`](../../crates/backend/nomifun-assets/) | 随服务器一同发布的内嵌静态资源（`include_dir!`）。 |

## 认证与会话

| Crate | 职责 |
| --- | --- |
| [`nomifun-auth`](../../crates/backend/nomifun-auth/) | JWT HS256（`JwtService`）、bcrypt 密码哈希、登录 / 登出 / 刷新 / 修改密码 / 初始化路由、`auth_middleware`、**CSRF 双提交 cookie** 中间件（cookie `nomifun-csrf-token`、header `x-csrf-token`）、安全响应头中间件、**限流**（auth / api / authenticated-action 等变体）、二维码登录 token 存储、`validate_username` / `validate_password`。为 handler 暴露 `CurrentUser`。 |

## Agent 接缝

| Crate | 职责 |
| --- | --- |
| [`nomifun-ai-agent`](../../crates/backend/nomifun-ai-agent/) | **通往 `crates/agent/` 的唯一桥梁。** 构建 Agent runtime 工厂（ACP / Nomi / OpenClaw / Nanobot / Remote 等变体），由 `AgentRuntimeRegistry` 按 Conversation 缓存唯一的进程内 runtime handle，持久化 ACP 会话，广播 `AgentStreamEvent`，暴露 `agent_routes`（模型信息、能力、斜杠命令等）和 `remote_agent_routes`。再导出 `nomi_config`、`nomi_types` 和 `RequirementSink` 供其余后端使用。 |

## 功能 crate（产品的主体）

| Crate | 职责 |
| --- | --- |
| [`nomifun-conversation`](../../crates/backend/nomifun-conversation/) | 会话与消息 CRUD、send-message 路由、**流式中继**（将后端 agent token 投递到 `/ws`）、ACP 错误恢复、响应中间件（如 `/cron` 斜杠命令检测、`<think>` 剥离）、技能解析 / 快照、运行时状态持久化。 |
| [`nomifun-agent-execution`](../../crates/backend/nomifun-agent-execution/) | 持久化 Agent 协作：`AgentExecutionEngine` 门面统一负责规划、依赖调度、Attempt、恢复、决策、事件和显式 Conversation 关联；单 Agent 与多 Agent 共用同一聚合。详见[统一执行架构](agent-execution.zh.md)。 |
| [`nomifun-mcp`](../../crates/backend/nomifun-mcp/) | MCP 服务器 CRUD、**OAuth 流程**、多 CLI 同步（`adapters/` 下的 `Claude`、`Codex`、`CodeBuddy`、`Gemini`、`Qwen`、`OpenCode`、`Nomi`、`Nomifun` 适配器）、连接测试、向会话注入 MCP 能力（含内置图像生成）。 |
| [`nomifun-extension`](../../crates/backend/nomifun-extension/) | 扩展与技能枢纽：清单、依赖图、分类器、安装 / 启用 / 禁用，捆绑技能 + MCP 服务器 + 设定的扩展包。 |
| [`nomifun-channel`](../../crates/backend/nomifun-channel/) | 外部聊天渠道适配器（Telegram、Lark、DingTalk、WeChat）——通过 feature 控制。将入站消息映射到共享的 Agent / Conversation runtime，解析按机器人或平台配置的伙伴归属，并应用渠道 Agent 上下文。它是接入边界，不是额外的 Agent 类型或模式。 |
| [`nomifun-gateway`](../../crates/backend/nomifun-gateway/) | **平台 Gateway MCP** —— `nomi_*` 工具（会话、定时任务、伙伴记忆、需求平台，以及 feature 控制的 browser/computer 工具）的进程内能力注册表与传输层。内部子进程经 `nomicore mcp-gateway-stdio` 接入，只接收服务端派生、带作用域、有效期和签名的能力声明；Conversation 或 build-extra 字段都不能授权。公开入口只投影其鉴权边界允许的能力子集。 |
| [`nomifun-cron`](../../crates/backend/nomifun-cron/) | 定时任务：cron 表达式、时区修复、cron 守护进程、由斜杠命令驱动的创建。 |
| [`nomifun-requirement`](../../crates/backend/nomifun-requirement/) | **AutoWork 持久执行器** —— 后端驱动、支持 boot-resume 的持久循环。通过 `RequirementSink` 与 Agent 层通信。 |
| [`nomifun-idmm`](../../crates/backend/nomifun-idmm/) | 智能决策模式（IDMM）：一个按会话的监督器，在提供商故障与决策停滞中保活智能体 / 终端会话（规则层 + 旁路模型）。详见[智能决策](../guides/intelligent-decision.zh.md)。 |
| [`nomifun-webhook`](../../crates/backend/nomifun-webhook/) | 外发飞书消息发送器，以及 Agent 工作完成时的 `CompletionNotifier`。 |
| [`nomifun-preset`](../../crates/backend/nomifun-preset/) | 面向 Conversation、Execution 参与者、伙伴和定时任务的可复用启动配置：合并 builtin/user/extension 目录、关系化 CRUD、按目标解析、不可变快照与导入。 |
| [`nomifun-companion`](../../crates/backend/nomifun-companion/) | 桌面伙伴状态、形象 / 图片资源、记忆 / 人格数据、伙伴公开图片服务，以及伙伴绑定令牌集成。 |
| [`nomifun-knowledge`](../../crates/backend/nomifun-knowledge/) | 知识库、来源摄取、绑定库挂载状态，以及作用域只读的知识 MCP 服务器。 |
| [`nomifun-public`](../../crates/backend/nomifun-public/) | 由伙伴令牌鉴权的公开对外入口：`/mcp`、`/mcp-agent` 与 `/v1`。 |
| [`nomifun-secret`](../../crates/backend/nomifun-secret/) | 按伙伴的 browser-use 密钥存储与凭据查询。 |

## 基础设施特性

| Crate | 职责 |
| --- | --- |
| [`nomifun-terminal`](../../crates/backend/nomifun-terminal/) | 基于 `portable-pty` 的终端会话，支持 resize，通过 WS 进行输入 / 输出流式传输。 |
| [`nomifun-shell`](../../crates/backend/nomifun-shell/) | 操作系统外壳辅助：用系统应用打开文件，针对 Deepgram 或 OpenAI 的语音转文字，剪贴板 / 粘贴集成。 |
| [`nomifun-file`](../../crates/backend/nomifun-file/) | 在会话工作目录下的沙箱化文件系统（`browse`、`path_safety`、`watch_service`、`snapshot_service`），zip 辅助。 |
| [`nomifun-office`](../../crates/backend/nomifun-office/) | LibreOffice 转换 / 预览管线（Office 文档 → 预览）。 |
| [`nomifun-system`](../../crates/backend/nomifun-system/) | LLM provider / 模型查询、应用级设置、sysinfo、应用版本检查 / 自更新框架。 |

## 组合根：`nomifun-app`

[`nomifun-app`](../../crates/backend/nomifun-app/) 是两个宿主二进制所链接的 crate。其结构如下：

| 模块 | 角色 |
| --- | --- |
| `cli.rs` | 顶层 `nomicore` clap 解析器：`--host/--port/--data-dir/--work-dir/--app-version/--local/--log-dir/--log-level`，加上子命令 `mcp-requirement-stdio`、`mcp-knowledge-stdio`、`mcp-gateway-stdio`、`mcp-open-stdio`、`mcp-computer-stdio`、`mcp-browser-stdio`、`terminal-hook`、`doctor`、`tools`、`call`、`backup`、`restore`。Web 宿主调用 `Cli::parse_from(["nomifun-web"])` 取得带默认值的实例，然后覆盖自身关心的项。 |
| `bootstrap/` | 分层初始化：`tracing_init`（文件 + 控制台层）、`work_dir` 解析、`builtin_skills` 物化、`environment::{init_environment,init_data_layer}`、`admin::ensure_admin_credentials`（认证模式下的首次运行预置）。 |
| `services.rs` | `AppServices` 大杂烩：每个功能 crate 的服务带着对应仓储一并接好。通过 `AppServices::from_config(database, &config)` 一次构建。 |
| `router/` | `create_router(&services)` 以及类型化的 `routes`、`state`、`health`、`trace` 辅助；`build_preset_state` / `build_conversation_state` / `build_extension_states` / `build_module_states` / `build_ws_state`。 |
| `commands/` | CLI 子命令的实现体：服务器、各 stdio MCP bridge、终端生命周期 hook、诊断，以及公开能力客户端命令。 |
| `lib.rs` | 公共门面：`run_embedded_server`、`AppServices`、`create_router`、`bootstrap` 再导出。这是宿主二进制唯一引入的 API。 |

## 在哪里检查依赖规则

如果你想自行检查直接的 `nomi-*` 依赖，可以扫描每个后端 crate 的清单：

```sh
# from the repo root, on a Unix shell
rg -l 'nomi-[a-z-]+\s*=' crates/backend/*/Cargo.toml
```

预期会看到主接缝（`nomifun-ai-agent`）以及上文描述的、由 feature 控制的桥接例外。
