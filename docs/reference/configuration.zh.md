# 配置参考

NomiFun 读取的每一个参数与环境变量、它们的默认值，以及各自归属的文件。
所有取值都直接来自源码——本页没有列出的设置就不存在。

NomiFun 交付的是**一个**统一的 Rust 后端（`nomifun-app`，二进制
`nomicore`），以及两个嵌入它的宿主：

- `nomifun-desktop` —— Tauri 桌面外壳。在选定的 loopback 端口上以
  `AuthPolicy::TrustLocalToken` 启动后端，并把每次启动生成的本地信任 secret
  注入自己的 WebView。
- `nomifun-web` —— 独立的 Web/服务端宿主。默认以**已鉴权**模式启动同一
  个后端，并在同一端口上提供 SPA。

两个宿主共享后端的同一组配置面；各自的 CLI 仅会覆盖它们自己拥有的那几项。

## `nomifun-web` 参数与环境变量

来源：[`apps/web/src/main.rs`](../../apps/web/src/main.rs)。

| 参数 | 环境变量 | 默认值 | 用途 |
|---|---|---|---|
| `--host` | `NOMIFUN_WEB_HOST` | `127.0.0.1` | 绑定的 IP。`0.0.0.0` 会接受 LAN/VPN/公网流量；大范围暴露前请先预置或完成首次设置。不解析主机名；非法输入将在启动阶段直接失败。 |
| `--port` | `NOMIFUN_WEB_PORT` | `8787` | TCP 端口。在同一个 socket 上提供 API、`/ws` WebSocket 与 SPA。 |
| `--data-dir` | `NOMIFUN_DATA_DIR` | 按用户的应用数据目录 | 后端数据目录（SQLite 数据库、智能体状态、日志、Bun 缓存）。默认是所有宿主共享的按用户位置——Windows 上是 `%LOCALAPPDATA%\NomiFun\Nomi`，macOS 上是 `~/Library/Application Support/NomiFun/Nomi`，Linux 上是 `$XDG_DATA_HOME/NomiFun/Nomi`。可用本参数或 `NOMIFUN_DATA_DIR`（按字面值，不附加后缀）覆盖；生产环境请使用绝对路径。 |
| `--dist` | `NOMIFUN_WEB_DIST` | `../../ui/dist` | 已构建 SPA 所在目录。在仓库之外部署时务必显式指定。 |
| `--admin-user` | `NOMIFUN_ADMIN_USERNAME` | `admin` | 预置首位管理员时使用的用户名。管理员存在后将被忽略。 |
| `--admin-password` | `NOMIFUN_ADMIN_PASSWORD` | — | 在启动时预置首位管理员密码，跳过交互式设置。管理员存在后将被忽略。 |
| `--insecure-no-auth` | `NOMIFUN_WEB_INSECURE_NO_AUTH` | `false` | 危险。完全禁用鉴权（桌面式本地模式）。仅可用于 loopback 或完全受信任的私有网络。 |

布尔环境变量接受 `1`、`true`、`yes`、`on`（不区分大小写）。

## `nomicore`（后端）参数

来源：[`crates/backend/nomifun-app/src/cli.rs`](../../crates/backend/nomifun-app/src/cli.rs)。

下面是独立 `nomicore` 二进制对外暴露的参数。两个宿主会构造一个带默认值
的 `Cli`，仅覆盖各自拥有的那部分——所以单独运行后端时这些参数同样适用。

