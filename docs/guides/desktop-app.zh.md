# 以桌面应用方式运行 NomiFun

桌面应用 (`nomifun-desktop`) 是一个 [Tauri](https://tauri.app/) 外壳，**在同一进程内**链接 Rust 后端 (`nomifun-app`)。这里没有派生的后端二进制，没有 Electron，也没有捆绑的 `nomicore`。外壳在一个空闲的 `127.0.0.1` 端口上将后端启动为异步任务，然后将打包好的 SPA (`ui/dist`) 加载进 WebView，并使其指向 `http://127.0.0.1:<port>/api`。

桌面 WebView 不显示登录页。嵌入式后端使用 `AuthPolicy::TrustLocalToken`：
外壳把每次启动生成的本地信任 secret 注入自己的 WebView，只有携带该 secret
的请求会被视为桌面用户。如果你想要登录 + 远程浏览器/手机访问，请参阅
[WebUI 远程访问](./webui-remote-access.zh.md)（应用内功能），或
[自托管 Web 服务器](./web-server-deployment.zh.md)（独立服务器）。

![NomiFun 桌面主窗口](../images/desktop-01-main-window.png)

## 快速开始

### 前置条件

桌面应用需要：

- Tauri 支持的平台 (Windows 10+、macOS 11+、主流 Linux 发行版)。
- WebView 运行时：Windows 上的 **WebView2** (Win 11 预装；Win 10 上请安装 [Evergreen Bootstrapper](https://developer.microsoft.com/microsoft-edge/webview2/))，macOS 上的 **WKWebView** (内置)，Linux 上的 **WebKitGTK** (`libwebkit2gtk-4.1-0`)。
- 用于开发：Rust 工具链、[Bun](https://bun.sh) ≥ 1.3.13，以及对应平台的 Tauri 构建依赖 (参见 [Tauri 前置条件](https://v2.tauri.app/start/prerequisites/))。

### 从源码运行 (开发模式)

在仓库根目录：

```bash
bun install
bun run dev
```

这会执行 `tauri dev --config apps/desktop/tauri.conf.json`。它启动 Vite 开发服务器 (`http://localhost:5173`) 来托管 SPA，构建并启动 `nomifun-desktop`，并在每次启动时在一个全新的空闲 localhost 端口上启动嵌入的后端。

### 构建发布包

```bash
bun run build
```

输出包按平台落到 `target/release/bundle/` 下 (Windows 上是 NSIS 安装器 `.exe`，macOS 上是 `.app` + `.dmg`，Linux 上是 `.deb` + `.AppImage`)。要生成签名的更新器构件 (额外的 `.sig` 文件)，请在配置好签名密钥后使用 `bun run build:updater` (参见下方[更新器状态](#更新器状态))。

构建成功后会打印包的位置，例如在 macOS 上：

```text
$ bun run build
   Compiling nomifun-app v0.1.0
    Finished `release` profile [optimized] target(s)
    Bundling NomiFun.app (macos)
    Bundling NomiFun_0.1.0_aarch64.dmg (macos)
    Finished 2 bundles at:
      target/release/bundle/macos/NomiFun.app
      target/release/bundle/dmg/NomiFun_0.1.0_aarch64.dmg
```

## 窗口与标题栏

主窗口在 Windows 和 Linux 上是**无边框**的：React 标题栏组件在与应用内导航同一行绘制最小化/最大化/关闭按钮。在 macOS 上，原生的红绿灯按钮通过 Tauri 的 `Overlay` 标题栏样式得以保留，内容延伸至栏底之下。

- 默认尺寸：`1280 × 832`，最小 `880 × 600`。
- 各处都可调整大小 (即使没有 OS 绘制的装饰，Windows 上的边缘调整和 Snap 仍然可用)。
- 标题栏：`NomiFun`。

> 窗口边框因系统而异：Windows / Linux 上是带应用内控件的无边框标题栏，macOS
> 上保留原生红绿灯按钮（内容延伸至 `Overlay` 栏下）。

## 单实例

`tauri-plugin-single-instance` 在 Windows 和 Linux 上强制应用只运行一个副本。试图启动第二个 `nomifun-desktop` 不会在另一个端口上启动新的后端，而是会静默地聚焦到已有的窗口。

## 深度链接

应用注册了 `nomifun://` URL 协议 (在 `apps/desktop/tauri.conf.json` 的 `plugins.deep-link.desktop.schemes` 下配置)。当操作系统通过 `nomifun://...` URL 启动 Nomi 时，外壳会通过 Tauri 事件 `deep-link://received` 将 URL 转发给渲染进程。渲染进程可以使用 `@tauri-apps/api/event` 中的 `listen('deep-link://received', ...)` 订阅以处理负载。

启动时会调用 `register_all()` 来安装该协议；在需要带外注册步骤的平台上 (某些 Linux 桌面、开发环境)，该调用是尽力而为的，失败会被忽略。

## 自启动

外壳附带 `tauri-plugin-autostart`，使得渲染进程可以通过插件的 invoke API 让应用加入 "登录时启动"。在 macOS 上这使用 `LaunchAgent`；在 Windows 上使用注册表的 `Run` 键；在 Linux 上则使用 autostart 文件夹中的 `.desktop` 文件。面向用户的开关位于应用设置中。

## 通知

`tauri-plugin-notification` 已启用。渲染进程可以显示 OS 级别的通知 (例如，当 agent 完成一个长任务或 AutoWork 有结果时)。在 macOS 上，第一次会请求用户授权；在 Windows 上，通知使用现代的操作中心；在 Linux 上则通过 `libnotify`。

## 数据存储位置

已安装桌面应用将 SQLite 数据库、agent 状态、日志和 Bun 运行时缓存持久化到 stable 的按用户应用数据目录下 —— Windows 上是 **`%LOCALAPPDATA%\NomiFun\Nomi`**，macOS 上是 **`~/Library/Application Support/NomiFun/Nomi`**，Linux 上是 **`$XDG_DATA_HOME/NomiFun/Nomi`**（由 `nomifun_app::cli::default_data_dir()` 解析）。同一 build channel 的宿主共享默认目录；开发脚本改用隔离的同级目录 `Nomi-dev`。开发环境需要 stable 状态副本时可运行 `bun run seed:dev`。

在启动应用前设置 `NOMIFUN_DATA_DIR=<absolute path>`，数据目录就会变为 `$NOMIFUN_DATA_DIR/Nomi`。后端启动时会对数据目录取排他的 `server.lock`；若启动失败 (例如该目录已被另一个实例占用)，桌面外壳会弹出原生错误对话框并退出。

> 旧版本默认使用 `<system temp>/nomifun-data/Nomi`。在那里发现的安装会在启动时自动迁移到按用户位置 (一次性)：数据被复制，数据库中存储的绝对路径会被改写，旧目录保留作为备份。可再生的缓存 (解压出的 Bun 运行时、日志、浏览器配置 …) 不会带过去 —— 它们会在首次使用时重建。

要重新开始，**退出应用**并删除该目录。要迁移，将该目录复制到新机器上即可。

```text
~/Library/Application Support/NomiFun/Nomi/    # macOS（Windows/Linux 路径见上文）
├── nomifun-backend.db        # SQLite 状态（会话、设置、session 等）
├── logs/                     # nomicore.log
├── companion/                # 伙伴 + 共享记忆中枢
├── knowledge/                # 受管理的知识库
├── runtime/                  # 解压出的 Bun 运行时缓存（可再生）
└── server.lock               # 后端运行期间持有的排他锁
```

## 认证与本地信任

桌面外壳不会把旧式完全无鉴权后端暴露给所有 localhost 调用者。它以
`TrustLocalToken` 启动嵌入式后端，向 WebView 注入 `window.__nomiLocalTrust`，
渲染端在 HTTP 与 WebSocket 请求中呈递该 secret。只知道
`127.0.0.1:<port>/api` 的其他进程不会自动被信任。

桌面应用仍是单用户工具：启动它的 OS 账户拥有 agent 能做的一切，包括 shell
和文件访问。

如果你想从另一台设备访问同一个安装，**不要**直接暴露嵌入的端口。请使用以下之一：

- **WebUI 远程访问** (一个按实例启用的功能，参见 [WebUI 远程访问](./webui-remote-access.zh.md)) —— 启动一个独立的认证服务器并提供二维码登录。
- **自托管 Web 服务器** ([Web 服务器部署](./web-server-deployment.zh.md)) —— 在 `nomifun-web` 下以无头方式运行同一个后端，并要求认证。

## 更新器状态

Tauri 更新器插件 (`tauri-plugin-updater`) 已接入，渲染进程暴露了 `invoke('check_for_updates')` (返回新版本字符串，若已是最新则返回 `null`)。然而：

- 在 `apps/desktop/tauri.conf.json` 中配置的端点 (`plugins.updater.endpoints`) 是一个**占位符** (`https://REPLACE-WITH-YOUR-HOST/...`)。在你将其替换为一个提供已签名的 `latest.json` 的真实 HTTPS URL 之前，更新器检查会失败。
- 包含的 `pubkey` 是一个为本地测试生成的**开发密钥**。**在任何公开发布前请替换它**，并将私钥存储在 CI 密钥中。
- `bun run build:updater` 会生成已签名的更新构件 (在每个安装器旁边附带 `.sig` 文件)。

完整 updater 流程（签名环境变量、`latest.json` schema、支持的平台键）在
`apps/desktop/updater/README.md` 中。OS 级别代码签名/公证是另一层：macOS
Developer ID 签名与公证已通过 `bun run build:signed` 和
`apps/desktop/signing/README.md` 接好；Windows 签名仍需要外部代码签名证书。

## 故障排查

**窗口打开后是空白白屏。**
确保已安装 WebView 运行时 (Windows 10 上的 WebView2 需要 Evergreen Bootstrapper)。在 Linux 上需要 `libwebkit2gtk-4.1-0`。

**"Failed to bind backend port"。**
另一个进程占用了 `127.0.0.1` 临时端口。后端会尝试 `pick_free_port()`，失败时回退到 `8799` —— 退出任何其他 NomiFun 实例后再试。

**Agent 命令失败并报 `bun: command not found`。**
Agent 引擎会派生 Bun 作为子进程来执行工具。请安装 Bun (`curl -fsSL https://bun.sh/install | bash`) 并确保它在系统 `PATH` 上，或者使用 `NOMIFUN_EMBED_BUN=1` 构建桌面包以将其嵌入。

## 另请参阅

- [Web 服务器部署](./web-server-deployment.zh.md) —— 在 `nomifun-web` 下以无头方式运行同一个后端。
- [WebUI 远程访问](./webui-remote-access.zh.md) —— 暴露你的桌面实例供远程浏览器/手机使用。
