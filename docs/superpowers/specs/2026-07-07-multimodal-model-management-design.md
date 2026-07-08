# 统一多模态模型能力中心 — 设计文档

- **日期**: 2026-07-07
- **分支**: `feat/multimodal-model-hub`(基于 `feat/creative-workshop`)
- **状态**: 设计已批准,待实施

---

## 1. 背景与问题

### 1.1 触发案例(已实测坐实)

用户录入 StepFun Step Plan 图像模型 `step-image-edit-2` 后,点击「健康检查」失败:

```
Provider error: API error 404:
{"error":{"message":"The model \"step-image-edit-2\" does not exist or you do not have access to it.","type":"model_invalid"}}
```

用实测验证根因(用户提供的 key,2026-07-07):

| 测试 | Endpoint | 结果 |
|---|---|---|
| 正确图像端点 | `POST https://api.stepfun.com/step_plan/v1/images/generations` | ✅ HTTP 200,返回 `data[0].b64_json` |
| 错误 chat 端点 | `POST https://api.stepfun.com/v1/chat/completions` | ❌ HTTP 404 `model_invalid`(与截图逐字一致) |

**key 有效、模型真实存在。** 探测把图像模型当文本模型,打到了 `/chat/completions`,而 Step Plan 图像模型只服务于 `/step_plan/v1/images/*` → 404。

### 1.2 问题的本质

这不是 StepFun 个例,而是架构缺口:**模型管理层对"模态"完全无知**。

投诉链路的精确定位(`feat/creative-workshop` 现状,勘察坐实):

- **无 `models` 表**:一个"模型"只是 `providers.models[]` JSON 数组里的一个字符串(`crates/backend/nomifun-db/migrations/001_baseline.sql:82-100`)。模型身份 = `(provider_id, model)` 值对。每模型属性散落在 provider 行上的并行 JSON map(`model_enabled` / `model_health` / `model_protocols` / `model_context_limits` / `model_descriptions` / `sort_order`)。
- **能力/模态无持久化**:能力只存在 **provider 级**(`crates/backend/nomifun-api-types/src/provider.rs:25-31`);运行时靠**按模型名猜测**的启发式(Rust `crates/backend/nomifun-api-types/src/model_capability.rs` ⇄ TS `ui/src/common/utils/modelCapabilities.ts` 双份重复)。
- **两套模态词表并存**:`ModelType`(10 项:Text/Vision/FunctionCalling/ImageGeneration/VideoGeneration/WebSearch/Reasoning/Embedding/Rerank/ExcludeFromPrimary,**无 TTS/ASR**)与 `MediaCapability`(`t2i|i2i|inpaint|t2v|i2v|v2v|tts|text`,`crates/backend/nomifun-creation/src/types.rs:9-57`)。
- **404 精确位置**:`crates/backend/nomifun-ai-agent/src/factory/nomi.rs:951-954`,对 `stepfun`/`stepfun-plan`/`ark`/`zhipu` 等平台**硬编码 `/chat/completions`**;探测链路 `crates/backend/nomifun-ai-agent/src/services/provider_health.rs:70-82` **完全不分模态**;前端只传 `{provider_id, model}` 无模态提示。
- **三套互不相通的执行子系统**:
  | 子系统 | Crate | Trait | 入口 | 模态 |
  |---|---|---|---|---|
  | LLM/聊天(流式) | `crates/agent/nomi-providers` | `LlmProvider` | `create_provider()`→`.stream()` | text, vision-in |
  | 媒体生成(submit/poll) | `crates/backend/nomifun-creation` | `MediaProvider` | `CreationService::create_task()` | t2i,i2i,inpaint,t2v,i2v |
  | ASR/STT(独立) | `crates/backend/nomifun-shell` | 无 | `SttService::transcribe()` | audio→text |

  **图像/视频分发其实已能跑**(`nomifun-creation/src/adapters/openai_images.rs:34-63` 走 `/v1/images/generations`)——坏的只是"模型管理"这条链路对模态无知。ASR 更是独立在客户端偏好 `speechToText` 里(凭据重复一份)。

