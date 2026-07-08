# 《创意工坊》产品功能需求文档(PRD)

- 日期:2026-07-05
- 状态:待评审(评审通过后进入分模块并行开发)
- 领域代号:`workshop`(前端路由 `/workshop`,后端 crate `nomifun-workshop` + `nomifun-creation`,数据目录 `{data_dir}/workshop/`)
- 中文名:创意工坊;英文名:Creative Workshop

---

## 0. 背景与目标

用户希望在 NomiFun 中新增一个一等创作领域「创意工坊」:以**无限画布**为中心的 AI 视觉创作工作台,能力对标开源项目 infinite-canvas 及其演示口播稿(介绍v1/介绍v2)所展示的完整功能面,同时用 NomiFun 已有的平台能力**重做参考项目最薄弱的两块:资源(资产)管理与模型管理**。

一句话定位:**在一张无限大的画布上,用节点和连线组织素材与 AI 生成,图片/视频/文本混排创作,支持批量、循环、连续编辑,全部产物沉淀进统一资产库。**

### 0.1 素材来源与口径

本 PRD 综合三路输入:

1. **口播稿 v1/v2**(`C:\Users\rika0\Desktop\wuxianhuabu\介绍v1.md`、`介绍v2.md`):产品能力的目标态描述。
2. **参考项目代码勘察**(`download/infinite-canvas`,实际版本 v0.5.0+):已验证的成熟能力与数据模型形态。
3. **本仓库勘察**:可复用的平台能力(provider 平台、react-flow 画布经验、数据目录/迁移/网关惯例)。

**关键事实(勘察坐实)**:口播稿描述的部分明星功能——**循环节点、智能画布(免连线智能卡片)、资产库@引用、ComfyUI/ModelScope/RunningHub 接入、宫格切分(自定义线/去缝)、扩图、对比节点**——在下载的这份代码里**全部不存在**(全文检索零命中,官方 TODO 也未提及)。口播稿对应的是另一条产品线/另一版本。因此这些功能**没有参考实现,属于我们的原创设计**;下载仓库可对标的是画布内核、节点/批次模型、图片编辑基础件、画布助手等。

---

## 1. 合规红线:净室开发守则(不可妥协)

参考项目为 **AGPL-3.0**,作者在文档中明确:禁止闭源商用、网络服务也触发开源义务、且主张"换框架重写也不能脱离关系"。结合既有调研结论(源禁商用须净室重写、选 react-flow),规则如下:

1. **实现阶段任何 agent 禁止读取 `download/infinite-canvas` 目录下的任何文件。** 唯一输入是本 PRD 与后续设计文档(勘察报告已把能力转写为功能描述,不含源码)。
2. 独立的命名体系、数据结构、UI 设计、文案。禁止复刻参考项目的字段名、组件结构、CSS、文案措辞。本 PRD 中出现的参考项目字段名仅用于说明"它是什么形态",我们的 schema 在 §6 独立定义。
3. 技术选型天然隔离:参考项目是自研 DOM 画布 + IndexedDB + antd;我们是 react-flow + SQLite/文件系统 + Arco + 自有 provider 平台。
4. 提示词库若做(P2),数据源许可证需逐一自查,不照抄其源清单。

---

## 2. 口播稿能力总结(目标能力全集)

以下是两篇口播稿的功能提炼,按主题归并,并标注「参考实现:有/无」。

### 2.1 无限画布核心交互
- 平移/滚轮缩放(鼠标锚点)/缩放滑杆/重置视图/小地图/明暗主题/点阵网格背景。(参考:有)
- 框选、多选、全选;**Ctrl+G 打组、Ctrl+Shift+G 解组,分组内统一管理、可对分组整体编辑**;分组转"输入组"连给生成节点。(参考:无,原创)
- 连线:锚点拖拽建线;**拖到空白弹快捷菜单直接建下游节点并自动连线**;连线高亮相关上下游;**Shift+划线快速断开**。(参考:前两项有,划线断开无)
- 拖入图片/视频直接建节点;粘贴板粘贴;节点内多图左右拖动排序、拖出删除、拖入替换。(参考:拖入/粘贴有,节点内排序无)
- 复制/粘贴(含内部连线)、删除、Esc、撤销/重做。(参考:有,快照式)
- **全局预览快捷键**(按 Z 俯瞰全局)。(参考:无)
- 大量节点不卡(视口裁剪等性能手段)。(参考:有,DOM 方案;我们用 react-flow `onlyRenderVisibleElements` + 既有 DagCanvas 调优经验)
- 日志面板:运行记录与报错,便于排查。(参考:弱)

