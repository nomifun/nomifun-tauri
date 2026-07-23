# 开发

本页面向修改 **NomiFun** 仓库本身的人：React SPA、Rust 后端、agent 引擎或
Tauri 桌面壳。如果只是安装或部署，请先看
[`../getting-started/installation.zh.md`](../getting-started/installation.zh.md)
或 [`../guides/web-server-deployment.zh.md`](../guides/web-server-deployment.zh.md)。

当前仓库已经是活跃的 Tauri monorepo。旧 Electron 迁移阶段的计划、审计与设计稿
不在仓库中保留；需要这些背景时请查阅 git 历史。

## 前置工具

| 工具 | 最低要求 | 用途 |
| --- | --- | --- |
| Rust | stable，edition 2024 | Workspace 使用 resolver `3` 和 edition `2024`。 |
| Bun | >= 1.3.13 | 前端包管理、Vite runner，也被 agent 运行时使用。 |
| Tauri CLI v2 | 来自 `devDependencies` | 通过 `bun run dev` / `bun run build` 调用，无需全局安装。 |
| Git | 较新版本 | 开发流程和部分内置工具需要。 |
| 原生编译工具 | 按平台 | SQLite、TLS、libgit2、WebKit/WebView 与原生 crate 需要。 |

平台提示：

- Windows：MSVC C++ Build Tools 与 WebView2 runtime。
- macOS：Xcode Command Line Tools。
- Linux：`build-essential cmake clang pkg-config perl git`；构建桌面端还需要 WebKitGTK 4.1 开发包。

## 安装依赖

```bash
git clone <repo-url> nomifun-tauri
cd nomifun-tauri
bun install
cargo check --workspace
```

根 `package.json` 只有一个 Bun workspace：`ui/`。Rust crate 由根 `Cargo.toml`
管理。

## 开发循环

| 命令 | 适用场景 | 实际运行内容 |
| --- | --- | --- |
| `bun run dev:ui` | 纯 UI 工作，可接受 API 请求失败 | Vite on `http://localhost:5173`，不启动后端。 |
| `bun run dev:web` | 浏览器 + 后端联调，关闭登录 | `nomifun-web --port 8787 --dist ui/dist --insecure-no-auth` 加 Vite。 |
| `bun run serve:web` | 从源码跑生产形态 Web host | `nomifun-web` on `http://127.0.0.1:8787`，服务 `ui/dist`，默认开启登录。 |
| `bun run dev` | 桌面/Tauri 开发 | Tauri dev shell、Vite、桌面本地信任策略下的嵌入式后端。 |

`serve:web` 需要先构建 SPA：

```bash
bun run build:ui
bun run serve:web
```

`dev:web` 会同时启动 API 与 UI，并使用 `--insecure-no-auth`，只适合 localhost
或隔离网络。

桌面循环已经不是旧 Electron 模型。Tauri shell 直接链接 `nomifun-app`，在进程内
启动后端，选择一个空闲 localhost 端口，注入 `window.__backendPort` 与
`window.__nomiLocalTrust`，renderer 每次请求都会带上这个本次启动生成的信任
secret。

## 验证命令

| 命令 | 覆盖范围 |
| --- | --- |
| `cargo check --workspace` | 所有 Rust crate 和 app host 编译。 |
| `cargo test -p <crate>` | 单个 crate 的 Rust 测试。 |
| `bun run typecheck` | Renderer TypeScript。 |
| `bun run check:i18n` | i18n key 类型生成是否最新。 |
| `bun run check:theme` | 主题 token 契约。 |
| `bun run help --check` | 根脚本帮助文本。 |
| `bun run build:ui` | 生产 Vite 构建。 |
| `bun run build` | 当前 OS 的 Tauri 桌面包。 |

提交前常用组合：

```bash
cargo check --workspace
bun run typecheck
bun run check:i18n
bun run check:theme
bun run help --check
```

## 后端 CLI

`nomifun-app` 仍然提供独立 `nomicore` binary。app host 不会 spawn 它，但诊断、
stdio MCP bridge 和公开能力调用仍会用到。

当前子命令：

- `mcp-requirement-stdio`
- `mcp-knowledge-stdio`
- `mcp-gateway-stdio`
- `mcp-open-stdio`
- `mcp-computer-stdio`
- `mcp-browser-stdio`
- `terminal-hook --event <kind>`
- `doctor`
- `tools`
- `call <name> [json-args]`
- `agent "<goal>"`

agent 无法启动时，先跑：

```bash
cargo run -p nomifun-app --bin nomicore -- doctor
```

它会按后端看到的 PATH 探测各个 agent CLI，并把结果打印到 stdout。

## 数据目录与工作目录

所有 host 未显式覆盖时共享同一个默认数据目录：

- Windows：`%LOCALAPPDATA%\NomiFun\Nomi`
- macOS：`~/Library/Application Support/NomiFun/Nomi`
- Linux：`$XDG_DATA_HOME/NomiFun/Nomi` 或 `~/.local/share/NomiFun/Nomi`

数据目录包含 SQLite、日志、Bun runtime cache、extension 数据和 agent 状态。
后端启动时会先拿 `{data_dir}/server.lock` 独占锁，避免两个活跃后端同时写同一
目录。

隔离开发环境时显式指定：

```bash
NOMIFUN_DATA_DIR=/tmp/nomifun-dev bun run serve:web
NOMIFUN_DATA_DIR=/tmp/nomifun-dev bun run dev
```

桌面端会追加 channel 对应的 `Nomi` leaf；Web host 和 `nomicore` 按 env 值字面
使用。自动化脚本依赖这个行为前，请先读
[`../reference/configuration.zh.md`](../reference/configuration.zh.md)。

`NOMIFUN_WORK_DIR` 控制会话工作区位置；未设置时回退到数据目录。

## 日志

日志同时写 stdout 和 `<data-dir>/logs/nomicore.log`。示例：

```bash
NOMIFUN_LOG_LEVEL='info,nomifun_mcp=trace' bun run serve:web
```

也可以把 `--log-level` 直接传给 `nomicore` / `nomifun-web`。该值是
`tracing_subscriber::EnvFilter` 语法。

## 继续阅读

- [`project-structure.zh.md`](project-structure.zh.md)
- 修改数据库 schema、ID、逻辑关联、reset 行为或 backup/restore 契约前，
  阅读 [`data-and-identifier-standards.zh.md`](data-and-identifier-standards.zh.md)。
- [`../architecture/backend-crates.md`](../architecture/backend-crates.md)
- [`../architecture/frontend.md`](../architecture/frontend.md)
- [`building-and-packaging.zh.md`](building-and-packaging.zh.md)
