# Local AI Lite 制品维护

本文记录 Local AI Lite 当前允许下载的固定制品，以及更新 URL、大小和 SHA-256 的最小安全流程。实现中的唯一事实来源是 `crates/backend/nomifun-system/src/local_model.rs`；本文必须与其中的 `RUNTIME_VERSION`、`runtime_artifact()` 和 `built_in_catalog()` 同步更新。

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

### Qwen3 GGUF 模型

URL 格式固定为：

```text
https://huggingface.co/{repository}/resolve/{revision}/{file}
```

| 模型 | repository @ revision | 文件名 | 大小（bytes） | SHA-256 |
|---|---|---|---:|---|
| Qwen3 0.6B Q4_K_M | `bartowski/Qwen_Qwen3-0.6B-GGUF@60b85c0e3d8fe0f6474f406922a26d12aca4550d` | `Qwen_Qwen3-0.6B-Q4_K_M.gguf` | 484,220,320 | `9acfc1e001311f34b4252001b626f2e466d592a42065f66571bff3790d4e1b14` |
| Qwen3 1.7B Q4_K_M | `bartowski/Qwen_Qwen3-1.7B-GGUF@dcb19155b962dbb6389f4691a982043a8e651022` | `Qwen_Qwen3-1.7B-Q4_K_M.gguf` | 1,282,439,584 | `72c5c3cb38fa32d5256e2fe30d03e7a64c6c79e668ad84057e3bd66e250b24fb` |
| Qwen3 4B Q4_K_M | `bartowski/Qwen_Qwen3-4B-GGUF@cb76885dc66d50759b207c5a48c4e78dfa00c638` | `Qwen_Qwen3-4B-Q4_K_M.gguf` | 2,497,280,960 | `fbe1d5edd4ce802ae3ae7c7e4ab7d09789d697fdac1fc7929f8df4ca3c41bae3` |

模型来自 Qwen Team 的 Qwen3 Apache-2.0 模型；GGUF 转换和量化由 Hugging Face 账号 `bartowski` 发布。固定 revision 的模型卡是来源与归属记录的一部分，不得只保留文件直链。

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

   0.6B 的真实安装与流式推理测试默认忽略（会下载约 520 MB），可显式运行：

   ```powershell
   cargo test -p nomifun-system real_qwen_0_6b_install_and_streaming_smoke_test --lib -- --ignored --nocapture
   ```

   若 CI 已有经过独立校验的 0.6B GGUF，可用 `NOMIFUN_LOCAL_MODEL_SMOKE_MODEL` 指向该文件；测试仍会通过生产代码重新核对固定大小和 SHA-256，再下载/校验 runtime、启动 sidecar 并验证模型列表与 SSE 对话。

## 合入门槛

- 所有 URL 都固定到不可变 revision/tag，重定向仍受 allowlist 约束。
- 实测字节数与 SHA-256 同代码、本文完全一致。
- 归档解压和 GGUF 加载均在目标平台通过。
- 小模型经过 NomiFun 真实中文提示词回归；未通过工具调用测试的模型不得声明 `function_calling`。
- `NOTICE`、模型卡链接和许可证显示同步更新。
