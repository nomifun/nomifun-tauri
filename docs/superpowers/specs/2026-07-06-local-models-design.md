# 「本地模型」领域架构设计(Local AI)

- 日期:2026-07-06
- 状态:设计定稿待评审(评审通过后按模块并行实施)
- 领域代号:`local-ai`(crate `nomifun-local-ai`,数据目录 `{data_dir}/local-ai/`,路由 `/api/local-ai/*`)
- 用户可见名:「本地模型」

---

## 0. 总判断(先回答"是否可行、架构上有什么问题")

**可行,且时机很好。** 三个关键事实(2026-07 调研坐实,来源见 §12):

1. **运行时层已经"轻量化成熟"**:llama.cpp 官方 `llama-server` 自 2025-12 起支持 **router mode**(单进程单端口、多 GGUF 模型按需加载、LRU 逐出),OpenAI 兼容 + `--api-key`,Vulkan 单二进制 ~10MB 覆盖 A/N/I 全部消费级显卡;stable-diffusion.cpp 官方支持 **Z-Image**(2025-12 起,4GB VRAM 可跑)并自带 HTTP server;语音侧 sherpa-onnx(Apache-2.0)官方 Rust 绑定可静态链出 20-40MB 的自建 sidecar。**全链 MIT/Apache,无 GPL 传染,不需要 Ollama。**
2. **仓库地基已相当完整**:`platform` 是自由字符串,未知值天然落 OpenAI 分支——**造一个 `platform='local'` 的 provider 行,会话/编排/IDMM/故障转移全部零改动可用**;子进程治理(Job Object/PDEATHSIG/CREATE_NO_WINDOW)、代理感知下载、镜像回退、原子落盘、127.0.0.1+token、领域路由/crate 范式全有成熟先例。
3. **真正要新建的收敛为四件**:GB 级下载器(现有下载全是整包进内存,无续传/校验/进度)、运行时监督器(实例生命周期+显存预算)、统一门面(固定接入点+受管 provider 行)、语音 sidecar(全仓语音是空白)。

**架构上的主要问题与对策**(详见 §10):动态端口 vs provider 行静态 base_url(→受管行 boot 对账)、显存争抢(→ResourceBroker+LRU+TTL)、Windows 后端碎片化(→Vulkan 默认+CUDA 可选)、下载可靠性(→多源镜像+续传+sha256)、桌面聊天用户图片目前并未内联给模型(→本设计一并补齐)。

---

## 1. 设计原则(不可妥协)

1. **零捆绑**:安装包不含任何运行时二进制与模型;运行时(llama-server/sd-server/nomi-speech)与模型全部按需下载。
2. **数据驱动、可扩展**:支持哪些模型由**远程目录(catalog)**决定,不硬编码;用户可自定义导入(HF/ModelScope 仓库或本地 GGUF)。
3. **OpenAI 兼容是唯一接入语言**:所有本地能力以 OpenAI 兼容面(chat/embeddings/images/audio)进入平台,复用既有 provider 体系,**不为"本地"发明第二套模型消费协议**。
4. **本地模型 = 一个受管 provider**:对会话/编排/IDMM/failover/创意工坊而言,本地与云只是不同的 provider 行,能力面完全同构——这是"少返工"的核心。
5. **进程必须可收尸**:一切 sidecar 走 `nomifun-runtime::Builder`(Job Object/PDEATHSIG,随主进程死),杜绝僵尸锁文件。
6. **中国网络友好**:ModelScope/hf-mirror 优先、canonical 兜底、env 覆写指向内网源(沿用 `NOMIFUN_CHROME_BINARY` 的优先级设计)。

## 2. 总体架构

