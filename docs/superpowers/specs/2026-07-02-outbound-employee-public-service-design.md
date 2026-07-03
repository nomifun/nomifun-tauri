# 外呼员工：桌面伙伴对外公共服务的安全隔离设计

> 状态：设计待评审。作者：架构（Opus 4.8）。日期：2026-07-02。
> 前置事实基线：8 路并行只读审计（`companion-external-service-security-audit`）+ 对 `capability.rs` / `factory/nomi.rs` / `agent_build_extra.rs` / `caps_knowledge.rs` 的一手核对。
> 关联（姿态相反，取其管线弃其取舍）：`2026-06-22-external-capability-exposure-design.md`、`2026-06-23-companion-bound-external-access-and-robot-design.md`（二者均为"全有或全无信任、安全次要"，只适合**对外自用**，不适合本设计的**对外公共服务**）。

## 0. 目标与用户定调

桌面伙伴很受欢迎，但要可靠地**对外服务陌生人**，现有权限模型不成立。区分两种"对外"：

1. **对外自用**：owner 或其信任的 agent 远程驱动自己的伙伴，期望**全能力对标本地**。→ 已由 Remote 前门（`nomifun-public` `/mcp` `/v1`）+ per-companion 令牌覆盖，**本设计不动**。
2. **对外公共服务**：把伙伴发布到社交渠道当"客服/员工"服务**陌生人**，只能问答、知识库检索总结、记忆总结、安全网搜；**绝不能**控制 owner 电脑、跑任意代码、读写 owner 文件、外泄私有知识/记忆、脚本注入、越权。→ **本设计的全部内容**。

**用户已定（2026-07-02）**：
- **首期范围 = 仅安全问答档**（chat + 知识库检索总结 + 记忆总结 + 安全网搜）；沙箱编程留到独立后期。
- **产品形态 = 独立"外呼员工"顶级 Tab**。
- **隔离单元 = 专门的"对外伙伴"**（独立记忆、只绑公开知识库、独立工作区），绝不复用私人伙伴的会话/记忆。

工程价值序：**安全正确（不可为解决问题牺牲用户安全）＞ 架构优秀不埋坑 ＞ 用户体验**。可接受大面积重构。

## 1. 现状安全评估（为什么不能打补丁）

> 今天唯一拦住灾难的是**配对审批门**：`PairingService::get_internal_user_id`（`nomifun-channel/src/action.rs:133`）要求陌生发送者由 owner 在设置里逐个手动批准，代码中**无自动放行模式**。本需求要打开这道门——门一开，下列缺陷全部变为现实。

### 1.1 陌生人经渠道的真实调用链（放行后）
- 陌生人原文以 **owner 身份** `owner_user_id` 执行（`nomifun-channel/src/message_service.rs:236`），非匿名；
- 汇入伙伴的**一个共享会话** `ensure_companion_session`（`message_service.rs:186`）——与桌面气泡/聊天 Tab 同一条 transcript 与记忆，所有陌生人共用；
- `session_mode` 硬设 `yolo`（`message_service.rs:585`），全自动批准 file/shell/exec/mcp，无审批 UI；
- 拿到**全套原生工具**：Bash/ExecCommand、Read/Write/Edit/ApplyPatch、`computer_use`（桌面版默认开）、`browser_use`（默认开）；
- 拿到**原生** `RecallMemories` / `KnowledgeSearch` / `KnowledgeRead`（读私有记忆与全部绑定库）。

### 1.2 结构性缺陷（一手核对，含 file:line）

| # | 缺陷 | 证据 | 后果 |
|---|---|---|---|
| C1 | Surface 矩阵**只管网关 `nomi_*`，完全不管原生工具** | `nomifun-gateway/src/registry/capability.rs:108-137`，仅在 `registry/mod.rs:164` 网关 dispatch 生效 | Bash/文件/computer/browser 对陌生人**无 surface 门** |
| C2 | 原生工具唯一白名单 `retain_named` 对伙伴/渠道会话**恒空=不限制** | `nomi-agent/src/bootstrap.rs:767` + `api-types/src/agent_build_extra.rs:330`（"普通会话恒空"） | 白名单机制**存在但未被驱动** |
| C3 | `KnowledgeSearch/RecallMemories/SaveMemory` 在 `bootstrap.build()` 之后直接注册到 `engine.registry_mut()` | `nomifun-ai-agent/src/manager/nomi/agent.rs:371` | 它们**连 `retain_named` 都绕过**——任何原生白名单方案的**必修前置 bug** |
| C4 | 网关 `nomi_knowledge_list_bases/search` **全局**（列 owner 全部库），Read=所有 surface 放行 | `nomifun-gateway/src/caps_knowledge.rs:135,211` | 网关层**无数据隔离** |
| C5 | Remote 面 `nomi_agent_run`（Write=放行）内部 `create()` 直建 `yolo + desktopGateway` 的 **Desktop-surface** 全原生工具 agent | `nomifun-gateway/src/caps_conversation.rs:426-503,680` | **一个放行 Write 击穿整个 Remote 矩阵** |
| C6 | 所有 companion 令牌 → `SYSTEM_USER_ID`；`companion_id` 仅"归属标注非访问范围" | `nomifun-gateway/src/deps.rs:128` | **无 per-caller 数据分区** |
| C7 | 网关档白名单仅在 stdio bridge（广告层）过滤，权威 in-process server 只按 surface×danger 复核 | `nomifun-app/src/commands/gateway_stdio.rs` vs `nomifun-gateway/src/registry/mod.rs:164` | 档位是"广告式防御"，非硬边界 |