- **前端**:模型管理页 4 个 Tab(Agent / 模型 / 创作模型 / 全局模型配置)。`创作模型`(`CreationModelsContent.tsx`)只是 providers 的**只读镜头**(启发式过滤 + 平台级能力覆盖),非独立存储。`添加模型`表单(`AddModelModal.tsx:85-121`)只有 3 个字段(model id / context window / protocol),**无模态选择器**。

---

## 2. 目标与非目标

### 2.1 目标

1. 让每个模型拥有**权威、持久化**的模态/能力,彻底取代按名字猜测的启发式。
2. **探测/健康检查按模态命中正确端点**——StepFun `step-image-edit-2` 探测返回健康。
3. 全模态**就绪**:图像生成/编辑、TTS、ASR、embedding、视频、视觉输入统一在一套词表、数据模型、UI、探测、分发解析里。
4. **本轮图像(生成+编辑)端到端接通并用 StepFun 实测**;其余模态一等公民、分发增量补。
5. ASR/TTS/embedding **并入统一的 provider+能力 管理**(一个地方管所有类型的模型)。
6. `创作模型` Tab **收敛**为 `模型` 页的模态筛选视图(单一真相源)。
7. **服务配置能力**:逐模型/逐任务的参数(TTS 音色、ASR 语言、图像尺寸/步数、端点覆盖等)。
8. **易扩展**:录入任意"非标准"多模态 provider 无需改代码(逐模型端点/请求体覆盖)。

### 2.2 非目标(YAGNI)

- **不合并三套执行引擎**。它们的执行契约本就不同(流式 vs 异步轮询 vs 同步),强行合并更糟。只统一「目录 + 能力解析 + 端点解析」。
- **不做完整"roles"系统**(无消费方);只做"按任务的默认模型"。
- **不迁移**现有 `model_enabled/health/context/description/sort` 那几张 map(能用、与多模态无关)。
- 不为 TTS/embedding 现造消费方(伙伴朗读 / RAG 后续接);本轮只做到"可管理+可探测+可分发"的就绪槽位。

---

## 3. 架构

### 3.0 核心取舍:统一目录,专精引擎

问题不在分发,而在模型管理层对模态无知。方案:引入
1. **一个权威「模型能力档案」目录**(真相源:这个模型能做什么);
2. **一个「任务→端点/请求体」解析器**(给定 provider+model+task → HTTP 目标);
3. **一个「按能力取模型」后端权威**(消费方问能力要模型)。

三套执行引擎**保持各自专精**,但都从**同一份档案**取真相、经**同一个解析器**定位端点。

### 3.1 统一能力词表(消灭双词表)

统一为**两个正交维度**:

- **`ModelTask`(决定端点+请求体的"任务")**:
  `Chat` · `ImageGeneration` · `ImageEdit` · `VideoGeneration` · `SpeechSynthesis`(TTS) · `SpeechRecognition`(ASR) · `Embedding` · `Rerank`
- **`ModelTrait`(同一任务内的细化,主要修饰 Chat)**:
  `VisionInput`(能吃图) · `FunctionCalling` · `Reasoning` · `WebSearch`

一个模型 = `{ tasks: Set<ModelTask>, traits: Set<ModelTrait>, params }`。一模型可多任务(如 `step-image-edit-2` 同时是 `ImageGeneration + ImageEdit`)。

**旧词表在边界处派生,过渡期老消费方零感知**:
- `ModelType::Text` → `Chat`;`Vision` → trait `VisionInput`;`FunctionCalling/Reasoning/WebSearch` → 同名 trait;`ImageGeneration/VideoGeneration/Embedding/Rerank` → 同名 task。
- `MediaCapability::t2i` → `ImageGeneration`;`i2i`/`inpaint` → `ImageEdit`;`t2v/i2v/v2v` → `VideoGeneration`;`tts` → `SpeechSynthesis`;`text` → `Chat`。