```
                    ┌──────────────────────── NomiFun 后端(单进程)────────────────────────┐
 会话/编排/IDMM ──→ │  provider 体系(platform='local' 受管行,base_url=门面,boot 对账)     │
 创意工坊生成引擎 ─→ │  nomifun-creation MediaProvider: local_image 适配器(进程内 trait)    │
 渠道语音/伙伴TTS ─→ │  nomifun-local-ai crate                                              │
                    │   ├─ 门面 Facade  /api/local-ai/v1/{chat/completions,embeddings,     │
                    │   │                audio/transcriptions,audio/speech,models}          │
                    │   │   (长期 bearer token 鉴权;反代/翻译到下方实例;按需拉起+就绪等待) │
                    │   ├─ InstanceSupervisor(生命周期/健康/崩溃退避/闲置 TTL 停机)         │
                    │   ├─ ResourceBroker(RAM/VRAM 预算,跨运行时逐出协调)                  │
                    │   ├─ ModelStore(blobs+manifests,内容寻址去重,GC)                    │
                    │   ├─ Downloader(多源/续传/sha256/进度事件/磁盘预检)                   │
                    │   └─ Catalog(远程目录+内置快照兜底+自定义导入)                        │
                    └───────────┬──────────────────┬─────────────────┬─────────────────────┘
                        127.0.0.1:auto        127.0.0.1:auto      127.0.0.1:auto
                    ┌───────────┴────────┐ ┌───────┴─────────┐ ┌─────┴──────────────┐
                    │ llama-server       │ │ sd-server       │ │ nomi-speech(自建)   │
                    │ router mode        │ │ (sd.cpp 官方)    │ │ sherpa-onnx 静态链   │
                    │ chat/VL/embedding  │ │ 生图/编辑        │ │ ASR SenseVoice      │
                    │ 多GGUF LRU 按需载   │ │ Z-Image/SDXL/…  │ │ TTS Kokoro/Melo     │
                    └────────────────────┘ └─────────────────┘ └────────────────────┘
```

### 2.1 运行时家族(三个,各司其职)

| 家族 | 二进制 | 覆盖能力 | 进程形态 |
|---|---|---|---|
| **llama** | llama.cpp `llama-server` 官方预编译(锁版本) | 文本 LLM、视觉理解(mmproj)、embedding | **常驻单进程 router mode**:`--models-dir` + `/models/load\|unload` + `--models-max`(LRU);supervisor 叠加"全局闲置 TTL→停进程释放显存" |
| **sdcpp** | stable-diffusion.cpp `sd-server` 官方预编译(锁版本) | 文生图/图生图/局部重绘(Z-Image/Qwen-Image/SDXL/Flux…) | **按需常驻**:首个生图任务拉起(冷载 4-12s 摊薄),闲置 TTL 停;切模型=重启实例 |
| **speech** | **nomi-speech**(我们自建的 Rust sidecar:官方 `sherpa-onnx` crate 静态链 + axum) | ASR(SenseVoice-Small int8)、TTS(MeloTTS-zh_en / Kokoro) | 按需常驻,内存占用小;原生暴露 OpenAI `/v1/audio/transcriptions` + `/v1/audio/speech` |

自建 nomi-speech 的理由:调研确认**不存在非 Python 的现成 OpenAI 兼容语音 server**;sherpa-onnx 官方 Rust 绑定 2025 年已就位(第三方 sherpa-rs 已归档),静态链单可执行 +20-40MB,全 Apache-2.0。它作为独立 workspace bin crate 由我们 CI 按平台构建、随 release 发布、**运行期按需下载**(不进主安装包)。

### 2.2 后端变体策略(硬件适配)

- 变体维度:`os × arch × backend`。默认:**Windows/Linux→Vulkan**(单二进制覆盖 A/N/I 卡,解码速度接近 CUDA,MIT 干净)、**macOS→Metal**、无 GPU→CPU(AVX2)。
- **CUDA 为可选升级**(NVIDIA 用户在硬件面板一键切换):prefill/峰值更强,但引入 ~373MB cudart 包与 **NVIDIA EULA 再分发条款**(上线前法务过一遍;初期可让 CUDA 变体直接从 llama.cpp 官方 release 下载,规避我们再分发)。
- 运行时工件来源:目录里每个 runtime 版本声明多源 URL——**长期主选"我们 CI 重打包+签名"**(治 SmartScreen/杀软误报+镜像可靠性),官方 release 直链作兜底;`NOMIFUN_LOCALAI_RUNTIME_DIR` env 覆写支持内网离线部署。
- 版本管理:`{data_dir}/local-ai/runtimes/{family}/{version}/{variant}/` 并存,升级=下载新版目录+闲时切换,坏版本可即时回退(目录还在)。

## 3. 统一门面与受管 provider(平台接入的关键设计)

### 3.1 门面(Facade)

挂在主后端 router 的 `/api/local-ai/v1/*`,**独立 bearer token 域**(不走登录鉴权,照 public token 域的 `.nest()` 模式;禁 extract CurrentUser):

