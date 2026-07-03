# 对外伙伴（Public Companion）独立领域设计

> 状态：设计已对齐（用户拍板），实施中。作者：架构（Opus 4.8）。日期：2026-07-03。
> 取代：`2026-07-02-outbound-employee-public-service-design.md` 的**管理层**部分（"companion 挂 exposure 标记 + 复用 companion 管理"被推翻）。**保留**其运行时安全内核（exposure 钳制 / C3 / KB 烘死 / 首轮语言）。

## 0. 用户定调（关键转向）

「对外伙伴」**不是"桌面伙伴 + 一个开关"**，而是一个**独立的一等公民领域**：独立数据、独立配置、独立入口、独立管理与迭代。**不复用桌面伙伴的管理能力**（成长/skill/角色/记忆/抽屉）。目标 = **企业级对外服务**：能力**窄而深**（问答+知识库检索，深在 grounded 防幻觉与合规），迭代路径与桌面伙伴完全不同。

**已对齐决策（2026-07-03）**：
- **P1 范围 = 企业核心**：独立实体+控制台 + 身份/话术 + 知识库(grounded) + 服务守则 + 渠道部署 + 审计(搜索/天级保留)。护栏细化/转人工/营业时间/限流 → P2；版本化/灰度/分析看板 → P3。
- **知识库 = 复用平台知识库（nomifun-knowledge）+ 独立绑定 + grounded 只读**（不重建 KB 管理）。

## 1. 桌面伙伴 vs 对外伙伴（拆分依据）

| 维度 | 桌面伙伴 | 对外伙伴 |
|---|---|---|
| 服务对象 | 主人（可信） | 陌生人/客户（不可信） |
| 能力 | 广（shell/文件/电脑/浏览器/编排/记忆/技能） | 窄而深：仅问答+KB检索(+安全网搜) |
| 成长 | 自主学习/记忆/skill 进化 | 禁自主漂移（合规红线），只受控人工策展 |
| 形象 | 桌宠角色/DIY/情绪 | 企业品牌 logo/话术/语气 |
| 记忆 | 个人长期记忆 | 无个人记忆，仅会话审计留痕 |
| 知识 | 可回血/个人库 | 只读、策展、grounded 严格模式 |
| 配置 | 模型/形象/隐私/情绪 | 守则/护栏/营业时间/转人工/限流/版本 |
| 迭代 | 个人化随性 | 版本化/灰度/回滚/变更管控 |
| 数据 | `companion/companions/{id}` | `public-agents/{id}`（独立存储） |
| 管理 | 桌面伙伴中心 | 对外伙伴控制台（企业级） |

## 2. 架构：换掉"身份与管理"，保留"安全执行内核"

**两层边界：**
- **管理/数据/入口层 —— 独立重建**：新领域 `nomifun-public-agent` crate，实体 `PublicAgentConfig`，独立存储 `public-agents/{id}/`，独立控制台入口。不复用 companion。
- **运行时执行层 —— 复用已建成的安全内核**：exposure 钳制 / C3 修复 / KB 烘死 / 首轮语言（均身份无关）。运行时身份从 `companion_id` 改由 `public_agent_id` 解析配置，再套安全钳制。

**无环依赖（照 companion 范式）**：`nomifun-ai-agent` 定义 `PublicAgentProvider` trait（按 id 解析 persona/model/policy/kb/grounded）；`nomifun-public-agent` 实现它；`nomifun-app` 装配。ai-agent 不依赖 public-agent（无环）。

## 3. 数据模型 `PublicAgentConfig`（`public-agents/{id}/config.json`）

```rust
struct PublicAgentConfig {
    id: String,              // "pubagent_<uuidv7>"
    seq: Option<u64>,        // 展示序号（独立 watermark）
    name: String,
    greeting: String,        // 开场白/欢迎语（首轮下发）
    tone: String,            // 语气/风格规范（P1 自由文本）
    model: ModelConfig,      // 复用 nomifun-common ModelConfig
    knowledge_base_ids: Vec<String>,  // 绑定的平台 KB 子集（grounded 只读）
    grounded_mode: bool,     // true=只答 KB 内内容，查不到礼貌拒答（防幻觉，默认 true）
    service_policy: String,  // 服务守则/系统设定（P1 自由文本，结构化留 P2）
    audit_retention_days: u32, // 审计天级保留（默认 30）
    enabled: bool,           // 启用/停用
    created_at: i64,
    // 品牌 logo、营业时间、护栏、转人工、限流、版本 → P2/P3
}
```

存储/写盘：仿 companion `profile.rs`（原子 temp+rename）。`PublicAgentRegistry`（list/get/create/patch/delete，独立 seq watermark）。