### 2.2 生成能力
- **文生图/图生图/参考图编辑**:提示词+多参考图,模型/尺寸(含 2K/4K 与比例预设)/质量/数量(批量出图成"批次组",可展开/收起/设主图)。(参考:有)
- **视频生成**:文生视频/图生视频;多图输入+编号提示词("图一的人运动到图二的场景");时长/比例/分辨率;**首尾帧标记且可拖动调换**;参考视频(视频生视频);生成声音/水印开关。(参考:OpenAI 视频协议+火山 Seedance 有;首尾帧标记无)
- **LLM 文本节点**:多平台模型问答、识图(VL)、改写扩写、**聊天模式带上下文**;**反推提示词**。(参考:有)
- **工作流串联**:生成节点首尾相连成链(A 出图→B 二次放大),链中间可插**输出节点**查看中间结果;**对比节点**(双击调出,before/after 对比)。(参考:链式引用有,输出/对比节点无)
- **循环节点**(口播稿的核心差异化,原创设计):
  - 参数:起始计数(从第几个输入开始)、批次(每轮取几个输入)、次数(共几轮)、模式(循环=串行 / 并发=并行)。
  - **计数变量注入提示词**:模板里插入"当前轮次号",每轮自动更新(如"现在生成第 {n} 张卖点图"),预览可见。
  - 多提示词并联:多个提示词源与轮次轮转匹配。
  - 拖放到目标节点即自动连接、连接态高亮、运行动画、一键运行整条链。
  - 典型用例:一张 4K 十宫格卖点图切分后逐张放大;LLM 产出 10 段描述循环生成 10 张同风格图。
- **智能生成卡片**(口播稿 v2 的"智能画布",原创设计):免连线的一体化卡片——卡片内直接选平台/模型/参数、写提示词、@引用参考,点击运行;**直觉连续编辑**:对结果图直接说"变为夜晚"继续编辑,形成编辑链,操作顺滑无节点噪音。

### 2.3 图片编辑器(双击图片进入)
- **裁剪**:自由/锁比例,8 向手柄。(参考:有)
- **遮罩局部重绘(inpaint)**:画笔/擦除、笔刷大小、涂抹区域+修改要求→局部编辑。(参考:有)
- **画笔精准区域**:框选/圈选某处让 AI 单独修改(遮罩的变体交互)。(参考:部分)
- **宫格切分**:行×列等分;**间隔(gap)裁掉接缝**;**自定义分割线**(任意位置加垂直/水平线、可拖动微调、每条线带间隔)——切分结果成为一组图片节点。(参考:仅等分;间隔与自定义线为原创)
- **扩图(outpaint)**:向外拖拽扩展画布,实时显示目标分辨率,填提示词生成扩展区域。(参考:无,原创)
- **放大/超分**:本地插值放大(1K/2K/4K)与 AI 超分(走生成通道)。(参考:本地插值有)
- 预览(缩放/拖动/左右切换)、下载、存入资产库、复制提示词、查看生成信息。(参考:有)

### 2.4 资产库(口播稿 v2 重点,参考实现薄弱→我们重点强化)
- 快捷键(A)呼出;上传图片/视频(拖入即传);重命名;**按角色/场景等集合分类**+自由标签;搜索/筛选/分页。
- **@ 引用**:画布任意生成卡片/提示词框内 `@资产名` 直接把资产作为参考输入,**免连线**;也可 @ 上游全部图片。
- **序号打标**:提示词中"图一/图二"自动对应输入序号,且可把序号映射为语义名(如"图一=武松、图二=老虎"后直接用语义名写提示词)。
- 全局跨画布共享;资产可直接拖入画布成节点;导入导出(zip)。
- 生成产物自动登记来源(prompt/模型/参数溯源)。

### 2.5 模型与平台管理(参考实现簡陋→我们用 NomiFun 平台重做)
- 多平台渠道管理:Base URL + Key(多 key)+ 协议;**验证地址/验证协议**(自动探测 OpenAI 直连 vs 异步任务协议 vs Gemini vs 火山);**拉取模型**一键导入;推荐平台一键填写。
- **模型自动分类**(生图/视频/LLM/音频)+ 手动修正归组。
- 协议面:OpenAI 兼容(同步 images)、**通用异步任务协议**(提交→轮询,断网/审核失败不扣费)、Gemini、火山方舟(Seedream/Seedance)。目标"兼容市面九成 API 平台"。
- 三种生成模式生态:API 平台(便宜/并发)、ModelScope(免费/Lora/VL)、本地 ComfyUI(自定义工作流、暴露参数)。(分级见 §5)

