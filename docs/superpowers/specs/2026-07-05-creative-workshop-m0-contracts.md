# 创意工坊 M0 契约与模块分工(实施规范)

- 配套 PRD:`docs/superpowers/specs/2026-07-05-creative-workshop-prd.md`(先读它)
- 本文是所有创意工坊开发 agent 的**共同契约**:接口/Schema 以此为准,模块只能改自己名下的文件。

---

## 0. 所有 agent 的铁律

1. **禁止读取 `D:\code\nomito\explorer\nomifun\download\infinite-canvas` 下任何文件**(AGPL 净室红线)。功能语义不清楚时,按 PRD 描述 + 本文契约实现;仍有歧义则在产出报告中列出"待澄清",不要自行去看参考源码。
2. **禁止执行任何 git 命令**(不 commit/不 checkout/不 stash);提交由主控统一做。
3. 只创建/修改**自己模块所有权表(§9)名下的文件**。发现需要改别人的文件 → 报告,不要动手。
4. 开发期只跑**自己触碰 crate** 的 `cargo check -p` / `cargo nextest run -p`;禁全量测试、禁启动应用。本机存在与本任务无关的存量环境性测试失败,只看自己 crate 的结果。
5. 前端硬门槛:`bun run typecheck`(在 `ui/` 下)必须 exit 0 零新错;禁 `any`/`ts-ignore`;Arco 弹窗必经 `useArcoMessage`;真按钮用 `<div onClick>`;`@icon-park/react` 具名导入禁别名;改 locale 后跑根目录 `bun run gen:i18n` 并保证 `bun run check:i18n` 过。
6. UI 必须漂亮(验收门槛):对齐既有视觉语言(参考 knowledge / publicCompanion / modelHub 页面),间距/圆角/色彩用主题变量与 UnoCSS 语义类。
7. 不确定仓库惯例时,**照抄范例域**:后端范例 = `crates/backend/nomifun-public-agent`(crate 结构)与 `nomifun-knowledge`(routes/service),前端范例 = `ui/src/renderer/pages/knowledge`。

## 1. 命名与 ID

- 领域代号 `workshop`;生成引擎 `creation`。
- ID 前缀:画布 `wsc_`、资产 `wsa_`、生成任务 `wst_`,后接 uuidv7(用仓库现有 id 生成工具,搜 `att_` 附件 id 的生成方式照抄)。
- 时间戳一律 INTEGER 毫秒(Unix ms)。
- Wire JSON 一律 **snake_case**。画布正文 doc 对后端是**不透明 JSON**(后端只存取、算 node_count、限制大小 ≤ 8MB),doc 内部字段由前端契约(§4)约束。

## 2. 数据库迁移 `032_workshop.sql`

实施者须先看 `018_orchestrator.sql`/`029_task_on_fail.sql` 的风格,并检查 `nomifun-db/src/database.rs` 与 db_lifecycle 的 pre_baseline 机制(历史规矩:每加迁移需同步 bump pre_baseline,如机制存在必须照做)。表结构(可按仓库惯例微调列序/索引名,语义不得变):

```sql
CREATE TABLE workshop_canvases (
    id TEXT PRIMARY KEY,
    title TEXT NOT NULL,
    thumbnail_rel_path TEXT,
    node_count INTEGER NOT NULL DEFAULT 0,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);

CREATE TABLE workshop_assets (
    id TEXT PRIMARY KEY,
    kind TEXT NOT NULL,                 -- image | video | text
    title TEXT NOT NULL,
    collection TEXT,                    -- 集合(角色/场景…),可空
    tags TEXT NOT NULL DEFAULT '[]',    -- JSON string[]
    rel_path TEXT,                      -- 相对 data_dir;text 资产可空
    thumb_rel_path TEXT,
    mime TEXT,
    width INTEGER,
    height INTEGER,
    bytes INTEGER,
    text_content TEXT,                  -- kind=text 正文
    in_library INTEGER NOT NULL DEFAULT 1,  -- 1=出现在资产库;0=画布内部素材
    origin TEXT,                        -- JSON:{prompt,model,provider_id,params,canvas_id,node_id,task_id} 可空
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);
CREATE INDEX idx_workshop_assets_kind ON workshop_assets(kind);
CREATE INDEX idx_workshop_assets_library ON workshop_assets(in_library);

CREATE TABLE creation_tasks (
    id TEXT PRIMARY KEY,
    canvas_id TEXT,
    node_id TEXT,
    provider_id TEXT NOT NULL,          -- 类型须与 providers.id 实际类型一致(实施时核对!)
    model TEXT NOT NULL,
    capability TEXT NOT NULL,           -- t2i|i2i|inpaint|t2v|i2v|v2v|tts|text
    params TEXT NOT NULL,               -- JSON 参数快照
    status TEXT NOT NULL,               -- queued|running|succeeded|failed|canceled
    error TEXT,                         -- JSON {kind,message,http_status?} 可空
    result_asset_ids TEXT NOT NULL DEFAULT '[]',
    remote_task_id TEXT,                -- 异步协议远端任务 id(boot 恢复轮询用)
    attempt INTEGER NOT NULL DEFAULT 0,
    submitted_at INTEGER NOT NULL,
    started_at INTEGER,
    finished_at INTEGER
);
CREATE INDEX idx_creation_tasks_status ON creation_tasks(status);
CREATE INDEX idx_creation_tasks_canvas ON creation_tasks(canvas_id);
```