| 参数 | 默认值 | 用途 |
|---|---|---|
| `--host` | `127.0.0.1`（`DEFAULT_HOST`） | 监听的主机地址。 |
| `--port` | `25808`（`DEFAULT_PORT`） | 监听端口。 |
| `--data-dir` | 按用户的应用数据目录 | 数据库 + 文件存储根目录。通过 clap 绑定 `NOMIFUN_DATA_DIR` 环境变量（按字面值）；两者都未设置时解析 `default_data_dir()`——所有宿主共享的那个按用户位置。 |
| `--work-dir` | （无） | 会话工作区目录。回退顺序：`NOMIFUN_WORK_DIR` 环境变量 → 数据目录本身。 |
| `--app-version` | crate 版本 | 报告给扩展引擎用于做兼容性检查的宿主应用版本。 |
| `--local` | `false` | 独立 `nomicore` 的无鉴权本地模式。`nomifun-web --insecure-no-auth` 映射到同一策略。桌面外壳不使用该 flag，而是使用 `TrustLocalToken`。 |
| `--log-dir` | `<data-dir>/logs` | 滚动日志的目录。 |
| `--log-level` | `info` | 日志级别过滤。支持按 target 覆盖——例如 `info,nomifun_mcp=trace`。 |

子命令（供智能体 CLI 桥与诊断使用）：

| 子命令 | 用途 |
|---|---|
| `mcp-requirement-stdio` | AutoWork requirement 声明工具的 MCP stdio server。 |
| `mcp-knowledge-stdio` | 每会话 knowledge search 的 MCP stdio server。 |
| `mcp-gateway-stdio` | Desktop Gateway 工具的 MCP stdio server。 |
| `mcp-open-stdio` | 暴露可靠 OS `open` 工具的 MCP stdio server。 |
| `mcp-computer-stdio` | 暴露 desktop computer-use 工具的 MCP stdio server。 |
| `mcp-browser-stdio` | 暴露 browser-use 工具的 MCP stdio server。 |
| `terminal-hook --event <kind>` | 一次性 terminal 生命周期 hook relay。 |
| `doctor` | 自检：填充智能体注册表，逐个探测 `$PATH` 上的每个 CLI，并打印一张按智能体维度的可用性表格。 |
| `tools` | 以 JSON 列出 Remote 能力名称与描述。 |
| `call <name> [json-args]` | 通过 `/v1` 调用运行中实例上的 Remote 能力。 |
| `agent "<goal>"` | `nomi_agent_run` 能力的便捷包装。 |

## 共享环境变量

下列变量由后端读取，不论被哪个宿主嵌入。