### 2.6 画布助手(AI 操作画布)
- 侧栏助手:围绕选中节点与上游上下文对话、生图,结果自动插回画布(建节点/连线/触发生成),写操作可开确认闸,支持撤销。(参考:有,双宿主 Agent 架构)
- 我们的实现直接复用 NomiFun 内建 agent 体系(Nomi + 网关能力域),**不需要**参考项目那种本机边车进程。

### 2.7 明确不做/放弃(弃其糟粕)
- ❌ 浏览器 IndexedDB 存储、WebDAV 手动同步(last-write-wins、删除复活):我们用后端 SQLite+文件系统,天然多端(桌面/WebUI LAN)。
- ❌ 账号/算力点计费、管理后台(参考项目已自砍)。
- ❌ 本机 canvas-agent 边车 + Codex 插件:NomiFun 内建 agent 直连,无需边车。
- ❌ 纹身图/细节增强/角度控制等独立网页小工具:本质是预置工作流,画布内即可完成,不单独做页面。
- ❌ 一键更新/网盘启动等发行杂项:NomiFun 有自己的发行体系。
- ❌ 多人实时协同编辑(参考项目实际也没有):v1 不做,预留单写者+观看者模式(P2)。

---

## 3. 需求分级

### P0(可用基线,一次交付的主体)
1. 画布项目管理:列表画廊(缩略图)、新建/重命名/删除、导入导出(zip:画布 JSON+媒体文件)。
2. 画布内核:平移/缩放/框选/多选/拖拽/连线/快捷建节点菜单/右键菜单/复制粘贴/删除/撤销重做(快照式,≥50 步,拖拽合并)/小地图/缩放滑杆/背景网格/明暗主题/快捷键帮助浮层/视口持久化。
3. 节点体系:图片节点、文本节点、视频节点(承载+播放)、**生成卡片**(图/文模式)、批次组(多图展开/收起/设主图)。
4. 生成执行:文生图/图生图/参考图编辑/LLM 文本;同步(OpenAI images)+ 通用异步任务 + Gemini 三种协议适配;**后端任务队列**(状态机/并发闸/取消/失败详情/boot 对账);产物落盘+资产登记;前端节点状态(idle/loading/success/error)+日志面板。
5. 模型管理:复用 providers 平台;新增媒体能力标注(图像生成/视频生成)与媒体协议标注;Model Hub「创作模型」分组与按能力筛选;拉取模型/协议探测/健康检查沿用;模型名启发式自动分类+手动修正。
6. 图片编辑器:预览、裁剪、遮罩局部重绘、宫格切分(等分+**间隔去缝**)、本地放大、下载/存资产库。
7. 资产库 P0:上传/管理(标签+搜索+分页)、拖入画布、**@ 引用**(生成卡片内 @ 资产/画布节点免连线)、生成产物自动入库(可关)、GC。
8. 门槛:UI 必须漂亮(frontend-design 流程)、i18n 双语、typecheck 0、后端 nextest 全绿。

### P1(完整版差异化能力,同一工期内完成)
1. **循环节点**全套(计数/批次/串行/并发/计数变量注入/多提示词并联/一键运行链)。
2. **视频生成**:OpenAI 视频协议 + 火山方舟 Seedance(参考图/参考视频/首尾帧标记可调换/时长/比例/声音/水印);视频上传与视频生视频。
3. 智能卡片**直觉连续编辑链**(结果图上直接二次指令,自动串链)。
4. 图片编辑器增强:**自定义分割线切分**、**扩图(outpaint)**、画笔精准区域、AI 超分、反推提示词。
5. **对比节点**(A/B 滑动对比)与**输出节点**(链中间产物落点)。
6. 分组(Ctrl+G/解组/分组整体作为输入组连给生成节点)。
7. 资产库增强:集合(角色/场景)、序号打标+语义名映射、导入导出、来源溯源详情。
8. **画布助手**:Nomi agent 经网关 `caps_workshop` 操作画布(读状态/建节点/连线/触发生成),写操作确认闸+撤销。
9. ModelScope 适配器(含 Lora 选择);多提示词/提示词模板变量。
10. LLM 聊天模式面板(节点放大即聊,带上下文)。

### P2(后续迭代,本期只留扩展点)
- 本地 ComfyUI 适配器(工作流上传→参数暴露勾选→简易测试→成为可选"工作流模型")。
- RunningHub 等工作流云平台适配器。
- 音频节点与 TTS。
- 提示词库(开源提示词源+缓存,许可证自查)。
- 工作流模板化(把一段链保存为模板/资产,@ 调用)。
- 多端观看/单写锁;移动端触控。
- LLM 辅助模型自动分类。

