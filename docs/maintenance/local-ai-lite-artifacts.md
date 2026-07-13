# Local AI Lite 制品维护

本文记录 Local AI Lite 当前允许下载的固定制品，以及更新 URL、大小和 SHA-256 的最小安全流程。实现中的事实来源为 `nomifun-system/src/local_model.rs` 与 `nomifun-creation/src/adapters/local_image.rs`；本文必须与代码同步更新。

## 当前固定制品

### llama.cpp runtime

- 版本：`b9957`
- URL 前缀：`https://github.com/ggml-org/llama.cpp/releases/download/b9957/`
- 许可证：MIT；归属信息见仓库根目录 `NOTICE`

| 目标平台 | 文件名 | 大小（bytes） | SHA-256 |
|---|---|---:|---|
| Windows x86_64 / Vulkan | `llama-b9957-bin-win-vulkan-x64.zip` | 32,897,089 | `fcc0a8c0f0f3140122452ed2728cebb520c5fbc4fc921836ee3a45dd77e18c68` |
| Windows ARM64 / CPU | `llama-b9957-bin-win-cpu-arm64.zip` | 12,134,012 | `3eeecdc9d1d33932e84bb7cecec9b6dcbc95072f3f7e52a1d7252f17afac6542` |
| macOS ARM64 / Metal | `llama-b9957-bin-macos-arm64.tar.gz` | 10,737,291 | `7a43fd3c4ddd30f3c408da7c80975503f18b829da023a7d0e34bdb6f1b1a056f` |
| macOS x86_64 / Metal | `llama-b9957-bin-macos-x64.tar.gz` | 11,006,704 | `f03f6669c7e34c2768ca4a318dd13e105dec46e1f87a2165d2be7fd6a0ee4716` |
| Linux x86_64 / Vulkan | `llama-b9957-bin-ubuntu-vulkan-x64.tar.gz` | 31,171,524 | `0a65257a72010e93c39136a50b8904202f3c4c40ff3ecd8a33a47c903035c724` |
| Linux ARM64 / Vulkan | `llama-b9957-bin-ubuntu-vulkan-arm64.tar.gz` | 25,413,005 | `87554e8d13a1980d9a3829361b430249fd74a8b924a02f74e29dc996b58384b3` |

完整 URL 为“URL 前缀 + 文件名”。不得改用 `latest`、分支名或其他可移动引用。

### Qwen3.5 GGUF 模型

URL 格式固定为：

```text
https://huggingface.co/{repository}/resolve/{revision}/{file}
```

| 模型 | repository @ revision | 文件名 | 大小（bytes） | SHA-256 |
|---|---|---|---:|---|
| Qwen3.5 4B Q4_K_M | `unsloth/Qwen3.5-4B-GGUF@e87f176479d0855a907a41277aca2f8ee7a09523` | `Qwen3.5-4B-Q4_K_M.gguf` | 2,740,937,888 | `00fe7986ff5f6b463e62455821146049db6f9313603938a70800d1fb69ef11a4` |
| Qwen3.5 9B Q4_K_M | `unsloth/Qwen3.5-9B-GGUF@3885219b6810b007914f3a7950a8d1b469d598a5` | `Qwen3.5-9B-Q4_K_M.gguf` | 5,680,522,464 | `03b74727a860a56338e042c4420bb3f04b2fec5734175f4cb9fa853daf52b7e8` |
| Qwen3.5 4B 视觉投影器 | `unsloth/Qwen3.5-4B-GGUF@e87f176479d0855a907a41277aca2f8ee7a09523` | `mmproj-F16.gguf` | 672,423,616 | `cd88edcf8d031894960bb0c9c5b9b7e1fea6ebee02b9f7ce925a00d12891f864` |
| Qwen3.5 9B 视觉投影器 | `unsloth/Qwen3.5-9B-GGUF@3885219b6810b007914f3a7950a8d1b469d598a5` | `mmproj-F16.gguf` | 918,166,080 | `f70dc3509053962b0d0d3ee8a7eacebf5d60aa560cad78254ae8698516ae029f` |

模型来自 Qwen Team 的 Qwen3.5 Apache-2.0 模型；GGUF 转换和量化由 Hugging Face 组织 `unsloth` 发布。固定的 llama.cpp `b9957` runtime 已确认能够识别 `qwen35` 架构。两个 GGUF 的原生上下文元数据为 262K，NomiFun 为控制普通设备上的 KV cache 占用，将运行上下文统一限制为 65,536。固定 revision 的模型卡是来源与归属记录的一部分，不得只保留文件直链。