**关键洞见**:是 **task 决定端点和请求体**(404 的根),trait 只在同模态内细化选择。

### 3.2 数据模型:`model_profiles` 表(权威、非补丁)

新增专用表(迁移 `033`),以 `(provider_id, model)` 值对为主键——**消费方零迁移**,又不再往 provider 行塞第 7 张 JSON map:

```sql
-- 033_model_profiles.sql
CREATE TABLE model_profiles (
    provider_id TEXT    NOT NULL,
    model       TEXT    NOT NULL,
    tasks       TEXT    NOT NULL DEFAULT '[]',   -- JSON: ModelTask[]
    traits      TEXT    NOT NULL DEFAULT '[]',   -- JSON: ModelTrait[]
    params      TEXT    NOT NULL DEFAULT '{}',   -- JSON: 服务配置(见 §3.8)
    source      TEXT    NOT NULL DEFAULT 'inferred', -- inferred|user|catalog 溯源
    updated_at  INTEGER NOT NULL,
    PRIMARY KEY (provider_id, model),
    FOREIGN KEY (provider_id) REFERENCES providers(id) ON DELETE CASCADE
);
```

- **名字启发式降级**为"一次性回填种子 + 新模型默认建议",**不再是运行时权威**。
- 迁移时用现有启发式**回填全部存量模型**,老聊天/图像模型行为不回归。
- `source` 溯源:`user`(手改,最高权威)> `catalog`(本地 AI 目录)> `inferred`(自动猜)。用户手改后启发式不再覆盖。
- 迁移由 `sqlx::migrate!()` 自动发现(`crates/backend/nomifun-db/src/database.rs:28`),无需注册列表;本分支无硬编码迁移计数常量需 bump。

### 3.3 「任务→端点/请求体」解析器(404 根治 + 长尾可扩展)

新建单一权威:`resolve_dispatch_target(provider_row, model, task) -> DispatchTarget { url, method, request_kind }`。

- **按任务的默认约定**(OpenAI 兼容):
  | Task | 方法 | 追加路径 | 请求体 |
  |---|---|---|---|
  | Chat | POST | `/chat/completions` | JSON messages |
  | ImageGeneration | POST | `/images/generations` | JSON |
  | ImageEdit | POST | `/images/edits` | multipart |
  | VideoGeneration | POST | `/videos`(submit→poll) | JSON |
  | SpeechSynthesis | POST | `/audio/speech` | JSON |
  | SpeechRecognition | POST | `/audio/transcriptions` | multipart |
  | Embedding | POST | `/embeddings` | JSON |
- **平台覆盖**:StepFun Step Plan 的 base 已含 `/step_plan/v1`,`ImageGeneration` 追加 `/images/generations` 即得实测通过的正确 URL。
- **逐模型覆盖逃生舱**(`params.endpoint` / `params.request_shape`):录入任何"非标准"多模态 provider(Deepgram `/v1/listen`、自建服务)时,无需改代码即可接入。**这是"方便录入其他多模态模型"的核心机制。**

`factory/nomi.rs:951-954` 的硬编码 `/chat/completions` 由本解析器取代:非 Chat 任务不再走聊天端点。**探测与真实分发共用此解析器**。

### 3.4 模态感知探测(直接修好 StepFun 404)

- 前端健康检查按钮携带**被探测的 task**(从档案读;多任务模型让用户选一个探)。
- 后端 `provider_health.rs` 不再一律走聊天:按 task 经 §3.3 解析器命中正确端点,发**该任务的最小合法请求**(图像 steps=1/最小尺寸、Chat "回 OK"、TTS 短句…),据响应判定:可达且鉴权通过=健康;`model_invalid`/401/网络错=不健康并给原因。
- 生成类探测消耗极少量额度,UI 明示。实现期评估"最小生成 vs 只探可达性"两种更省策略。

**验收**:StepFun `step-image-edit-2` 探测命中 `/step_plan/v1/images/generations` 返回 200 = 健康。

### 3.5 分发与消费:「按能力取模型」后端权威

