# 本地语音识别多引擎设计：FunASR

## 1. 背景与目标

当前本地语音识别由 `AsrModelService` 管理 Whisper GGML 模型，并通过
`whisper-cli` 单次进程完成转写。该实现部署简单，但中文普通话的准确率、
中文专有词表现和 CPU 推理效率不够理想。

本设计在不引入 Python、常驻服务或云端依赖的前提下，增加 FunASR 官方
llama.cpp/GGUF runtime，首期提供：

1. **Paraformer-zh Q8**：默认推荐，面向中文普通话和中英混说，CPU 速度快。
2. **SenseVoiceSmall Q8**：面向中文、英文及多语言，并为后续语言、情绪和
   音频事件标签保留扩展能力。
3. **FSMN-VAD**：两个模型共享，用于长音频切段。

首期不接入 PyTorch、ONNX Python 环境、Docker 服务或 Fun-ASR-Nano。
Fun-ASR-Nano 需要编码器和 Qwen 解码器两份模型，安装体积和内存需求更高，
适合作为后续“高精度中文”档位。

## 2. 设计原则

- **离线优先**：安装完成后，录音、音频和转写文本不离开本机。
- **零 Python runtime**：继续使用一次请求启动一个原生 CLI 的模式。
- **引擎隔离**：Whisper 和 FunASR 拥有独立 runtime，不能再假定全目录只有
  一个 `whisper-cli`。
- **制品不可变**：runtime 和所有 GGUF 均固定版本、commit、长度和 SHA-256。
- **共享依赖**：FSMN-VAD 只保存一份，但安装状态按模型正确计算。
- **兼容现有接口**：`/api/stt`、录音 WAV 归一化、当前模型选择配置保持兼容。
- **单活模型**：同一时间只启用一个本地 ASR 模型，切换模型不需要常驻加载。

## 3. 目录结构

```text
local-ai/asr/
  state.json
  downloads/
    runtimes/
    artifacts/
  runtimes/
    whisper-cpp/1.9.1/windows-x86_64/
    funasr-llamacpp/0.1.4/windows-x86_64/
  artifacts/
    whisper-small-q5-1/
      ggml-small-q5_1.bin
    whisper-large-v3-turbo-q5-0/
      ggml-large-v3-turbo-q5_0.bin
    funasr-paraformer-zh-q8/
      paraformer-q8.gguf
    funasr-sensevoice-small-q8/
      sensevoice-small-q8.gguf
    shared/
      funasr-fsmn-vad/
        fsmn-vad.gguf
  jobs/
```

从旧版 `runtime/windows-x86_64-1.9.1` 和 `models/<id>` 迁移时只移动已知、
已通过 SHA 校验的文件。未知文件、符号链接和重解析点不得递归移动或删除。

## 4. 核心数据模型

### 4.1 公开目录元数据

`AsrModelCatalogEntry` 增加：

```rust
pub enum AsrEngine {
    WhisperCpp,
    FunAsrLlamaCpp,
}

pub struct AsrModelCatalogEntry {
    // 现有字段……
    pub engine: AsrEngine,
    pub capabilities: Vec<AsrCapability>,
}

pub enum AsrCapability {
    Transcription,
    LanguageDetection,
    EmotionDetection,
    AudioEventDetection,
    LongAudioVad,
}
```

前端模型卡展示引擎标签和能力标签，不再把 runtime 文案写死为
`whisper.cpp`。

### 4.2 内部制品描述

当前 `AsrModelArtifact` 只有一个模型文件和全局 runtime。需要改为：

```rust
struct AsrModelArtifact {
    entry: AsrModelCatalogEntry,
    engine: AsrEngineKind,
    runtime: RuntimeRef,
    files: Vec<ArtifactFile>,
    shared_files: Vec<ArtifactFile>,
    invocation: AsrInvocation,
}

enum AsrInvocation {
    Whisper,
    FunAsrParaformer { vad: ArtifactRef },
    FunAsrSenseVoice { vad: ArtifactRef, keep_tags: bool },
}
```

每个文件包含固定 URL、SHA-256、字节数和相对安装路径。安装完成的判定是
该模型的全部私有文件、共享文件和对应 runtime 均存在且通过校验。

### 4.3 持久化状态

`state.json` 升级到 v2：

```json
{
  "version": 2,
  "installedModelIds": ["funasr-paraformer-zh-q8"],
  "activeModelId": "funasr-paraformer-zh-q8"
}
```

不持久化“runtime 已安装”或“共享 VAD 已安装”等派生状态，启动时从固定制品
重新计算，避免状态与磁盘不一致。v1 可无损迁移：保留原 Whisper 模型 ID 和
当前启用项。

## 5. Runtime 抽象

将转写执行从 `AsrModelService::transcribe` 中拆出：