---

## 4. 与 NomiFun 平台的关系(改进两大短板的方案)

### 4.1 模型管理:不再另造轮子,长在 providers 平台上
参考项目的渠道管理只有 `ModelChannel{baseUrl,key,models[]}` 级别。NomiFun 已有完整平台:`providers` 表(AES-256-GCM 密钥加密、多 key、模型目录/能力/协议/健康 per-model map)、模型拉取(`ModelFetchService`+URL 自动纠错)、协议探测(`detect-protocol`)、健康检查、故障转移队列。**创意工坊全部复用**,只补:

- `ModelType` 能力枚举已有 `ImageGeneration`(从未被消费),新增 `VideoGeneration`(P1)、`AudioGeneration`(P2);`infer_model_modalities` 的名字启发式扩展为四类分类建议,用户可改。
- providers 增加(或经 `model_protocols` 扩展)**媒体调用协议**标注:`images-sync`(OpenAI /images)、`media-async`(通用提交-轮询)、`gemini-image`、`ark`(火山)、后续 `comfyui`/`runninghub`。协议探测端点扩展出媒体协议探测(打一次轻量请求判别)。
- Model Hub 前端加「创作模型」视图:按能力(生图/视频/LLM)分组展示、勾选进创意工坊可用清单、每模型默认参数(尺寸/质量/时长等)。

### 4.2 执行链:新建生成引擎(现有引擎是 chat-only)
勘察坐实:现有 `LlmProvider::stream` 全链无任何图像/视频产物路径、无异步任务队列、无产物资产层。新建独立 crate **`nomifun-creation`**:

- `MediaProvider` trait:`submit(task) / poll(task_id) / fetch(result)`,能力声明(t2i/i2i/inpaint/t2v/i2v/v2v/tts)。适配器:OpenAI-images、通用异步任务、Gemini(P0);Ark 火山、ModelScope(P1);ComfyUI、RunningHub(P2)。
- **任务队列**:`creation_tasks` 表(状态机 queued/running/succeeded/failed/canceled、参数快照、结果引用、错误详情、耗时);每 provider 并发闸+限流;取消传播 AbortController→HTTP;**boot 对账**(沿用 orchestrator "running⟺活体轮询器"不变量);事件推送前端(任务状态/节点状态联动)。
- 产物:落 `{data_dir}/workshop/assets/`,登记 `workshop_assets`,serve 路由供前端与导出使用。

### 4.3 资产管理:后端权威 + DB 索引(参考项目是浏览器本地扁平列表)
元数据入 SQLite(集合/标签/来源/尺寸/mime/rel_path),二进制落 data_dir,缩略图生成,引用计数 GC,跨画布共享天然成立;比参考项目多出:集合体系、@ 资产引用、来源溯源、与画布导入导出联动。

---

## 5. 技术方案概要(净室,全部 NomiFun 惯例)

### 5.1 前端
- 画布引擎:`@xyflow/react` v12(已在依赖)。直接继承 DagCanvas 的全部踩坑经验:主题镜像(`data-theme`→JS hex,MutationObserver)、`nodeTypes` 冻结常量、`initialWidth/Height` minimap 修复、节点对象 identity 缓存防 handleBounds 重置、`ResizeObserver` refit、`proOptions.hideAttribution`。新开启:`nodesConnectable`、自由拖拽写回坐标、`onlyRenderVisibleElements`。
- 页面:`pages/workshop/`(列表 `/workshop` + 编辑器 `/workshop/:id`);侧栏 `SiderWorkshopEntry`(仿 `SiderKnowledgeEntry`),放「常用」段;i18n 新增 `workshop.json`(zh-CN/en-US)+ `gen:i18n`。
- 组件红线:Arco `useArcoMessage`、`<div onClick>` 代 `<button>`、icon-park 具名导入禁别名、UnoCSS 语义类、禁 any/ts-ignore。
- 编辑器(裁剪/遮罩/切分/扩图)为纯前端 Canvas2D 组件族,独立于画布内核,可并行开发。