- `POST /v1/chat/completions`、`/v1/embeddings` → 反代 llama-server(流式透传 SSE);请求带 `model` 字段,router mode 自动按需加载。
- `POST /v1/audio/transcriptions`、`/v1/audio/speech` → 反代 nomi-speech。
- `GET /v1/models` → 汇总已安装可用模型(含能力标签),这让"拉取模型列表/协议探测/健康检查"现有机制原样可用。
- 生图**不走门面**(见 §5.2,进程内 trait 更优);后续若要给外部 MCP/agent 暴露 `/v1/images/generations` 翻译层,再按需加。
- 反代期间由 Supervisor 完成"实例未起→拉起→就绪探针→转发",首请求慢(冷启动秒级)但语义简单;门面对上游永远是"同步可用"。

token:首次启用领域时生成一枚**长期 token**(加密落 `{data_dir}/local-ai/config.json`),门面校验它;llama-server 的 `--api-key` 与 nomi-speech 的 token 每次启动随机(仅 supervisor 知道),外界只见门面。

### 3.2 受管 provider 行(零改动接入全平台)

系统自动创建并维护**一条** providers 行:

- `platform='local'`(自由字符串,未知值天然落引擎 OpenAI 分支——勘察坐实 `map_nomi_provider` 的 `_ => "openai"`)、`name='本地模型'`、`api_key`=门面长期 token(既有 AES-GCM 加密存储)、`base_url=http://127.0.0.1:{port}/api/local-ai`(引擎默认补 `/v1/chat/completions`,恰好命中门面路径)。
- **动态端口对策**:主后端 loopback 端口每次启动漂移 → **boot 时对账受管行 base_url**(providers 表支持 partial update;desktop/webui 单实例由 server.lock 保证不打架)。
- 行上的 `models[]` = 已安装且能力为 chat/vision/embedding 的本地模型 id,安装/卸载时同步;能力标注直接来自**目录元数据**(比名字启发式更准,例如 Qwen3-VL 标 Vision)。
- 保护:`nomi_system_update/delete_provider` 与 Model Hub 对该行只允许启停模型,不允许改 base_url/删除(注册进 provider 守卫;删除=在「本地模型」页关闭领域)。

**由此白拿的能力**:会话模型选择器直接出现本地模型;编排/协作模型可选本地;IDMM 可用本地模型值守;故障转移队列可配"云挂了切本地";健康检查走既有 chat 探测;per-companion 能力收窄照常生效。

## 4. 模型目录、存储与下载

### 4.1 Catalog(目录)

- 远程 JSON(带 schema 版本+签名校验),托管于我们的 release/CDN,**应用内置一份快照兜底**(离线也能看目录);TTL 缓存刷新。
- 条目 schema(核心字段):

```json
{
  "id": "qwen3.5-4b-instruct",
  "display_name": "Qwen3.5 4B", "modality": "chat", "runtime": "llama",
  "license": "Apache-2.0", "context": 262144,
  "variants": [{
    "quant": "Q4_K_M", "quality": "balanced",
    "requirements": { "ram_mb": 4200, "vram_mb": 3200, "vram_optional": true },
    "files": [{
      "role": "main", "size": 2740000000, "sha256": "…",
      "sources": ["modelscope://…", "hf-mirror://…", "hf://unsloth/Qwen3.5-4B-GGUF/…"]
    }]
  }],
  "defaults": { "ctx": 8192 }, "tags": ["中文", "推荐"]
}
```

- `files[].role` 支持 `main | mmproj | vae | text_encoder | voices | config`——覆盖 VL 双件套(主模型+mmproj)、Z-Image 三件套(扩散 GGUF+Qwen3-4B 文本编码器+VAE)、TTS 音色包。
- **首发目录建议**(全部 Apache-2.0/MIT,均已核实可得):文本 Qwen3.5-4B(Q4_K_M≈2.7GB);视觉理解 Qwen3-VL-2B/4B(+mmproj,CJK OCR 最强);生图 Z-Image-Turbo(GGUF,4GB VRAM 档)+ SDXL-Turbo 兜底;ASR SenseVoice-Small int8(≈250MB);TTS MeloTTS-zh_en 与 Kokoro;embedding 一枚(如 bge-m3 GGUF)。
- **自定义导入**(可扩展性的兑现):粘 HF/ModelScope 仓库地址→解析 GGUF 清单→选量化→走同一下载管线(无官方 sha256 则下载后自算记录+明示"未经目录校验");或直接导入本地 GGUF 文件(复用浏览器下载沙箱的 magic-bytes 嗅探+可执行 denylist 防伪装)。