**判断**：现状安全边界是"提示词复述确认 + yolo"，对陌生人**等于零**。必须换成**"工具与数据物理不可达"**——假设提示注入 100% 控制 agent 意图，它仍无害。这是新增一等抽象，不是加开关。

## 2. 架构决策：ExposureProfile（对外服务档）正交一等抽象

**核心洞察**：现有 `Surface{Desktop,Channel,Remote}` 把**"从哪来"（传输）**误当成**"信不信任"（trust）**。同一条渠道可能是 owner 私人 Telegram（可信）也可能是对外客服微信（不可信）。**信任必须独立成正交轴。**

新增 `ExposureProfile`，挂在**对外伙伴 / 令牌 / 渠道绑定**上，由入口盖章进 `CallerCtx`，全链路强制：

```rust
enum ExposureMode { Private, TrustedRemote, PublicService }

struct ExposurePolicy {
    mode: ExposureMode,
    allowed_native_tools: Vec<ToolName>,   // 驱动 retain_named（默认拒绝白名单）
    allowed_gateway_caps: Vec<CapName>,    // 显式能力白名单（非按 tier 放行）
    public_kb_ids:        Vec<KbId>,       // 检索只能碰这些
    memory_access:        MemoryAccess,    // None | ReadOwnPersona
    web_search:           bool,            // 新的安全网搜
    coding_sandbox:       Option<SandboxRef>, // 第二期，首期恒 None
}
```

### 2.1 六个强制点（全部 execution-time、后端权威，不信客户端）

1. **原生工具**：`PublicService` 把 `NomiBuildExtra.allowed_tools` 设为安全集 → `retain_named`（`bootstrap.rs:767`）。**必修 C3**：把 build-后注册的 knowledge/memory/companion 工具改为经同一白名单过滤（或改为按 policy 条件注册）。
2. **网关**：新增 `PublicService` 的**默认拒绝白名单模式**（不再"按 tier 放行所有 Write"），并在**权威 in-process server dispatch** 处强制（消除 C7 的广告/权威不一致），非仅 bridge 广告层。
3. **数据隔离**：`KnowledgeSearch` 复用**已存在的"kb_ids 烘进环境、模型不能加宽"模式**（`NOMI_KB_MCP_KB_IDS`，ACP 已在用，见 `agent_build_extra.rs:77-85`）限定到 `public_kb_ids`；记忆按 `memory_access` 只读且限对外人格自身库。
4. **身份/资产隔离**：对外服务用**专门的对外伙伴**为隔离单元——独立记忆库、只绑公开库、独立工作区/数据目录，**永不复用私人伙伴会话**（详见 §3）。
5. **入口盖章**：新增"公开"配对/令牌模式把 `ExposurePolicy` 盖进 turn；`desktopGateway` 继续后端专设 + HTTP 剥离（防自授权，已有 `conversation/routes.rs:56`）。
6. **不依赖 yolo/提示词**：`PublicService` 下无危险工具，"有无审批"不再是安全依赖——边界在**工具集与数据可达性本身**。

**红线原则**：安全 = 陌生人的 agent 物理够不到危险工具与私有数据；提示词、审批都不计入安全边界。

## 3. 对外伙伴 = 隔离单元

复用现有设计已确立的"**伙伴 = 唯一隔离单元**"。对外服务创建/指定一个专门的对外伙伴：

- **独立记忆**：独立 `CompanionStore` 命名空间，`recall` 只回该对外人格自身记忆，禁 `SaveMemory`（防记忆投毒/存储型注入）。
- **只绑公开知识库**：`public_kb_ids` 是 owner 显式勾选的公开子集；检索/读取被烘环境限死，模型不能加宽。
- **独立工作区/数据目录**：与私人数据物理分离（首期无 shell/文件工具，此项为纵深防御 + 为 §5 沙箱预留）。
- **渠道/令牌绑定该对外伙伴**：入口即确定 `ExposurePolicy=PublicService`。

## 4. 能力白名单：安全问答档

### 4.1 原生工具（`bootstrap.rs` 注册，首期 `PublicService` 白名单）
- ✅ 允许：纯对话（无工具）、`KnowledgeSearch`/`KnowledgeRead`（限 `public_kb_ids`）、记忆总结（只读、限对外人格）、**新的安全网搜**（§4.3）。
- ❌ 禁：Bash/ExecCommand/WriteStdin、Write/Edit/ApplyPatch、Read/Grep/Glob（无沙箱前一律禁）、Computer、Browser、Spawn、Skill、Plan、SaveMemory。