### 5.2 后端
- **`nomifun-workshop`**(域 crate,完全对标 `nomifun-public-agent` 范式):`service.rs`(start(data_dir))、`fsio.rs`(原子写)、`routes.rs`(`/api/workshop/*`:画布 CRUD/资产 CRUD/媒体 serve/导入导出)、`state.rs`;画布正文 `canvas.json` 文件存储(原子写+损坏回退),`workshop_canvases` 轻索引表。
- **`nomifun-creation`**(生成引擎 crate):§4.2。
- 迁移:`032_workshop.sql`(`workshop_canvases`、`workshop_assets`、`creation_tasks`;主键字符串 uuidv7、时间戳 ms;检查 pre_baseline 是否需 bump;提交前 `git pull --rebase` 防撞号)。
- 装配:`nomifun-app` services/state/routes 三点接入;provider 删除清理登记 `provider_deletion.rs`。
- 网关:新增 `caps_workshop` 域(3 步契约+CI 守卫):读画布状态/应用操作(建节点/连线/改参数/触发生成)/查任务/取产物,供画布助手与桌面伙伴调用。

### 5.3 数据模型(独立设计,示意)
- `workshop_canvases(id, title, thumbnail_rel_path, node_count, created_at, updated_at)`;正文 `{data_dir}/workshop/canvases/{id}/canvas.json`(nodes/edges/viewport/settings,schema 版本号字段)。
- 节点(前端 schema):`{ id, kind: 'image'|'text'|'video'|'generator'|'loop'|'compare'|'output'|'group', x, y, w, h, data }`;生成卡片 `data`:`{ mode, providerModelRef, params, prompt, mentions[], status, taskId?, resultAssetIds[], batch{...} }`。
- `workshop_assets(id, kind, title, collection, tags_json, rel_path, thumb_rel_path, mime, width, height, bytes, origin_json(prompt/model/params/canvas_id), created_at, updated_at)`。
- `creation_tasks(id, canvas_id?, node_id?, provider_id, model, capability, params_json, status, error_json, result_asset_ids_json, submitted_at, finished_at, attempt)`。

---

## 6. 模块拆分与并行开发规划

按「文件簇不相交则并行」原则(热点先串行钉死契约):

**M0 骨架(串行先行,钉死全部契约,消灭并行冲突面)**
一次性完成:迁移 030、两个新 crate 骨架、`nomifun-app` 装配、`api-types` DTO、前端路由/侧栏入口/页面壳、i18n 骨架、TS API 客户端契约、`caps_workshop` 空域注册。此后各模块只在自己目录内工作。

**并行簇(M0 后同时开跑,互不重叠)**
- M1 画布内核 FE(`pages/workshop/canvas/**`):交互全套+节点渲染框架+撤销重做+小地图。
- M2 生成引擎 BE(`nomifun-creation/**`):trait+三适配器+任务队列+对账。
- M3 工坊域 BE(`nomifun-workshop/**`):画布/资产 CRUD、serve、导入导出、GC。
- M4 资产库 FE(`pages/workshop/assets/**`):管理面板+@引用数据源。
- M5 图片编辑器 FE(`pages/workshop/editor/**`):裁剪/遮罩/切分/扩图/放大组件族。
- M6 模型管理扩展(`modelHub/**`+`nomifun-system` 小改+能力枚举):创作模型分组。

**集成簇(依赖前面,次轮并行)**
- M7 生成卡片+参数面板 FE(M1+M2+M6)。
- M8 循环节点/智能编辑链/对比/输出/分组(M1+M7)。
- M9 画布助手+`caps_workshop` 实装(M3+M1)。
- M10 收尾:联调、导入导出全链路、性能压测(500+ 节点)、opus 终评、全量测试、真机视觉验收。

评审规则:每模块完成即按 diff 规模评审;耦合热点(装配/契约文件)只在 M0/M10 动。

---

## 7. 验收标准
1. 口播稿 §2 中标注 P0/P1 的能力逐条可演示;宫格切分含间隔去缝、循环节点含计数注入与并发模式。
2. 500 节点画布平移/缩放不掉帧;生成任务并发 10 不阻塞 UI;重启后 running 任务正确恢复或收敛失败态。
3. 断网/审核失败的异步任务不产生半成品资产;取消即时生效。
4. `bun run typecheck` 0 错、`check:i18n` 过、后端全量 nextest 绿、UI 视觉达到「必须漂亮」门槛。
5. 全程未读取参考项目源码(实现 agent 的输入仅本 PRD 及派生设计文档)。

## 8. 开放问题(不阻塞开发,默认按推荐执行)
1. 侧栏位置:默认放「常用」段;如需独立分组再调。
2. 音频能力:默认 P2;若用户有 TTS 刚需可提前。
3. ComfyUI/RunningHub:默认 P2(接口已预留 MediaProvider 扩展点)。