### 4.2 ModelStore(内容寻址,Ollama 式)

```
{data_dir}/local-ai/
  blobs/sha256-<hex>            # 权重文件,内容寻址,跨模型去重
  manifests/{model_id}.json     # "配方":引用 blobs + 参数默认值 + 能力标签
  runtimes/{family}/{ver}/{variant}/
  config.json                   # 领域配置(token/后端变体/预算/目录源)
  state/                        # 实例运行态、下载任务断点(全文件化,零 DB 迁移)
```

- 去重收益真实存在:多量化并存、Z-Image 的 Qwen3-4B 编码器与其它图像模型共件、mmproj 复用。
- GC:删除模型=删 manifest,blob 引用计数为零且过宽限期才删文件(借鉴创意工坊 GC 的宽限期教训)。
- 存储可迁移:设置里改 local-ai 根目录(大模型放机械盘的刚需),迁移=复制+校验+原子切 config。
- **不新增任何 SQLite 迁移**:该域状态全文件化(manifest/state.json 原子写),与 DB 解耦,少一类返工面。

### 4.3 Downloader(必须新建的一块)

现有下载先例(CfT/MODNet/bun)都是整包进内存,GB 级必须新写,但风格全部沿用既有范式:

- **流式落盘** `.part` + rename(原子);**HTTP Range 断点续传**(跨应用重启,断点态存 state/);
- **sha256 流式校验**(写法抄 `nomifun-runtime/build.rs`,唯一带校验的先例);
- **多源顺序回退**(ModelScope/hf-mirror 优先、hf 兜底——抄 matting_model 的 UPSTREAMS 语义),单文件失败换源续传;
- 代理:统一 `nomifun_net::http_client()`;connect 超时 15s、**总传输不封顶**(matting 的既定语义);
- 并发闸(同时 ≤2 文件)、磁盘预检(剩余空间<所需×1.2 拒绝并提示)、单飞锁防重复;
- **进度事件**:复用既有 WS 事件通道推 `{model_id, file, received, total, speed}`,前端进度条/暂停/取消。

## 5. 能力接入设计(逐能力)

### 5.1 文本/视觉理解/embedding —— 全靠受管 provider,零新面

如 §3.2。视觉理解补充:llama-server 的 OpenAI vision 消息格式(`image_url` data URI)与引擎现有 Image 块序列化兼容。

### 5.2 生图 —— 进创意工坊生成引擎,不绕 HTTP 弯路

`nomifun-creation` 新增适配器 `local_image`(能力 t2i/i2i/inpaint):它不发外网 HTTP,而是通过 app 层注入的 `LocalImageBackend` trait(仿 AssetSink 防环模式)调用 `nomifun-local-ai` 的 Supervisor→sd-server。生图模型在创意工坊「生成卡片」的模型选择器中与云模型并列(provider=本地模型行,capability=image_generation 来自目录)。**依赖:`feat/creative-workshop` 先合入 main。**

### 5.3 ASR —— 三个消费点

1. **IM 渠道语音消息转写**(现状:Telegram 只标记 `Voice` 不转写——纯空白即纯增量):渠道插件下载语音→门面 transcriptions→转写文本注入伙伴回合(带「语音转写」标记);
2. **桌面聊天语音输入**:输入框加录音按钮(FE MediaRecorder)→门面→回填输入框;
3. 创意工坊音频(P2 预留)。

### 5.4 TTS —— 伙伴开口说话

伙伴气泡/对外伙伴回复可选播报:回复文本→门面 speech→前端播放。音色=目录里的 voices 资产,按需下载。

### 5.5 视觉桥(用户点名的体验场景)+ 用户图片内联补齐

勘察发现一个重要现状:**桌面 nomi 会话的用户图片目前并不内联给模型**(`SendMessageData.files` 是路径,引擎只吃文本;内联 Image 块只来自工具结果截图)。因此设计两件事,一并做:

1. **补齐用户图片内联链路**(独立价值,云 VL 也受益):conversation service 发送预处理读取 files 中的图片字节→构造 Image 块进用户回合(受 `supports_image` 门控,大小/数量限幅)。
2. **视觉桥**:发送预处理中,若目标模型无视觉(capabilities/VisionUnsupportedRegistry)且本轮含图片→用**本地 VL 模型**(设置里指定,默认 Qwen3-VL-2B)经既有 `one_shot_completion` 生成结构化描述→以文本前置注入(与 knowledge prelude 同构),UI 标注「图片已由本地视觉模型转述」。模式三挡:关闭/自动(仅无视觉模型时)/总是。
   - 注入点定为 **conversation service 预处理层**(勘察结论:它同时拥有 provider 上下文、视觉判定与 one-shot 能力;不放 engine middleware——provider 层是无状态单请求转换器,拿不到"另起 VL 调用"的依赖)。

## 6. InstanceSupervisor 与 ResourceBroker

- **实例状态机**:`stopped → starting(下载缺件→spawn→就绪探针) → ready → idle(TTL 计时) → stopping`;崩溃→退避重启(1s/5s/30s,三次进 `failed` 并缓存失败态,学 browser_fetcher 不无限重拉);全部经 `nomifun-runtime::Builder`(Job Object/CREATE_NO_WINDOW),端口用 `bind_with_fallback` 语义取临时口。
- **ResourceBroker**:硬件探测(总/可用 RAM;VRAM 经 Vulkan 枚举,探不到则保守按 RAM 模式);每模型内存需求来自目录 `requirements`;加载前预算检查,不足时按策略逐出——llama 内部靠 router LRU(`--models-max` 由预算折算),跨家族(要开 sd-server 而 llama 占满)由 broker 令 llama 卸载模型或停机;**用户可 pin 常驻**(pin 的不逐出,预算不够直接明示)。并发钳制:llama `--parallel` 默认 1-2(KV cache×并发×上下文是 OOM 主因——Ollama 的教训),生图任务经创意工坊队列天然串行。
- **配置走文件不走 env**(Ollama 守护进程 env 不可见的教训):所有实例参数(ctx/gpu-layers/TTL/pin)入 config.json,UI 改即热重载。

## 7. Model Hub「本地模型」区(UI)

与「创作模型」并列的新 section:

1. **硬件面板**:检测到的 GPU/VRAM/RAM、当前后端变体(Vulkan/CUDA/Metal/CPU)、切换与重新下载运行时;
2. **模型目录**:按能力标签(对话/视觉/生图/语音…)浏览,每项显示体积/许可证/需求,并给出**本机适配徽标**("流畅/勉强/不可用",目录 requirements × 硬件面板即时计算);一键下载;
3. **下载管理**:进度/速度/暂停/续传/换源/取消;
4. **已安装**:启停/pin 常驻/每模型参数(ctx、gpu offload=auto)/在哪些入口可见(即受管行 models 勾选)/删除;实例实时状态(RAM/VRAM 占用、最近日志);
5. **存储**:总占用、按模型明细、GC、迁移目录;
6. **自定义导入**入口。
会话/工坊侧不需要新 UI——本地模型自然出现在既有模型选择器里(这正是架构的验收标准)。

## 8. 安全设计

- 全部实例只 bind `127.0.0.1`;llama-server `--api-key`/speech token 每启动随机,仅 supervisor 持有;对外唯一入口是门面(长期 token,加密落盘)。
- LAN WebUI 场景:门面与主 router 同进程,远程浏览器登录后即可经门面用本机模型(顺带兑现"局域网共享本地算力"),`host_guard` 防 rebinding 照旧。
- 下载完整性:目录条目强制 sha256;运行时二进制优先我们 CI 签名重分发;自定义导入走 denylist+magic 嗅探并明示风险。
- 模型许可证在目录与 UI 明示(运行时归运行时、模型归模型)。

## 9. crate 与代码布局

```
crates/backend/nomifun-local-ai/        # 领域 crate(public-agent 范式)
  lib.rs(常量/目录规划) catalog.rs  store.rs(blobs/manifests/GC)
  download.rs(流式/续传/校验/进度) hardware.rs(探测) broker.rs
  supervisor.rs(实例状态机) runtimes/{llama.rs,sdcpp.rs,speech.rs}
  facade.rs(门面路由) provider_sync.rs(受管行对账) config.rs state.rs
apps/nomi-speech/                        # 自建语音 sidecar(独立 bin,CI 单独出工件,不进主包)
ui/src/renderer/pages/modelHub/localAi/  # UI 区
```