模型/Repo 层跟随仓库现状:若既有表的 model+repository 都在 `nomifun-db`(如 provider),则同样放 `nomifun-db/src/models/` + `repository/`,trait 命名 `IWorkshopRepository`/`ICreationTaskRepository` 风格对齐。

## 3. REST API 契约

挂载与鉴权方式照抄 knowledge 路由(同层 merge、同套 auth extractor;**公开路由禁 extract CurrentUser** 的坑注意)。

### 3.1 画布 `/api/workshop/canvases`
- `GET  /api/workshop/canvases` → `{ "canvases": WorkshopCanvasMeta[] }`(按 updated_at 降序)
- `POST /api/workshop/canvases` body `{ "title"?: string }` → `WorkshopCanvasMeta`(默认标题"未命名画布",落库+建目录+写空 doc)
- `GET  /api/workshop/canvases/{id}` → `{ "meta": WorkshopCanvasMeta, "doc": <opaque json> }`
- `PUT  /api/workshop/canvases/{id}/doc` body `{ "doc": <opaque json> }` → `{ "updated_at": number }`(原子写 canvas.json;同步 node_count)
- `PATCH /api/workshop/canvases/{id}` body `{ "title": string }` → `WorkshopCanvasMeta`
- `DELETE /api/workshop/canvases/{id}` → 204(删索引行+域目录;资产行不删,GC 另管)

`WorkshopCanvasMeta` wire:`{ "id","title","thumbnail_url"(可空),"node_count","created_at","updated_at" }`。

### 3.2 资产 `/api/workshop/assets`
- `GET  /api/workshop/assets?kind=&collection=&q=&in_library=&page=&page_size=` → `{ "items": WorkshopAsset[], "total": number }`
- `POST /api/workshop/assets/upload` multipart(`file` 必填;`title`/`collection`/`tags`/`in_library` 可选)→ `WorkshopAsset`(校验 mime/大小≤64MB;图片提取宽高)
- `POST /api/workshop/assets` body(text 资产或登记)`{ "kind":"text","title","text_content","collection"?,"tags"? }` → `WorkshopAsset`
- `PATCH /api/workshop/assets/{id}` body 局部字段(title/collection/tags/in_library)→ `WorkshopAsset`
- `DELETE /api/workshop/assets/{id}` → 204(删行+删盘上文件)
- `GET  /api/workshop/files/{asset_id}`(可带 `?thumb=1`)→ 二进制(Content-Type=mime;路径穿越校验照抄 attachments)

`WorkshopAsset` wire:`{ "id","kind","title","collection","tags":[],"mime","width","height","bytes","in_library":bool,"text_content"(可空),"origin"(可空对象),"url","thumb_url"(可空),"created_at","updated_at" }`,其中 `url = /api/workshop/files/{id}`。

### 3.3 生成任务 `/api/creation/tasks`
- `POST /api/creation/tasks` body:
  ```json
  { "canvas_id"?, "node_id"?, "provider_id", "model", "capability",
    "params": { ... }, "inputs": [ { "asset_id": "wsa_...", "role": "reference"|"mask"|"first_frame"|"last_frame"|"video"|"audio" } ] }
  ```
  → `CreationTask`(M0 阶段:入库 queued 后立即置 failed,error.kind=`adapter_unavailable`;M2 实装)
- `GET  /api/creation/tasks?canvas_id=&status=&limit=` → `{ "tasks": CreationTask[] }`
- `GET  /api/creation/tasks/{id}` → `CreationTask`
- `POST /api/creation/tasks/{id}/cancel` → `CreationTask`