- 新增 `resolve_models(task, traits?)` / `default_model(task)` 后端服务——消费方问能力要模型,不再各自客户端启发式过滤。
- 三套执行引擎保持不动,但都改为**从档案取能力、经解析器定端点**。
- `全局模型配置` 增加**按任务的默认模型**(默认图像模型、默认 TTS 模型…),与既有 IDMM/记忆/伙伴默认一致。**不做**完整 roles。
- `创作模型` 镜头改为对能力权威的查询。

### 3.6 ASR/TTS/embedding 并入统一管理

- **ASR**:现独立在客户端偏好 `speechToText`(`ui/src/common/types/provider/speech.ts:9-34`,凭据重复)。改为在 providers 表登记 `SpeechRecognition` 模型,STT 服务(`crates/backend/nomifun-shell`)从档案取 provider+model+端点;**一次性迁移**旧 `speechToText` 配置 → provider 行。Deepgram 异形 API(`/v1/listen`)用 §3.3 逐模型覆盖桥接(不强塞)。
- **TTS / Embedding**:新任务槽,做到"可管理+可探测+可分发",暂无消费方(就绪槽位)。

### 3.7 前端

- **添加模型**表单(`AddModelModal.tsx`):加**模态/任务多选 + trait + 参数**,由启发式预填(常见模型一键)、可改。
- **模型行**(`ModelModalContent.tsx:960-1024`):模态徽章 + 逐模型模态/trait/参数编辑(复用现有 description/context popover 形态);健康检查按钮模态感知。
- **`创作模型` Tab → 收敛**为 `模型` 页按 task 的筛选视图,单一真相源。
- **平台预设**(`ui/src/renderer/utils/model/modelPlatforms.ts`)补多模态预设:如「StepFun 阶跃图像」预设 base=`https://api.stepfun.com/step_plan/v1` + 模型 `step-image-edit-2` 预标 `ImageGeneration+ImageEdit`,开箱即用。
- **视觉/工程约束**(硬门槛):Arco Design;`@icon-park/react` 具名导入不起别名(否则运行时崩);toast 走 `useArcoMessage`;真 `<button>` 会露 WebView2 黑框,用 `<div onClick>`;Arco Popover 外壳内边距清零;**UI 必须漂亮**(验收门槛)。

### 3.8 服务配置能力(`params`)

`model_profiles.params` 持有逐模型/逐任务的**服务配置**:
- TTS:音色 `voice`、语速 `speed`、格式。
- ASR:语言 `language`、时间戳。
- 图像:默认 `size` / `steps` / `cfg_scale` / `text_mode` / `response_format`。
- 通用:`endpoint` 覆盖、`request_shape` 覆盖、`timeout`。

UI 在模型参数编辑器里呈现,分发时作为默认注入。

---

## 4. 数据流

### 4.1 录入 + 探测(图像)
1. 用户在 `添加模型` 选平台预设「StepFun 阶跃图像」→ base 与模型预填,模态预标 `ImageGeneration+ImageEdit`。
2. 提交 → 写 `providers.models` + `model_profiles` 行(`source=user`)。
3. 点健康检查 → FE 带 `task=ImageGeneration` → 后端 §3.3 解析 `…/step_plan/v1/images/generations` → 最小请求 → 200 → 健康。

### 4.2 消费(创意工坊图像)
1. 工坊问能力权威 `resolve_models(ImageGeneration)` → 候选模型。
2. 用户选一个 → `nomifun-creation` 从档案取能力 + §3.3 解析端点 + `params` 注入默认 → 生成。

### 4.3 消费(ASR)
1. 语音输入 → `resolve_models(SpeechRecognition)` / 默认 → STT 服务从档案取 provider+端点(不再读客户端偏好)→ 转写。

---

## 5. 分期(供 writing-plans 展开)+ 并发策略

**并发原则**(按 memory `parallelize-disjoint-tasks-worktree`):地基串行锁契约,之后按文件不相交聚类用 git worktree 并行;耦合热点(共享 Rust/TS 类型)串行。