### 本地语音识别

语音识别控制面与文本/图片门面独立懒初始化。只有首次安装 ASR 模型或恢复已有 ASR 安装状态时才创建 `local-ai/asr`；Whisper 或 FunASR CLI 每次转写按需启动，完成后退出，不常驻占用模型内存。当前生产 runtime 仅支持 Windows x86_64，其他平台状态必须明确返回 `unsupported_platform`，不得开始下载。

| 制品 | 固定来源 | 大小（bytes） | SHA-256 |
|---|---|---:|---|
| whisper.cpp Windows x86_64 runtime | release `v1.9.1`, `whisper-bin-x64.zip` | 7,982,101 | `7d8be46ecd31828e1eb7a2ecdd0d6b314feafd82163038ab6092594b0a063539` |
| Whisper Small multilingual Q5_1 | `ggerganov/whisper.cpp@5359861c739e955e79d9a303bcbc70fb988958b1`, `ggml-small-q5_1.bin` | 190,085,487 | `ae85e4a935d7a567bd102fe55afc16bb595bdb618e11b2fc7591bc08120411bb` |
| Whisper Large v3 Turbo Q5_0 | `ggerganov/whisper.cpp@5359861c739e955e79d9a303bcbc70fb988958b1`, `ggml-large-v3-turbo-q5_0.bin` | 574,041,195 | `394221709cd5ad1f40c46e6031ca61bce88931e6e088c188294c6d5a55ffa7e2` |
| FunASR llama.cpp Windows x86_64 runtime | release `runtime-llamacpp-v0.1.4`, `funasr-llamacpp-windows-x64.zip` | 4,663,344 | `ae0bca37e046dcd0e59ac3399f2ed246abf0696a84dc1f4322adc894bb5339e7` |
| Paraformer-zh Q8 | `FunAudioLLM/Paraformer-GGUF@de2cbaaa0f30b34f398d7a066fdfefb8e50d902c`, `paraformer-q8.gguf` | 236,929,024 | `42bf76ea1575a336aaca4c1b7c01a82b79113e6d04d0d6b799561bfcf07ee011` |
| FSMN-VAD | `FunAudioLLM/fsmn-vad-GGUF@6840bae4c5c92ee8c04faaf4db23dd0105098d7f`, `fsmn-vad.gguf` | 1,720,512 | `1270f2559c495f4e7b6e739541151027d360761a3fda43fc147034f5719f5479` |

whisper.cpp、OpenAI Whisper 权重和 FunASR runtime 为 MIT；Paraformer 与 FSMN-VAD 固定模型卡声明 Apache-2.0。模型 URL 必须使用上述完整 Hugging Face revision，不得改为 `main`。实时浏览器录音在前端解码并重采样为单声道 16 kHz PCM16 WAV 后上传；用户选择的 WAV、MP3、OGG、FLAC 文件保持原格式。其他容器由本地 ASR 明确拒绝（云端 STT 仍保持原有格式支持）。后端 `/api/stt` 单独允许 31 MiB multipart body，模型服务仍将音频净载荷限制为 30 MiB。

### Z-Image-Turbo 本地生图

运行时固定为 stable-diffusion.cpp `master-775-b5d8120`。当前支持 Windows x86_64 Vulkan、macOS ARM64 Metal 与 Linux x86_64 Vulkan；平台不匹配时不得开始下载。

| 制品 | repository/release | 大小（bytes） | SHA-256 |
|---|---|---:|---|
| Z-Image-Turbo Q3_K | `leejet/Z-Image-Turbo-GGUF@c61c0e422dc8b541b7548cf33a4ef8302b0f8085` | 3,143,559,104 | `4b44bdaa7814f20d7cf144e3939bd93aa32f50660204dd0c2aea5c5376232980` |
| Qwen3-4B 文本编码器 Q4_K_M | `unsloth/Qwen3-4B-Instruct-2507-GGUF@a06e946bb6b655725eafa393f4a9745d460374c9` | 2,497,281,120 | `3605803b982cb64aead44f6c1b2ae36e3acdb41d8e46c8a94c6533bc4c67e597` |
| VAE | `Comfy-Org/z_image_turbo@d24c4cf2a0cd98a42f23467e27e3d76ee9438b8e` | 335,304,388 | `afc8e28272cd15db3919bacdb6918ce9c1ed22e96cb12c4d5ed0fba823529e38` |

