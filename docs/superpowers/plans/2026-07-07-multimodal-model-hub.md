# 统一多模态模型能力中心 — 实施计划

> **For agentic workers:** REQUIRED SUB-SKILL: superpowers:subagent-driven-development / executing-plans。步骤用 `- [ ]` 跟踪。

**Goal:** 让模型管理对模态一等感知,修好 StepFun 图像模型探测 404,并把多模态(图像/TTS/ASR/embedding/视频)模型的管理、探测、分发、服务配置统一到一套权威能力档案 + 端点解析器上。

**Architecture:** 新增权威 `model_profiles` 档案(键 `(provider_id, model)`)取代名字启发式;新增「任务→端点/请求体」解析器,探测与分发共用;三执行引擎(聊天/媒体/语音)专精不合并,均从档案取真相。

**Tech Stack:** Rust(axum + sqlx/SQLite + reqwest)、React + Arco Design + UnoCSS、SWR、`@icon-park/react`。

## Global Constraints

- **无 ts-rs**:provider/模型类型手工镜像。任何 schema 改动须同步:`crates/backend/nomifun-api-types/src/provider.rs`(或新 `model_task.rs`)+ `ui/src/common/config/storage.ts` + `ui/src/common/types/provider/providerApi.ts`。
- **迁移**:下一个编号 `033`;仅追加;绝不改 `001_baseline` 校验和。`sqlx::migrate!()` 自动发现(`database.rs:28`)。
- **前端**:Arco Design;`@icon-park/react` 具名导入**禁起别名**;toast 用 `useArcoMessage`(遵循所在文件既有模式);真 `<button>` 露 WebView2 黑框——用 Arco `<Button>` 或 `<div onClick>`;Arco Popover 外壳内边距清零;locale 改动后跑根 `bun run gen:i18n`;`bun run typecheck` 必 exit 0 零新错;`bun run check:icons` 门禁。**UI 必须漂亮**。
- **测试**:nextest 只跑触碰 crate,全量收尾;前端无 vitest,靠 typecheck。
- **提交**:每任务一提交;分支 `feat/multimodal-model-hub`。

---

## 契约(所有任务共享,务必一致)

### Rust — `crates/backend/nomifun-api-types/src/model_task.rs`(新)
```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelTask {
    Chat, ImageGeneration, ImageEdit, VideoGeneration,
    SpeechSynthesis, SpeechRecognition, Embedding, Rerank,
}
// wire: chat|image_generation|image_edit|video_generation|speech_synthesis|speech_recognition|embedding|rerank

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelTrait { VisionInput, FunctionCalling, Reasoning, WebSearch }
// wire: vision_input|function_calling|reasoning|web_search

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ProfileSource { #[default] Inferred, User, Catalog }

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ModelProfile {
    pub provider_id: String,
    pub model: String,
    pub tasks: Vec<ModelTask>,
    pub traits: Vec<ModelTrait>,
    #[serde(default)] pub params: serde_json::Value,
    #[serde(default)] pub source: ProfileSource,
    pub updated_at: i64,
}

// 启发式派生(复用 model_capability.rs 的 include 表 + 新增 tts/asr/embed/rerank)
pub fn derive_tasks_and_traits(model: &str) -> (Vec<ModelTask>, Vec<ModelTrait>);
```

### Rust — `crates/backend/nomifun-api-types/src/dispatch_target.rs`(新)
```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RequestShape { Json, Multipart }

#[derive(Debug, Clone, PartialEq)]
pub struct DispatchTarget { pub url: String, pub method: String, pub shape: RequestShape }

/// 约定 + 平台覆盖 + params.endpoint 逐模型逃生舱。
pub fn resolve_dispatch_target(
    platform: &str, base_url: &str, is_full_url: bool,
    task: ModelTask, params: &serde_json::Value,
) -> DispatchTarget;
```
约定路径:Chat`/chat/completions`、ImageGeneration`/images/generations`、ImageEdit`/images/edits`(multipart)、VideoGeneration`/videos`、SpeechSynthesis`/audio/speech`、SpeechRecognition`/audio/transcriptions`(multipart)、Embedding`/embeddings`、Rerank`/rerank`。规则:`is_full_url` → base 原样;`params.endpoint`(字符串)存在 → 覆盖整个 path;否则 `{base 去尾/}{约定 path}`。StepFun base 已含 `/step_plan/v1`,ImageGeneration → `https://api.stepfun.com/step_plan/v1/images/generations`(实测正确)。

### Rust — `crates/backend/nomifun-api-types/src/model_catalog.rs`(新)
```rust
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ModelRef { pub provider_id: String, pub model: String }

/// 纯函数:从 providers + profiles 里筛出支持 task 且含全部 required_traits 的启用模型。
pub fn resolve_models(
    providers: &[ProviderResponse], profiles: &[ModelProfile],
    task: ModelTask, required_traits: &[ModelTrait],
) -> Vec<ModelRef>;
```

### Rust — 探测请求扩展 `provider.rs`
```rust
pub struct ProviderHealthCheckRequest {
    pub provider_id: String,
    pub model: String,
    #[serde(default)] pub task: Option<ModelTask>, // None → 查档案主任务,再 fallback Chat
}
```

### TS 镜像 — `ui/src/common/config/storage.ts`
```ts
export type ModelTask = 'chat'|'image_generation'|'image_edit'|'video_generation'|'speech_synthesis'|'speech_recognition'|'embedding'|'rerank';
export type ModelTrait = 'vision_input'|'function_calling'|'reasoning'|'web_search';
export type ProfileSource = 'inferred'|'user'|'catalog';
export interface ModelProfile { provider_id: string; model: string; tasks: ModelTask[]; traits: ModelTrait[]; params?: Record<string, unknown>; source?: ProfileSource; updated_at: number }
```