- **Phase A — 地基(串行,阻塞后续)**:词表(`ModelTask`/`ModelTrait` + 旧词表派生)+ `model_profiles` 表迁移 + 回填 + Rust 类型 + `storage.ts` 镜像 + IPC 契约 + 解析器 + 能力权威。**这是耦合热点,必须先做。**
- **Phase B — 探测(后端并行)**:`provider_health.rs` + `factory/nomi.rs` 模态感知。→ **StepFun 404 当场修好、可实测。**
- **Phase C — 前端(与后端并行,ui/ 零重叠)**:添加模型模态选择器 / 模型行模态 UI / 创作模型→筛选 / 参数编辑器 / 平台预设。
- **Phase D — 图像端到端(后端并行,nomifun-creation)**:图像生成+编辑分发接档案 + 解析器;创意工坊 + 探测跑通 StepFun,真机验收。
- **Phase E — 语音/embedding 并入(后端并行,nomifun-shell + 新分发)**:ASR/TTS/embedding 一等化;ASR 客户端偏好迁移 → provider 行。排最后、可回退。

契约锁定后,并行切面:**前端(ui/) ∥ 后端 B ∥ 后端 D ∥ 后端 E**,各在 worktree,主会话集成 + 验收。

---

## 6. 测试策略

- **Rust 单元/集成**(nextest,只跑触碰 crate):词表派生映射双向、`model_profiles` repo CRUD + 回填、解析器每任务端点(尤其 StepFun Step Plan 平台覆盖)、探测按 task 分派。
- **契约**:Rust 类型 ⇄ `storage.ts` 手工镜像一致(无 ts-rs);清单化改动点。
- **前端**:`bun run typecheck` exit=0 零新错(无可跑 vitest);`check:icons` 门禁。
- **端到端实测**:用用户 key 探测 StepFun `step-image-edit-2` 返回健康 + 工坊生成一张图。
- **回归**:现有聊天/视觉/工坊图像不回归(启发式回填保证)。

---

## 7. 风险 / 取舍

- **不合并三引擎**:刻意取舍,见 §3.0。
- **手工双改负担**:provider 类型无 ts-rs;schema 改动要 Rust + `storage.ts` 双改。改动点集中、清单化。
- **ASR 迁移**是最大结构变动(动客户端偏好 + 异形 provider),排最后一期、可回退。
- **探测成本**:生成类探测消耗少量额度;实现期评估省额度策略 + UI 明示。
- **并行合并**:worktree 隔离避免互相覆盖;FE/BE 天然零重叠,后端各 Phase 按不同 crate/文件切分降低冲突。

---

## 8. 关键改动文件(勘察交叉引用)

- 数据模型:`crates/backend/nomifun-api-types/src/provider.rs:8-31`、新 `crates/backend/nomifun-db/migrations/033_model_profiles.sql`、新 repo、`ui/src/common/config/storage.ts:507-607`
- 词表/派生(取代启发式):`crates/backend/nomifun-api-types/src/model_capability.rs`、`ui/src/common/utils/modelCapabilities.ts`、`crates/backend/nomifun-creation/src/types.rs:9-57`
- 解析器 + 探测修复:`crates/backend/nomifun-ai-agent/src/services/provider_health.rs:70-82`、`crates/backend/nomifun-ai-agent/src/factory/nomi.rs:951-954`
- 图像分发:`crates/backend/nomifun-creation/src/adapters/mod.rs:37-45,56-77`、`service.rs`
- ASR 并入:`crates/backend/nomifun-shell/src/routes.rs:155-178`、`ui/src/common/types/provider/speech.ts:9-34`
- 前端:`ui/src/renderer/pages/settings/components/AddModelModal.tsx:85-121`、`ui/src/renderer/components/settings/SettingsModal/contents/ModelModalContent.tsx:960-1024`、`ui/src/renderer/components/.../CreationModelsContent.tsx`、`ui/src/renderer/utils/model/modelPlatforms.ts`、`ui/src/renderer/pages/modelHub/index.tsx`