```rust
#[async_trait]
trait LocalAsrEngine: Send + Sync {
    fn kind(&self) -> AsrEngineKind;
    fn runtime_ref(&self) -> RuntimeRef;
    fn supports_audio(&self, input: &NormalizedAudioInput) -> bool;
    async fn transcribe(
        &self,
        ctx: AsrExecutionContext<'_>,
    ) -> Result<LocalAsrTranscription, AppError>;
}
```

实现：

- `WhisperCppEngine`
- `FunAsrParaformerEngine`
- `FunAsrSenseVoiceEngine`

`AsrModelService` 继续负责目录安全、下载、校验、激活状态、互斥和任务清理；
engine 只负责构建受控参数、启动子进程和解析输出。

### 5.1 FunASR 命令

Paraformer：

```text
llama-funasr-paraformer
  -m <paraformer-q8.gguf>
  --vad <fsmn-vad.gguf>
  -a <input.wav>
```

SenseVoice：

```text
llama-funasr-sensevoice
  -m <sensevoice-small-q8.gguf>
  --vad <fsmn-vad.gguf>
  -a <input.wav>
```

首期不传 `--keep-tags`，只向聊天输入框返回干净文本。后续需要结构化标签时，
通过独立输出解析器和 API 字段启用，不能把 `<|zh|>`、情绪或事件标签直接
插入用户文本。

### 5.2 输出解析

FunASR CLI 当前把最终转写写到 stdout。解析规则：

1. 子进程退出码必须为 0。
2. stdout 以 UTF-8 解码，统一 CRLF/LF。
3. 去掉已知 runtime 日志行和首尾空白，但不任意删除中文标点。
4. 结果为空时返回 `ProviderUnavailable`。
5. stderr 只写入脱敏日志，不直接返回前端。
6. 增加固定输出样例的单元测试，并用真实 CLI 做集成冒烟测试。

如果上游增加机器可读 JSON 参数，应优先切换至 JSON，并固定支持的 runtime
版本，避免依赖人类可读输出长期不变。

## 6. 音频处理

前端录音已经转换为单声道 16 kHz PCM16 WAV，可直接复用。FunASR runtime
本身能读取 WAV、MP3、FLAC 等格式，但为了不同引擎行为一致：

- 实时录音：继续上传标准 WAV。
- 用户文件：后端首期沿用当前 WAV、MP3、OGG、FLAC 白名单。
- 若 FunASR runtime 对 OGG 的实际构建支持不稳定，统一在后端增加一次
  `NormalizedAudioInput` 转换，而不是在路由层按模型返回不同格式错误。
- `languageHint` 对 Paraformer 不生效；SenseVoice 首期自动检测。API 仍接受
  该字段，engine 可明确忽略。

## 7. 下载、校验和删除

### 7.1 安装事务

安装一个 FunASR 模型依次确保：

1. 对应平台的 `funasr-llamacpp` runtime。
2. 共享 `fsmn-vad.gguf`。
3. 目标 ASR GGUF。
4. 全部文件 SHA-256 校验。
5. 原子提交文件并激活模型。

进度组件建议扩展为：

```text
runtime | model | asr_auxiliary
```

其中 `asr_auxiliary` 用于 VAD。下载仍只允许一个 ASR 安装任务，支持断点续传。

### 7.2 引用计数

删除 Paraformer 时，如果 SenseVoice 仍安装，则保留 FSMN-VAD 和 FunASR
runtime。删除最后一个 FunASR 模型后：

- 默认保留小体积 runtime 和 VAD，以便快速重装；或
- 在“清理未使用组件”操作中删除。

模型删除接口不得隐式删除其他已安装模型依赖的共享文件。

### 7.3 自定义 runtime

保留 `NOMIFUN_WHISPER_CLI_PATH`，并新增：

```text
NOMIFUN_FUNASR_RUNTIME_DIR
NOMIFUN_FUNASR_PARAFORMER_CLI_PATH
NOMIFUN_FUNASR_SENSEVOICE_CLI_PATH
```

生产模式仍优先使用受校验的内置 runtime；环境变量只用于开发和诊断。

## 8. API 与 UI

现有 REST 路由不变：

```text
GET    /api/model-services/local/asr/catalog
GET    /api/model-services/local/asr/status
POST   /api/model-services/local/asr/models/{id}/install
POST   /api/model-services/local/asr/models/{id}/activate
DELETE /api/model-services/local/asr/models/{id}
POST   /api/stt
```

建议将 `protocolVersion` 升为 `2`。旧前端仍可读取原字段，新前端额外使用
`engine` 和 `capabilities`。

UI 调整：

- 默认将 **Paraformer 中文 Q8** 放在首位并标记“中文推荐”。
- 模型卡展示 `FunASR · Paraformer`、`FunASR · SenseVoice` 或
  `Whisper · whisper.cpp`。