三件模型文件合计 5,976,144,612 bytes。VAE 仓库卡未声明许可证，必须保留 NOTICE 与 UI 风险提示，不得随安装包重分发。安装后首次生成会重验全部 SHA，并从已验证 runtime ZIP 原子重建可执行目录。

### 从旧目录迁移

应用启动时会对已退役的 `qwen3-0.6b-q4-k-m`、`qwen3-1.7b-q4-k-m` 和 `qwen3-4b-q4-k-m` 做一次幂等清理。清理范围严格限定为对应固定目录中的已知 `.gguf` 文件和同名 `.part` 文件，目录也只在为空时删除；未知文件、未知模型目录、符号链接和重解析点不会被递归删除。旧 ID 会从持久化安装/启用状态中移除，不会被静默映射到新的 Qwen3.5 制品。

## 更新流程

1. **选择不可变来源。** runtime 只能使用明确的 llama.cpp release tag；模型只能使用完整的 Hugging Face commit SHA。确认 HTTPS 主机仍在下载 allowlist 内。
2. **审阅许可证与归属。** 阅读新 runtime 的 `LICENSE`、模型卡、上游基础模型许可证和任何使用限制。供应者、基础模型或许可证变化时，同一提交更新根目录 `NOTICE`。
3. **下载到隔离临时目录。** 不覆盖现有缓存，不从浏览器缓存或第三方网盘取样。记录最终重定向主机。
4. **独立计算大小和 SHA-256。** 两人或 CI 与本地至少各校验一次；不要抄 release 页面中的显示大小。

   PowerShell：

   ```powershell
   (Get-Item -LiteralPath $artifact).Length
   (Get-FileHash -LiteralPath $artifact -Algorithm SHA256).Hash.ToLowerInvariant()
   ```

   Unix：

   ```bash
   wc -c < "$artifact"
   sha256sum "$artifact"
   ```

5. **检查内容。** runtime 归档必须包含预期的 `llama-server` 可执行文件，不得含绝对路径、越界 `..`、硬链接或逃逸目标；GGUF 必须能被固定 runtime 读取，模型架构、量化类型和上下文元数据应与 catalog 一致。
6. **原子更新三元组。** 每个制品的 URL、准确字节数和 SHA-256 必须在同一提交更新。runtime tag 还必须同步更新 `RUNTIME_VERSION`、所有平台文件名和本文表格。
7. **运行自动化验证。** 至少执行：

   ```text
   cargo test -p nomifun-api-types
   cargo test -p nomifun-system local_model
   cargo test -p nomifun-system --test managed_model_routes
   bun test ui/src/renderer/pages/modelHub/localModelView.test.ts
   bun run typecheck
   ```

8. **做干净机器冒烟测试。** 每个受支持的 OS/架构至少验证一次：首次下载、断点续传、取消后继续、错误 SHA 拒绝、安装后启动、`/v1/models`、流式对话、切换模型、停止和删除。退出 NomiFun 后不得残留 `llama-server`。

   4B 的真实安装与流式推理测试默认忽略（含视觉投影器约 3.42 GB），可显式运行：

   ```powershell
   cargo test -p nomifun-system real_qwen_3_5_4b_install_and_streaming_smoke_test --lib -- --ignored --nocapture
   ```

   若 CI 已有经过独立校验的 4B GGUF，可用 `NOMIFUN_LOCAL_MODEL_SMOKE_MODEL` 指向该文件；测试仍会通过生产代码重新核对固定大小和 SHA-256，再下载/校验 runtime、启动 sidecar 并验证模型列表与 SSE 对话。

## 合入门槛

- 所有 URL 都固定到不可变 revision/tag，重定向仍受 allowlist 约束。
- 实测字节数与 SHA-256 同代码、本文完全一致。
- 归档解压和 GGUF 加载均在目标平台通过。
- 小模型经过 NomiFun 真实中文提示词回归；未通过工具调用测试的模型不得声明 `function_calling`。
- `NOTICE`、模型卡链接和许可证显示同步更新。