装配:services.rs 启动 LocalAiService(懒:未启用领域则零开销)、routes.rs 挂门面(独立 token 域)、gateway 可选 `caps_local_ai`(agent 查询/触发下载,后续)。

## 10. 风险清单与对策

| 风险 | 对策 |
|---|---|
| Vulkan prefill 慢于 CUDA(长上下文摄入) | 默认 Vulkan 保覆盖;NVIDIA 用户硬件面板一键换 CUDA 变体;目录首发以 ≤4B 模型为主,prefill 压力小 |
| 显存争抢(LLM+生图并存) | ResourceBroker 跨家族逐出 + pin 语义 + 加载前预算硬检查;并发钳制 |
| router mode 较新(2025-12) | 锁定 llama.cpp 版本于目录;版本目录并存可即时回滚;上线前复核流式+工具调用兼容性(调研标注的待验证项) |
| 杀软/SmartScreen 误报下载的 exe | CI 重打包+代码签名主选;MOTW 处理;官方直链兜底 |
| CUDA 再分发的 NVIDIA EULA | 初期 CUDA 变体从官方 release 直下(不经我们分发);法务确认后再纳入重分发 |
| 磁盘被模型吃爆 | 预检+存储面板+GC+可迁移目录(承接 windows-disk-hygiene 的教训) |
| Z-Image 三件套内存叠加(扩散+4B 编码器+VAE) | 目录 requirements 按三件套合计标注;`--offload-to-cpu`+FA 默认开;4GB VRAM 档验证过(官方 wiki) |
| sherpa-onnx 官方 Rust 绑定较新 | 锁版本+关键路径自测;nomi-speech 是薄壳,替换引擎(如未来 SenseVoice-on-llama.cpp 坐实)不影响门面契约 |
| 桌面用户图片内联是新链路 | §5.5 拆成独立模块,云 VL 同步受益,单独可测 |

## 11. 与既有工作的关系 & 模块拆分(实施并行用)

- **依赖 `feat/creative-workshop` 合入**:仅 §5.2 生图适配器;其余模块与该分支无耦合,可先行。
- 模块(所有权互斥,可并行):M-A 目录+ModelStore+Downloader → M-B Supervisor+Broker+llama 家族 → M-C 门面+受管行对账(主干串行);M-D sdcpp+local_image 适配器、M-E nomi-speech+audio 面、M-F Model Hub UI、M-G 图片内联+视觉桥、M-H 渠道语音转写(依赖 A/B/C 后并行)。

## 12. 决策记录(为什么不是……)

- **不是 Ollama/LM Studio 捆绑**:违反零捆绑;其守护进程/存储/升级节奏与产品耦合差;但**借鉴**其 blob+manifest 去重与 TTL/LRU/并发钳制经验。
- **不是 candle/mistral.rs 内嵌进主二进制**:模型架构跟进速度远慢于 llama.cpp 生态;CUDA 构建复杂;主包体积膨胀;mistral.rs 图像仅 FLUX 且 ~24GB 显存,不合消费级。
- **不是 Python 栈(ComfyUI/vLLM/faster-whisper)**:违反禁重依赖;这类需求已由"云 provider/外部端点"路径覆盖(用户自架 ComfyUI 可作为普通 OpenAI 兼容/后续 comfy 适配器接入创意工坊,见其 PRD P2)。
- **不自研多模型代理**:llama-server 官方 router mode 已覆盖"单端点+按需加载+LRU",自研 llama-swap 类代理是重复建设。
- **调研关键来源**:llama.cpp releases/server README/multimodal.md、Qwen3.5-4B GGUF(unsloth/lmstudio-community,Apache-2.0)、Qwen3-VL-2B/4B 官方 GGUF、sd.cpp README/Z-Image 4GB wiki(2025-12)、sherpa-onnx 官方 crate/docs.rs、SenseVoice 基准、Kokoro Apache-2.0、piper 归档转 GPL(弃用依据)、Ollama FAQ/LM Studio TTL 文档。