`CreationTask` wire:`{ "id","canvas_id","node_id","provider_id","model","capability","params","status","error"(可空),"result_asset_ids":[],"attempt","submitted_at","started_at","finished_at" }`。

`params` 常用字段(适配器各取所需):`prompt`,`negative_prompt`,`width`,`height`,`aspect`,`quality`,`count`,`seconds`,`resolution`,`generate_audio`,`watermark`,`seed`。

## 4. 画布 doc 契约(前端所有,后端不解析)

```ts
interface WorkshopCanvasDoc {
  schema: 1
  viewport: { x: number; y: number; zoom: number }
  background: 'dots' | 'lines' | 'blank'
  nodes: WorkshopNode[]
  edges: WorkshopEdge[]
}
type WorkshopNodeKind = 'image' | 'text' | 'video' | 'generator' | 'loop' | 'compare' | 'output' | 'group'
interface WorkshopNode {
  id: string; kind: WorkshopNodeKind
  x: number; y: number; w: number; h: number
  groupId?: string | null
  data: WorkshopNodeData   // 按 kind 判别联合,见下
}
interface WorkshopEdge { id: string; from: string; to: string }
```

M0 钉死的最小 data(后续模块只能**增**字段不能改语义):
- image:`{ assetId: string | null; naturalWidth?; naturalHeight?; caption? }`
- text:`{ content: string; fontSize?: number }`
- video:`{ assetId: string | null; durationMs? }`
- generator:`{ mode: 'image'|'text'|'video'; providerId?: string; model?: string; prompt: string; params: Record<string, unknown>; mentions: string[]; status: 'idle'|'queued'|'running'|'success'|'error'; taskId?: string | null; resultAssetIds: string[]; batch?: { expanded: boolean; primary?: string } ; errorMessage?: string }`
- loop/compare/output/group:M8 定义,M0 只保留 kind 枚举占位。

约定:**所有二进制内容一律先成为 workshop 资产**(上传/生成产物皆得 `wsa_` id),节点只存 `assetId`;画布内部上传默认 `in_library=0`。

## 5. 后端 crate:`nomifun-workshop`

`crates/backend/nomifun-workshop/`,完全对标 `nomifun-public-agent` 范式:
- `lib.rs`:`pub const WORKSHOP_REL_DIR: &str = "workshop";` 目录规划:`{data_dir}/workshop/canvases/{id}/canvas.json`、`{data_dir}/workshop/assets/{id}.{ext}`、`.../thumbs/{id}.webp`。
- `fsio.rs`:原子写(temp+rename)+ 损坏回退,照抄 public-agent。
- `service.rs`:`WorkshopService::start(data_dir, repo…) -> Arc<Self>`;画布 CRUD、doc 读写、资产存/取/删、serve 路径解析(严防目录穿越,对标 `nomifun-requirement/src/attachments.rs`)。
- `state.rs` + `routes.rs`(§3.1/3.2 全部路由)。
- 装配:`nomifun-app` 的 `services.rs`(start)、`router/state.rs`、`router/routes.rs`(merge),放 public-agent 旁。

## 6. 后端 crate:`nomifun-creation`

`crates/backend/nomifun-creation/`:
- `types.rs`:`MediaCapability`(t2i/i2i/inpaint/t2v/i2v/v2v/tts/text)、`CreationParams`、`CreationInput{asset_id,role}`、`TaskStatus`、`CreationError{kind,message,http_status}`。
- `provider.rs`:
  ```rust
  #[async_trait]
  pub trait MediaProvider: Send + Sync {
      fn id(&self) -> &'static str;                       // openai_images | media_async | gemini_image | ark | modelscope | comfyui
      fn supports(&self, cap: MediaCapability) -> bool;
      async fn submit(&self, req: &SubmitRequest) -> Result<SubmitAck, CreationError>;   // SubmitAck::Done(产物字节/URL) 或 ::Pending(remote_task_id)
      async fn poll(&self, remote_task_id: &str, req: &SubmitRequest) -> Result<PollResult, CreationError>; // Pending | Done | Failed
  }
  ```