| 环境变量 | 读取方 | 作用 |
|---|---|---|
| `NOMIFUN_DATA_DIR` | 所有宿主 | 当宿主选择遵循该值时，作为后端数据目录的真值来源。桌面外壳会附加 `/Nomi`：设置该环境变量时目录为 `$NOMIFUN_DATA_DIR/Nomi`；未设置时目录为按用户的应用数据默认值（见[下文](#数据目录与工作目录的语义)）。独立 Web 宿主与 `nomicore` 二进制则按字面值将其作为 `--data-dir` 的默认值（不附加任何后缀）。 |
| `NOMIFUN_WORK_DIR` | `nomicore` | `--work-dir`（按会话区分的工作区根）的回退值。 |
| `JWT_SECRET` | `nomifun-app` | 用于签发会话 JWT 的密钥。解析顺序见 [鉴权密钥解析](#鉴权密钥解析)。 |
| `NOMIFUN_HTTPS` | `nomifun-auth::CookieConfig` | 取真值时，会话与 CSRF cookie 会带上 `Secure` 标记和 `SameSite=Strict`。当应用通过 HTTPS 暴露（TLS 反向代理等）时请打开。默认 `false` → 不带 `Secure` 标记，`SameSite=Lax`。 |
| `SHELL` | 智能体引擎（Linux/macOS） | 智能体引擎派生子进程时使用的 shell。在 systemd 下的 Linux 服务器上请显式设置（系统账户通常没有 `$SHELL`）。 |
| `NOMIFUN_URL` | `nomicore call`, `nomicore agent` | 调用 Remote capability 时使用的运行中实例 base URL。 |
| `NOMIFUN_COMPANION_TOKEN` | `nomicore call`, `nomicore agent` | 访问 `/v1` Remote capability 路由的 companion access token。 |

代码库不集成 `SENTRY_DSN`：这个环境变量并未被读取。

## 后端常量

来源：[`crates/backend/nomifun-common/src/constants.rs`](../../crates/backend/nomifun-common/src/constants.rs)。
这些是编译期值，不是环境变量——列在这里只是为了让运维方了解相关上限。

| 常量 | 取值 | 用途 |
|---|---|---|
| `DEFAULT_HOST` | `127.0.0.1` | `nomicore` 的默认 `--host`。 |
| `DEFAULT_PORT` | `25808` | `nomicore` 的默认 `--port`。（Web 宿主将其覆写为 `8787`。） |
| `BODY_LIMIT` | `10 MiB` | 应用于每条路由的默认请求体大小限制。需要更大的路由（例如 `/api/fs/upload`）会安装自己的更大限制。 |
| `UPLOAD_MAX_SIZE` | `30 MiB` | 文件上传路由（`/api/fs/upload`）的上限。 |
| `REMOTE_IMAGE_MAX_SIZE` | `5 MiB` | 下载聊天中引用的远程图片时的上限。 |
| `COOKIE_NAME` | `nomifun-session` | 会话 cookie。 |
| `CSRF_COOKIE_NAME` | `nomifun-csrf-token` | CSRF cookie（不是 HttpOnly——JavaScript 需要读取它）。 |
| `CSRF_HEADER_NAME` | `x-csrf-token` | 与 CSRF cookie 值对应的请求头（Double Submit Cookie 模式）。 |
| `COOKIE_MAX_AGE_DAYS` | `30` | Cookie 的 `Max-Age`。 |
| `SESSION_EXPIRY` | `24h` | JWT 在需要刷新前的有效期。 |
| `HEARTBEAT_INTERVAL_MS` / `HEARTBEAT_TIMEOUT_MS` | `30000` / `60000` | WebSocket 的心跳 ping/pong。 |

## 数据目录与工作目录的语义

- `data-dir` 存放 SQLite 数据库（`nomifun-backend.db*`）、各智能体状态、
  Bun 缓存、日志文件，以及任何嵌入式扩展数据。把它当成普通数据库来
  对待——做好备份、限制权限。两个同时运行的后端共享它的情况已被机制
  性地阻止（见下面的服务器锁）。
- 三个宿主（`nomifun-desktop`、`nomifun-web`、独立的 `nomicore`
  二进制）通过 `nomifun_app::cli::default_data_dir()` 解析出**同一个
  默认**数据目录：Windows 上是 `%LOCALAPPDATA%\NomiFun\Nomi`，macOS
  上是 `~/Library/Application Support/NomiFun/Nomi`，Linux 上是
  `$XDG_DATA_HOME/NomiFun/Nomi`（通常为 `~/.local/share/NomiFun/Nomi`），
  经 `dirs` crate 解析；当 OS 报告不出用户目录时的极端回退是
  `<system temp>/nomifun-data/Nomi`。所有宿主共用一个默认值是有意为
  之：开发循环（`bun run serve:web`、`dev:web`、`dev`）与已安装
  的桌面应用读写同一份状态——配置一次提供商或伙伴，处处可测；排查
  问题也只需要看一个目录。想要隔离的沙箱时，把 `NOMIFUN_DATA_DIR`
  或 `--data-dir` 指到别处即可。
- 后端启动时（早于打开数据库）会对 `{data_dir}/server.lock` 取一把
  OS 级**排他锁**。同一数据目录上的第二个后端进程会快速失败，错误
  信息会指出持有者（pid + 可执行文件名）并给出两条出路：关掉另一个
  实例，或用 `NOMIFUN_DATA_DIR` / `--data-dir` 给这一个指一个独立
  目录。锁是 advisory 的（经 `fs2` 走 `flock` / `LockFileEx`），进程
  退出或崩溃时由 OS 自动释放——残留的 `server.lock` 文件无害。
  `nomicore doctor` 与 `mcp-*` stdio 子命令不取这把锁（doctor 设计上
  就允许与运行中的服务器并存）。
- `work-dir` 存放按会话区分的工作区。未设置时按以下顺序解析：
  `--work-dir` → 非空的 `NOMIFUN_WORK_DIR` 环境变量 → 数据目录本身。
  会话会在 `<work-dir>/conversations/` 下创建子目录；删除会话同时
  删除其工作区。
- 桌面外壳使用上述共享默认值。设置 `NOMIFUN_DATA_DIR` 后会附加
  `/Nomi`：目录变为 `$NOMIFUN_DATA_DIR/Nomi`——覆盖语义不变。旧版
  构建默认在 `<system temp>/nomifun-data/Nomi`；首次以新默认启动时，
  既有的 temp 根安装会被自动搬迁（一次性；旧目录保留为备份，数据库
  中存储的绝对路径会被改写）。
- Web 宿主按字面值使用该值——`--data-dir`（或 `NOMIFUN_DATA_DIR`）
  原样生效，不附加 `/Nomi` 后缀——因此 Docker（`/data`）与 systemd
  （`/var/lib/nomifun`）部署不受影响。两者都未设置时，回退到同一个
  共享的按用户默认目录；旧的相对 `data` 默认值已不复存在。

## 鉴权密钥解析

`JwtService` 由单一密钥构造；`AppServices::from_config` 按以下顺序解析它：

1. 若已设置，使用 `JWT_SECRET` 环境变量。
2. 否则，使用系统用户行（`system_default_user.jwt_secret`）中持久化的值。
3. 否则，生成一个全新的强随机密钥，并**持久化到数据库**供后续启动使用。

修改密码流会顺带轮换 JWT 密钥，使所有现有会话失效。

静态加密使用独立的持久密钥，存放在 `<data-dir>/encryption_key`。旧安装若还没有该文件，启动时会用当前解析到的 JWT 密钥派生并写入一次，以保证既有加密字段仍可读取；之后修改密码或轮换 JWT 密钥不会再改变数据加密密钥。

## TLS / HTTPS Cookie 处理

NomiFun 自身不做 TLS 终止——请在前面放一个负责 TLS 终止的反向代理
（Caddy、nginx 等）。届时：

- 设置 `NOMIFUN_HTTPS=true`，使 cookie 带上 `Secure` 标记和
  `SameSite=Strict`。否则浏览器会在 HTTPS 响应上拒收 `Secure` cookie，
  登录看似会无声失败。
- `/ws` 上的 WebSocket 升级无需额外的请求头即可穿过任何符合标准的
  代理；Caddy 开箱即用。

可参考 [`guides/web-server-deployment.md`](../guides/web-server-deployment.md)
中完整的 Caddy + Docker 示例。

## 日志

- 所有日志同时写入 stdout（让 `journalctl`/`docker logs` 能捕捉到）以及
  `<log-dir>/nomicore.log` 上的按日滚动文件。
- `--log-level` 接受完整的 [`tracing` `EnvFilter`](https://docs.rs/tracing-subscriber/latest/tracing_subscriber/filter/struct.EnvFilter.html)
  指令：一个全局级别，或一组以逗号分隔的按 target 覆盖项。

  示例：

  - `info` —— 全局 info。
  - `debug` —— 全局 debug。较啰嗦；适合短时复现。
  - `info,nomifun_mcp=trace` —— 默认 info，MCP 模块为 trace。
  - `warn,nomifun_conversation=info,nomifun_terminal=debug` —— 整体
    更安静；会话引擎为 normal/info；终端为 debug。

不存在另一套 `RUST_LOG` 通路——`--log-level`（或宿主中等价的环境驱动
开关）是唯一的总开关。

## 另见

- [Web 服务部署](../guides/web-server-deployment.md) —— 用 Docker、
  systemd、Caddy 运行 `nomifun-web`。
- [作为桌面应用运行 Nomi](../guides/desktop-app.md) —— 桌面端专属配置。
- [API 概览](./api-overview.zh.md) —— 配置完成并启动后，后端对外暴露
  了什么。
- [疑难排查](./troubleshooting.zh.md) —— 配置在运行时出错时的症状与
  修复方法。