**owner-only 安全字段**：`service_policy`/`grounded_mode`/`knowledge_base_ids` 等只经本机 owner REST 改；绝不经任何对外/网关路径（同 exposure 提权硬化原则）。对外伙伴运行时本就无网关，纵深防御仍钉死。

## 4. 运行时解析

- `NomiBuildExtra.public_agent_id: Option<String>`（新）。设置时：工厂经 `PublicAgentProvider` 解析 → 注入 persona(greeting/tone)+service_policy+grounded 指令+首轮语言指令；隐式 `exposure=PublicService` 钳制（安全白名单 + 无网关/computer/browser/spawn）；KB 烘死为 `knowledge_base_ids`。
- **grounded 指令**：grounded_mode=true 时系统提示强约束"只依据下方知识库作答；库中无据则礼貌说明无法回答或建议转人工，严禁编造"。
- 隐式 exposure：public_agent 会话 = PublicService（不再依赖 companion.exposure；companion.exposure 字段可退役或仅保留兼容）。

## 5. 渠道部署

渠道绑定从"绑 companion"扩展为"绑 companion 或 public_agent"。channel 绑定新增可选 `public_agent_id`；置位时 `message_service` 入站消息路由到 public-agent 会话（`NomiBuildExtra.public_agent_id` + surface=Channel + 隐式 PublicService）。一个渠道 bot 二选一（私人伙伴 or 对外伙伴）。

## 6. 审计（独立位置 + 搜索 + 天级保留）

- 存储：按天分区 `public-agents/{id}/audit/{day_index}.jsonl`，`day_index = at_ms / 86_400_000`（UTC 天，**无需日期库**）。
- 写：turn（每入站轮，截断文本）+ 关键事件（发布/守则变更/转人工 P2）。
- 保留：append 时机会性删除 `day_index < today - audit_retention_days` 的整天文件（天级淘汰天然）。
- 检索：`GET /api/public-agents/{id}/audit?limit=&cursor={at_ms}&q=&kind=&days=` → `{entries, next_cursor}`（新到旧，按天文件扫描+过滤+游标分页）。
- 手动删除：`DELETE /api/public-agents/{id}/audit?older_than_days=N`（删整天文件）。
- UI：控制台内**独立"审计&分析"区**（搜索/kind/天筛选/游标翻页 + 保留天数设置 + 手动清理）。

## 7. 新入口：独立"对外服务"控制台

侧栏**新增一级分组「对外服务 / Public Service」**（与常用/数据空间/…平级），内含**对外伙伴控制台**：
- **伙伴列表**：卡片/招募/启停/状态/指标。
- **单伙伴专属管理页（分区）**：概览 · 身份&话术 · 知识库(绑定+grounded) · 服务守则 · 渠道部署 · 审计&分析。
- 全程双语 i18n（en-US + zh-CN locale 文件，非仅 defaultValue）。

## 8. 迁移（对现有 9 提交的处置）

- **保留**：exposure 枚举+钳制、C3、KB 烘死、首轮语言、z-index 修复（运行时/通用）。审计后端**升级**为按天分区+保留+搜索并挪入 public-agent 域。
- **退役/重建**：`companion.exposure` 的管理用法、复用 companion 的 outbound 抽屉/角色/招募、当前 outbound Tab、companion-scoped 审计 → 由 `PublicAgent` 域 + 独立控制台取代。`companion.exposure` 字段与 set_exposure 路由退役（或短期兼容）。

## 9. 分期

- **P1（企业核心，可真机对外服务）**：`nomifun-public-agent` crate（实体+registry+CRUD+routes）→ `PublicAgentProvider`+运行时解析+隐式钳制 → KB 绑定(grounded) → 服务守则注入 → 渠道部署绑定 → 审计(按天/保留/搜索) → 前端"对外服务"控制台 + i18n → 退役旧 outbound 管理。
- **P2**：安全护栏细化 + 转人工/升级 + 营业时间 + 限流/黑名单 + 安全网搜 + 服务守则结构化。
- **P3**：版本/灰度/回滚 + 分析看板 + 未答问题策展回流。

## 10. 风险与护栏

- **无环依赖**：provider trait 在 ai-agent，impl 在 public-agent（照 companion）。
- **安全内核不动**：钳制/白名单/KB 烘死是既有已测机制，只换配置来源。
- **owner-only 敏感字段**：policy/grounded/kb 绑定绝不经对外/网关路径。
- **迁移期并存**：新域上线后再退役旧 outbound，避免断档。
- **不复用 companion 管理**：新 crate 自持 CRUD/registry/存储，杜绝耦合回流。