- `service.rs`:`CreationService`——任务入库、每 provider 并发闸(信号量)、轮询循环(间隔 2.5s,上限约 10 分钟)、取消传播、产物落盘(调 workshop 资产登记回调/trait,避免直接依赖 nomifun-workshop 造环:定义 `AssetSink` trait,由 app 装配时用 WorkshopService 实现)、**boot 对账**(启动时把 running 任务恢复轮询或收敛 failed)。
- `adapters/`:M0 只建 `mod.rs` 空骨架;M2 实装。
- `routes.rs` + `state.rs`(§3.3)。
- 依赖:`reqwest`(经 `nomifun-net` 若其提供 client 惯例则复用)、`nomifun-common`(crypto/AppError)、provider 解密读取对标 `nomifun-system` 现有做法。

## 7. 前端布局

```
ui/src/renderer/pages/workshop/
  index.tsx                 # /workshop 画布列表画廊(M0 壳,M0 即可用:列表/新建/重命名/删除)
  CanvasPage.tsx            # /workshop/:id 编辑器壳(M0:加载 doc+空画布占位;M1 实装)
  api.ts                    # REST 客户端(按 §3 契约;请求通道照抄 knowledge 页面的做法)
  types.ts                  # §3/§4 的 TS 类型(snake_case wire + doc 类型)
  canvas/                   # M1 所有权
  assets/                   # M4 所有权
  editor/                   # M5 所有权
  generation/               # M7 所有权
```
- 路由:`Router.tsx` 加 `/workshop`、`/workshop/:id`(lazy + withRouteFallback,挂 ProtectedLayout 下)。
- 侧栏:`SiderWorkshopEntry.tsx`(仿 SiderKnowledgeEntry),放「常用」段,icon 选 @icon-park 与创作相关的(如 `Platte`/`MagicWand` 类,具名导入)。
- i18n:新建 4 个命名空间文件并在两个 `index.ts` 注册:`workshop.json`(壳+列表)、`workshopCanvas.json`(M1)、`workshopAssets.json`(M4)、`workshopEditor.json`(M5)。M0 把 4 个文件都建好(后三个先放最小占位 key),后续模块只改自己的 json。
- 画布主题镜像、minimap 修复、identity 缓存等 react-flow 经验:实施 M1 时照 `pages/orchestrator/RunDetail/DagCanvas.tsx` 的既有解法(这是我们自己的代码,允许也应该读)。

## 8. 网关 `caps_workshop`

按 gateway 3 步契约(lib.rs 顶部注释 + CI 守卫测试):`mod caps_workshop;` + registry 注册。M0 先提供一个只读能力 `nomi_workshop_list_canvases`(列画布 id/title/node_count)。M9 再扩(读状态/应用操作/触发生成)。

## 9. 模块所有权表

| 模块 | 所有权(只能改这些) | 依赖 |
|---|---|---|
| M0a 后端骨架 | 迁移 030、`nomifun-db`(models/repository 新增文件+mod 注册)、`nomifun-workshop/**`、`nomifun-creation/**`、`nomifun-app`(services/state/routes 三点)、`nomifun-gateway`(caps_workshop+registry)、根 `Cargo.toml` workspace 登记 | — |
| M0b 前端骨架 | `pages/workshop/{index.tsx,CanvasPage.tsx,api.ts,types.ts}`、`Router.tsx`、`Sider/**`(新 entry+挂载)、i18n 4 文件+2 index、`common.json` 仅加 siderSection key(如需) | 契约 §3/§4 |
| M1 画布内核 FE | `pages/workshop/canvas/**`、`CanvasPage.tsx`、`workshopCanvas.json` | M0 |
| M2 生成引擎 BE | `nomifun-creation/**` | M0 |
| M3 工坊域 BE 完善 | `nomifun-workshop/**`(缩略图/导入导出/GC) | M0 |
| M4 资产库 FE | `pages/workshop/assets/**`、`workshopAssets.json` | M0 |
| M5 图片编辑器 FE | `pages/workshop/editor/**`、`workshopEditor.json` | M0 |
| M6 模型管理扩展 | `nomifun-api-types`(能力枚举)、`nomifun-system`(分类建议)、`pages/modelHub/**`、`ui/src/common/types/provider/**` | M0 |
| M7 生成卡片 FE | `pages/workshop/generation/**` + `canvas/`内注册点(与 M1 协调的钩子文件) | M1,M2,M6 |
| M8 循环/对比/输出/分组 | `pages/workshop/canvas/nodes/**` 扩展 | M1,M7 |
| M9 画布助手+caps | `nomifun-gateway/caps_workshop`、助手面板 FE | M1,M3 |

跨模块公共文件(`api.ts`/`types.ts`)M0 后**冻结语义**:后续模块可追加(append-only),不得改既有字段。