- 默认语言为中文时优先建议 Paraformer；多语言/语言检测需求建议
  SenseVoice；通用多语种仍保留 Whisper。
- runtime 汇总从单个版本改为“所选模型运行环境”，避免不同引擎同时安装时
  状态表达错误。

## 9. 安全与资源约束

- 子进程参数必须逐项使用 `Command::arg`，禁止拼接 shell 命令。
- 模型、runtime、输入和输出路径必须继续通过 managed path 校验。
- `kill_on_drop(true)` 和 15 分钟总超时继续保留。
- 一次只允许一个本地转写，避免多个大模型争抢内存。
- stdout/stderr 设置最大采集长度；超限时终止进程。
- runtime ZIP/TAR 解压继续限制条目数、展开大小、路径穿越、符号链接和硬链接。
- Windows 首期同时提供普通 x64 与 AVX2 runtime；只有完成 CPU feature 检测后
  才选择 AVX2，绝不能在不支持的 CPU 上尝试启动。

## 10. 首期固定制品候选

实现时必须再次独立下载并校验，本文数值不能代替发布流程中的复核。

| 制品 | 固定来源 | 文件 | 字节数 | SHA-256 |
|---|---|---|---:|---|
| FunASR llama.cpp Windows x64 runtime | GitHub release `runtime-llamacpp-v0.1.4` | `funasr-llamacpp-windows-x64.zip` | 4,663,344 | `ae0bca37e046dcd0e59ac3399f2ed246abf0696a84dc1f4322adc894bb5339e7` |
| Paraformer-zh Q8 | `FunAudioLLM/Paraformer-GGUF@de2cbaaa0f30b34f398d7a066fdfefb8e50d902c` | `paraformer-q8.gguf` | 236,929,024 | `42bf76ea1575a336aaca4c1b7c01a82b79113e6d04d0d6b799561bfcf07ee011` |
| SenseVoiceSmall Q8 | `FunAudioLLM/SenseVoiceSmall-GGUF@90c1c61912018b70ada0fcc024ea24aca62f2e63` | `sensevoice-small-q8.gguf` | 254,208,320 | `4ae45c94422de949b387e2e0fb10d7e14e4c42c69db30c3444ecc7d4b844b7c5` |
| FSMN-VAD | `FunAudioLLM/fsmn-vad-GGUF@6840bae4c5c92ee8c04faaf4db23dd0105098d7f` | `fsmn-vad.gguf` | 1,720,512 | `1270f2559c495f4e7b6e739541151027d360761a3fda43fc147034f5719f5479` |

模型卡声明 Apache-2.0；FunASR runtime 仓库当前为 MIT。合入时必须同步更新
根目录 `NOTICE` 和 `docs/maintenance/local-ai-lite-artifacts.md`。

## 11. 分阶段实施

### Phase A：多引擎基础设施

- 新增 `AsrEngine`、多文件 artifact 和 engine trait。
- runtime 路径按引擎/版本/平台隔离。
- state v1 -> v2 迁移。
- 保证现有两个 Whisper 模型行为和路径迁移测试全部通过。

### Phase B：Paraformer 中文推荐模型

- 接入 FunASR runtime、Paraformer Q8、FSMN-VAD。
- 实现 stdout 解析、进程超时和真实 WAV 冒烟测试。
- UI 展示引擎及“中文推荐”。
- 将其作为中文界面的默认推荐，但不自动下载或替换用户当前模型。

### Phase C：SenseVoiceSmall

- 复用 runtime/VAD 安装。
- 增加 SenseVoice 输出清洗。
- 为语言、情绪和事件标签设计可选结构化结果，但默认仅返回文本。

### Phase D：质量基线与增强

- 建立普通话、口音、中英混说、数字日期、长音频和噪声集。
- 对 Whisper Small、Whisper Turbo、Paraformer 和 SenseVoice 记录 CER、
  实时率、峰值内存和首次启动耗时。
- 评估 Fun-ASR-Nano Q4_K_M 作为“高精度中文”可选模型。

## 12. 验收标准

1. Windows x86_64 无 Python 环境可一键安装并离线转写。
2. 安装 Paraformer 时只下载一份 runtime、一份 VAD 和一份模型。
3. Paraformer 与 SenseVoice 同时安装后可立即切换，且删除其中一个不破坏另一个。
4. 重启应用后已安装、当前启用及 runtime 状态正确恢复。
5. 现有 Whisper 安装和转写不回归。
6. 中文基准集上，推荐模型相对 Whisper Small 的 CER 有明确改善，并记录
   CPU 实时率和峰值内存。
7. 网络中断可断点续传；SHA、大小、重定向主机或解压安全校验失败时不能激活。
8. 运行失败向用户返回可理解错误，日志保留退出码和脱敏后的有限输出。