### HTTP 路由(新,provider 路由旁)
- `GET  /api/model-profiles` → `ModelProfile[]`
- `POST /api/model-profiles` → upsert;body `{provider_id, model, tasks, traits, params?, source?}`;返回 `ModelProfile`
- `POST /api/model-profiles/delete` → body `{provider_id, model}`
- `POST /api/model-profiles/resolve` → body `{task, required_traits?}` → `{models: ModelRef[]}`

---

## 分期与任务

**并发切面**:Phase A 串行(主工作区,锁契约)→ 契约提交后:**Phase C 前端(worktree 并行 agent,只碰 ui/)** 与 **后端 B/D/E(主工作区,按 crate 顺序)** 同时进行。FE/BE 路径零重叠,合并无冲突。

### Phase A — 地基(串行,阻塞后续)
- **A1** 词表 + 派生:`model_task.rs`(枚举 + `derive_tasks_and_traits`);`nomifun-api-types/src/lib.rs` 导出。测试:派生映射(gpt-4o→[Chat]+[VisionInput];dall-e→[ImageGeneration];whisper→[SpeechRecognition];tts→[SpeechSynthesis];embed→[Embedding];text-only→[Chat])。
- **A2** 解析器:`dispatch_target.rs` + 测试(StepFun-plan ImageGeneration 得 `/step_plan/v1/images/generations`;is_full_url 原样;params.endpoint 覆盖;Chat 得 `/chat/completions`)。
- **A3** 迁移 `033_model_profiles.sql` + 模型 `models/model_profile.rs`(`ModelProfileRow` + `UpsertModelProfileParams<'a>`)。
- **A4** repo:`repository/model_profile.rs`(trait `IModelProfileRepository`)+ `sqlite_model_profile.rs`(仿 `sqlite_skill_tag.rs` upsert + `conversation_mcp_servers` 复合键);`repository/mod.rs`/`models/mod.rs`/`lib.rs` 导出。测试:upsert/get/list/delete round-trip。
- **A5** 目录纯函数:`model_catalog.rs` + 测试。
- **A6** 装配 + reconcile:`AppServices::from_config` 构造 `Arc<dyn IModelProfileRepository>`;`ModelProfileReconciler` 启动时为所有 provider models 无档案者按派生插入 `source=inferred`;provider create/update 后补档。
- **A7** HTTP 路由(list/upsert/delete/resolve)+ `ipcBridge.ts` 入口(TS 契约镜像)。测试:路由级。

### Phase B — 探测模态感知(主工作区;修好 404)
- **B1** `ProviderHealthCheckRequest.task` 字段 + TS 镜像。
- **B2** `provider_health.rs`:`health_check` 先定任务(req.task → 档案主任务 → Chat);Chat 走原路;非 Chat 走新 `run_modality_probe`(用解析器命中端点,发最小请求,分类响应)。图像:generations JSON `{model,prompt,n:1,response_format:b64_json}`;edit-only:embed 1x1 png multipart;tts:`/audio/speech`;asr:embed 极短 wav;embedding:`/embeddings {input:"hi"}`。复用现有错误分类。
- **B3** 实测门:StepFun `step-image-edit-2` 档案标 ImageGeneration → 探测命中 `/step_plan/v1/images/generations` → healthy。

### Phase C — 前端(worktree 并行)
- **C1** TS 契约镜像(storage.ts / providerApi.ts / ipcBridge.ts model-profiles 组)。
- **C2** `useModelProfiles` hook(SWR,list + upsert/delete/resolve)。
- **C3** `AddModelModal`:加模态多选 + trait + params;选模型时用派生预填(前端 `modelCapabilities.ts` 或调 resolve 预览),提交时 upsert 档案(source=user)。
- **C4** `ModelModalContent` 模型行:模态徽章 + 档案编辑 popover(复用 description popover 形态);健康检查按钮携带 task(多任务弹选择)。
- **C5** `CreationModelsContent` → 收敛为「模型」页按 task 筛选(或改为读档案的筛选视图);`modelHub/index.tsx` 相应调整。
- **C6** `modelPlatforms.ts` 多模态预设(StepFun 阶跃图像:base `/step_plan/v1` + 模型 `step-image-edit-2` 预标 ImageGeneration+ImageEdit)。
- **C7** i18n keys + 根 `gen:i18n`;typecheck 0;check:icons。

### Phase D — 图像端到端(主工作区)
- **D1** `nomifun-creation`:`route_adapter_id`/`select_adapter` 改为优先按档案 task(而非名字启发式)路由;端点用解析器(替换 `openai_versioned_base` 硬拼)。
- **D2** `params` 注入:档案 params(size/steps/cfg_scale/text_mode/response_format)作为生成默认。
- **D3** 实测:创意工坊用 StepFun `step-image-edit-2` 生成一张图成功。

### Phase E — 语音/embedding 并入(主工作区;排最后可回退)
- **E1** ASR:STT 服务从 providers+档案取 provider/model/端点(经解析器),不再只读客户端偏好;一次性迁移旧 `speechToText` 配置 → provider 行(启动 reconcile 或首次读取时)。Deepgram 异形端点用 `params.endpoint` 覆盖。
- **E2** TTS/embedding:做到可分发的最小适配(就绪槽位,无消费方也能探测/调用)。
- **E3** 回归:桌面语音输入仍可用。

---

## 验收(可交付判据)
- StepFun `step-image-edit-2` 健康检查返回 healthy(实测)。
- 添加模型可选模态;模型行显示模态徽章;创作模型收敛为筛选。
- `cargo check`/nextest 触碰 crate 全绿;`bun run typecheck` exit 0;`check:icons` 绿。
- 现有聊天/视觉/工坊图像不回归。