### 4.2 网关 `nomi_*`（默认拒绝，仅显式放行）
- ✅ 至多：收窄版 `nomi_knowledge_search`（限公开库）、极少数只读 conversation。
- ❌ 禁：`system`/`channel`/`companion`/`cron`/`requirement`/`memory写`/`terminal`/`files`/`agent`/`orchestrator`/`mcp` 全域。
- 注意：现渠道已强制 `LITE` 档（`team_mcp.rs:199`，含 conversation/provider/cron/requirement/autowork）——**LITE 仍不够安全**（含 provider/cron/requirement），`PublicService` 需比 LITE 更严的独立白名单。

### 4.3 安全网搜（新建，替代收费 web_search）
现状**无原生 web_search**（`bootstrap.rs` 无此工具；网访仅靠 Browser / Bash-curl / 网关）。首期新建一个**受限安全 fetch/search 工具**：
- 出口防火墙（禁 localhost/内网/`file://`/元数据端点，防 SSRF），沿用 browser 的 `allowed_origins`/egress 思路；
- 无凭据注入、限速、结果脱敏；仅此工具，不暴露完整 Browser 引擎。

## 5. 沙箱编程（明确后期，独立安全域）

首期**不发**。要对陌生人安全开放编程需**真正 OS 级隔离**（当前工作区仅普通目录 `companion.rs:214`，可 `../` 逃逸；Bash 仅 macOS Seatbelt 且默认关，**Windows 完全无约束**；无 container/chroot/namespace）。后期作为 **P3 独立安全域**：独立低权用户/容器（Win 走 WSL2 或容器）+ 资源限额 + 出口管控 + 独立凭据 + 独立可弃工作区。

## 6. "外呼员工" Tab（产品形态）

新增独立顶级 Tab：
- 列出所有对外服务伙伴（"员工"）、其档位、绑定的渠道/令牌、可见的公开知识库、开关（网搜等）；
- 一键从既有伙伴派生一个对外人格（自动置 `PublicService` + 独立记忆 + 选公开库）；
- **审计日志**：每次对外调用（谁/工具/是否放行）append-only 落库，白送可观测性；
- 心智：把对外服务当"员工"独立管理，与私人伙伴强隔离。底层仍是"伙伴挂 `ExposurePolicy`"。

## 7. 复用 / 必新建 / 必修

- **直接复用**：`retain_named`/`allowed_tools`（`registry.rs:73`、`agent_build_extra.rs:330`）、`tool_specs_for`/域档（`registry/mod.rs:136`）、烘环境 scoped KB bridge（`NOMI_KB_MCP_KB_IDS`）、companion 令牌（`nomifun-auth/companion_token.rs`）、per-surface 写策略（`factory/nomi.rs:95-123`）、配对门、`desktopGateway` 剥离。
- **必新建**：`ExposurePolicy` 抽象与全链路穿线、网关默认拒绝白名单模式（权威层强制）、由对外伙伴驱动原生白名单、安全网搜工具、对外隔离伙伴 + 独立记忆/数据目录、"外呼员工" Tab + 审计。
- **必修 bug**：C3（build-后工具绕过白名单）、C7（网关档权威/广告不一致）。
- **旧分支**：`feat/per-companion-capabilities` / `feat/external-capability-exposure` 取其令牌/管线，**换掉"全有或全无"取舍**，不原样合并。

## 8. 分期

- **P0 抽象与强制点**：`ExposurePolicy` 类型 + `CallerCtx` 盖章 + 六强制点接线 + 必修 C3/C7；单测覆盖"白名单外工具不可达""跨库检索被拒""记忆越权被拒"。
- **P1 安全问答档**：对外伙伴隔离单元 + native/gateway 白名单 + KB 烘环境收窄 + 记忆只读；渠道公开模式入口盖章。
- **P2 安全网搜**：出口防火墙 fetch 工具 + 白名单接入。
- **P3 沙箱编程**（独立安全域，后期）。
- **贯穿**："外呼员工" Tab + 审计日志随 P1 起步。

## 9. 风险与护栏

- **提示注入不可依赖提示词防御**：安全性能否成立**只看** agent 物理可达的工具集与数据集；每加一个对外能力都须过"若被完全操纵是否仍无害"。
- **权威点唯一**：网关白名单必须在 in-process server dispatch 强制（非仅 bridge），native 必须经 `retain_named`（且修 C3）。任何绕过路径都要有回归测试。
- **默认拒绝**：`ExposurePolicy` 缺省即最严；新增能力默认不进对外白名单（与历史"覆盖靠纪律会退化"相反的结构性缺省）。
- **数据烘死不可加宽**：`public_kb_ids`、memory scope 一律服务端烘进环境，模型入参不可扩大范围。
- **令牌**：对外令牌可吊销/轮换、绝不入日志、绑对外伙伴、限速。
- **诚实降级**：无沙箱前，任何文件/shell/computer/browser/编程能力在对外档一律不可见。

## 10. 明确不做（本轮）

- 不做对外沙箱编程（P3 独立立项）。
- 不做多租户（仍单实例·多对外伙伴隔离）。
- 不放开任何写域给陌生人（KB 写、记忆写、companion/system/channel/cron/requirement 写全禁）。
- 不依赖 yolo/提示词作为对外安全边界。
