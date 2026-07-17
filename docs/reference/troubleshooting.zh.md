# 疑难排查

运行 NomiFun 时可能遇到的症状，以及它们背后真实的成因。如果你遇到的
问题不在本表里，源码就是最快的参考——本页描述的每一个行为都对应
`crates/backend/` 下的某个具体文件。

## 后端端口 / 连接问题

### `nomifun-web: invalid --host '<value>'`

host 参数必须能被解析为 IP 地址（`127.0.0.1`、`0.0.0.0`、某个具体
网卡的 IP）。`localhost` 这样的主机名不会被解析——`nomifun-web` 会
直接以这条信息快速失败，而不是稍后再抛出晦涩的 socket 错误。请传入
一个 IP 字面量。

### 配置端口上出现 `address already in use`

有别的进程占着同一个端口。Web 宿主默认使用 `8787`
（`NOMIFUN_WEB_PORT`）。桌面外壳没有这个问题——它启动时会向 OS 索要
一个空闲的 localhost 端口，然后通过 IPC 告知渲染端。

在 Linux/macOS 上找出占用者：`lsof -i :8787`。Windows 上：
`Get-NetTCPConnection -LocalPort 8787`。要么把它杀掉，要么修改
`--port` / `NOMIFUN_WEB_PORT`。

### 浏览器无法在非 loopback 地址上访问服务

`nomifun-web` 默认绑定 `127.0.0.1`。要接受 LAN/VPN 流量，需要传入
`--host 0.0.0.0` 或设置 `NOMIFUN_WEB_HOST=0.0.0.0`。如果该主机可被较大范围访问，
请先预置管理员或完成首次设置。在 Windows / macOS 宿主上还要检查防火墙——OS
可能默默丢弃连接。

如果目标只是从手机或同一 LAN 上的其他设备进行远程访问，
[WebUI 远程访问](../guides/webui-remote-access.md) 通常比完整的服务
部署更省事。

## 首次启动管理员与登录问题

### 启动服务后 `GET /api/auth/status` 返回 `needs_setup: true`

这是全新安装且没有设置 `NOMIFUN_ADMIN_PASSWORD` 时的预期状态。第一个
访问浏览器的人输入的用户名 + 密码会通过 `POST /api/auth/setup` 成为
管理员。打开 URL，填写表单，登录即可完成。

如果希望在服务对外可达之前关闭这个时间窗口，请在首次启动前设置
`NOMIFUN_ADMIN_PASSWORD`（可选地再设置 `NOMIFUN_ADMIN_USERNAME`）。

### `/api/auth/setup` 返回 `409 Conflict`

管理员已经存在了。setup 端点只能调用一次。请改用 `POST /login`；
如果在自托管实例上忘了密码，可通过
[WebUI 远程访问](../guides/webui-remote-access.md) 描述的仅本地 WebUI
流程恢复。

### 登录看起来成功，但下一个请求返回 `401`

绝大多数情况是 TLS 反向代理后的 cookie 问题。`Secure` 标记只在
`NOMIFUN_HTTPS=true` 时才会被加到 cookie 上。在没有该标记的 HTTPS
响应中，浏览器会直接拒收 cookie，下一个请求于是没有会话。设置
`NOMIFUN_HTTPS=true` 然后重新加载即可。

第二个原因：服务器时钟漂移。如果系统时钟差得太远，签发它的同一台
服务器都会把 JWT 当作过期。请确认 NTP 在跑。

### 修改密码时密码明明对，却报 `Current password is incorrect`

该端点会用恒定时间的 bcrypt 比对存储的哈希。如果你怀疑数据损坏：
停止服务、备份数据目录，通过 `installation_identity.owner_user_id` 找到
安装所有者，然后查看该用户的 `password_hash` 列。可以做精细的修复（在本地模式下使用
`/api/auth/internal/users/{id}/password`），但最简单的路径是从备份
恢复或重新初始化。

### 用一个含糊的校验错误拒绝"用户名/密码"

校验器拒绝一小撮显而易见的模式——长度小于 8 的密码、常见词典词、
不在 `[a-zA-Z0-9_-]` 中的用户名，或者以 `-`/`_` 开头/结尾的用户名。
请换一个。

## CSRF 错误

### 在 POST/PUT/PATCH/DELETE 上出现 `403 CSRF token validation failed`

`nomifun-csrf-token` cookie 的值必须与 `x-csrf-token` 请求头匹配。
中间件会在第一个没带它的响应中自动设置该 cookie，因此一个新加载的
SPA 会在它的第一个 GET 请求中拿到。这通常因为以下几种原因失败：

- 客户端假设服务处于无鉴权本地模式，但服务实际运行在已鉴权模式下
  （或反过来）。`nomicore --local` 与 `nomifun-web --insecure-no-auth`
  会跳过 CSRF；普通 `nomifun-web` 需要它。桌面外壳使用 `TrustLocalToken`，
  自己的 WebView 正常不应看到 CSRF 失败，除非注入的本地信任 header/cookie
  流程被破坏。
- 反向代理剥掉了 cookie 或重写了 `Set-Cookie`。Caddy/nginx 的标准配置
  不会动它们；自定义 rewrite 规则可能会。
- 浏览器的第三方 cookie 屏蔽影响到了部署域名。

`/login`、`/api/auth/setup`、`/api/auth/qr-login` 对 CSRF 豁免；CSRF
仅作用于*登录后*的状态变更类路由。

## WebSocket 断连

### 连接立刻以 1008 关闭

1008 是"策略违规"——服务端在两种特定情况下使用：

- "no token provided" —— WebSocket 升级请求没有携带 JWT。
- 一条 `auth-expired` 事件后接关闭——token 存在但无效或已过期。

这两种通常都是 token 过期导致。通过 `GET /api/ws-token` 刷新 token
后重连即可。如果你在登录后立刻看到这一现象，请确认 cookie 是否被
正常透传（参见上面 cookie 不生效的小节），并确认
`Sec-WebSocket-Protocol`（或你使用的请求头）原样到达了服务端。

### WebSocket 连上之后悄悄不再收到事件

服务端每 30 秒 ping 一次，60 秒未响应则视为客户端已死。如果网络静默
地断掉了连接（移动 NAT、强制门户、不靠谱的代理），在服务端清理它
之前客户端侧仍显示已连接。客户端应当负责重连；SPA 会自动重连。如果
你写了自己的客户端，请在 close 事件上实现指数退避重连。

## "Agent CLI not found" 与 bun 相关问题

### 会话立即以 "agent not available" / "command not found" 失败

智能体引擎会派生 ACP 智能体 CLI（`claude`、`codex`、`gemini`、
`nomi`、`codebuddy` 等），它们必须出现在**进程**的 `PATH` 里。
进程 PATH 在启动时被增强（`nomifun_runtime::enhance_process_path`），
但若二进制位置不寻常，仍可能被错过。

跑一下 doctor：

```bash
nomicore doctor
```

它会填充智能体注册表，逐个探测 `$PATH` 上的每个 CLI，并打印一张按
智能体维度的可用性表格。务必在与启动应用相同的 shell 中运行它，
以看到应用真正看到的内容。如果某个智能体缺失，请安装其 CLI 或把它
的 bin 目录加入 `PATH` 后重启。

### 在 systemd 下：`bun: command not found`

智能体引擎要求 **`bun ≥ 1.3.13`**。一个 `nologin` 的系统账户看不到
`~/.bun/bin/`；请把 bun 装到系统级（`sudo install ~/.bun/bin/bun
/usr/local/bin/bun`），或用 `NOMIFUN_EMBED_BUN=1` 构建以将 bun 打包
进二进制——它会在首次运行时把自己解压到数据目录。已写好的食谱见
[Web 服务部署](../guides/web-server-deployment.md#bun-must-be-on-the-system-path)。

安装后用 `sudo -u nomifun -s -- which bun` 验证。

### 看到 "bun runtime extraction" 日志后再无智能体活动

嵌入式 bun 构建会在首次运行时把 bun 解压到数据目录。若解压失败
（通常是权限问题），智能体引擎就没有运行时。请检查数据目录中是否
存在 bun 二进制，确认服务用户拥有数据目录，并在日志里查看真实的
解压错误。

## Office 预览

### Word/Excel/PPT 预览返回 "LibreOffice not detected"

`/api/star-office/detect` 路由会在系统中探测 LibreOffice。Office 预览
功能（`/api/word-preview/*`、`/api/excel-preview/*`、
`/api/ppt-preview/*`、`/api/document/convert`）需要 LibreOffice 才能
渲染文档。

- Linux：`apt install libreoffice`（或同等发行版命令）。
- macOS：`brew install --cask libreoffice`。
- Windows：从 libreoffice.org 安装。

安装后请重启后端，让它重新探测。

### 预览 iframe 一直空白

Office 预览路由会派生 LibreOffice 子进程，并通过
`/api/ppt-proxy/*` 与 `/api/office-watch-proxy/*` 代理它们。这些代理
路由是有意**公共**（不鉴权）的——iframe 内容必须在不带 SPA 会话
cookie 的情况下加载。如果你的反向代理剥掉了 URL 路径段，或在边缘对
`/api/*` 全部加了鉴权，请把这些 proxy 路径豁免出去。

## 数据目录权限

### 服务起来了，但数据库写入失败 / "unable to open database file"

配置的数据目录必须对进程可写。常见情况：

- 在 systemd 下以 `User=nomifun` 运行，但数据目录的所属者是另一个
  用户。修复：`chown -R nomifun:nomifun /var/lib/nomifun`。
- 一个只读挂载（`RootDirectory=`、`ProtectHome=yes` 等）覆盖了数据
  路径。请去掉过宽的沙箱；保留官方 unit 中的中度加固
  （`NoNewPrivileges=yes`、`PrivateTmp=yes`）。
- 在 Docker 下，把宿主目录挂载进来但 UID 与容器不一致。请改用命名
  卷，或将宿主目录 `chown` 到正确的 UID。

桌面外壳的默认数据目录是**按用户的应用数据目录**（Windows 上是
`%LOCALAPPDATA%\NomiFun\Nomi`，macOS 上是
`~/Library/Application Support/NomiFun/Nomi`，Linux 上是
`$XDG_DATA_HOME/NomiFun/Nomi`），它天然对启动应用的用户可写。设置
`NOMIFUN_DATA_DIR=<absolute path>` 后目录会变成
`$NOMIFUN_DATA_DIR/Nomi`。位于 `<system temp>/nomifun-data/Nomi` 的
遗留安装会在启动时被自动搬迁到新默认位置（旧目录保留为备份）；若
搬迁未能完成，应用会继续从遗留目录启动，并在下次启动时重试。

### `data directory ... is already in use by another running NomiFun backend`（数据目录被占用）

所有宿主（桌面外壳、`nomifun-web`、`nomicore` 二进制）默认使用
**同一个**按用户的数据目录，而后端启动时会对 `{data_dir}/server.lock`
取一把 OS 级排他锁——同一目录上的第二个后端会以这条信息快速失败，
而不是悄悄破坏共享状态。最经典的触发方式：桌面应用还开着，又去跑
`bun run serve:web` / `dev:web`（或反过来）。两条出路：关掉另一个实例
（错误信息会写明持有者的 pid 与可执行文件名），或用
`NOMIFUN_DATA_DIR` / `--data-dir` 给新实例指一个独立目录。持有者
退出或崩溃时锁由 OS 自动释放；残留的 `server.lock` 文件无害。
`nomicore doctor` 与 `mcp-*` stdio 子命令不取这把锁，因此不受影响。

## Docker 专项

### `docker compose up` 完成构建并启动后立刻退出

请阅读日志（`docker compose logs nomifun`）。最常见的原因有：

- 数据卷为空*而且*没有设置 `NOMIFUN_ADMIN_PASSWORD`——服务运行
  正常，但你必须先通过 HTTP 完成首次启动设置才能进得去。这其实不是
  失败，而是一种状态。
- 镜像内 `--dist` 路径指错。官方 Dockerfile 把 `ui/dist` 复制到
  `/opt/nomifun/web`，`CMD` 也据此引用——只在你定制了 Dockerfile
  时才会出问题。
- 一个被 bind 挂载的、容器用户无写权限的数据目录。

### 日志显示 `nomifun-web: embedded backend + SPA on one port` 但浏览器无法连接

请确认端口映射（`docker compose ps`）。默认 compose 文件发布
`8787:8787`；如果你在前面放了 Caddy，应改为 `expose: ["8787"]`。
连错端口是最常见的元凶。

### 在国内网络或企业代理后构建很慢或失败

先看失败日志所在的阶段：

- `load metadata for docker.io/...` / `TLS handshake timeout`：基础镜像尚未
  拉取成功。配置 Docker daemon registry mirror，或设置 `BUN_IMAGE`、
  `RUST_IMAGE`、`RUNTIME_IMAGE` 为可信的兼容镜像地址。
- `apt-get` / `bun install` / `Updating crates.io index`：分别设置
  `APT_MIRROR`、`BUN_REGISTRY`、`CARGO_REGISTRY_MIRROR`。

Compose 可以一次传入这些参数：

```bash
APT_MIRROR=http://mirrors.aliyun.com \
CARGO_REGISTRY_MIRROR=https://rsproxy.cn/index/ \
BUN_REGISTRY=<可用的-npm-镜像地址> \
docker compose up -d --build
```

Dockerfile 已为 apt 与 Cargo 启用重试，并使用更小的 Rust slim 基础镜像；
上述参数仍应指向部署者信任且实际可达的镜像。

## 日志

后端**同时**写入 stdout 与 `<log-dir>/nomicore.log` 上的按日滚动文件
（默认 `<data-dir>/logs`）。出问题时：

- `journalctl -u nomifun-web` / `docker compose logs nomifun` 视图能
  看到最近几分钟的日志。
- `<log-dir>` 下的滚动文件保存了历史。
- 把受影响模块的级别拉高：MCP 问题用
  `--log-level info,nomifun_mcp=trace`，终端用
  `info,nomifun_terminal=debug`，智能体会话用
  `info,nomifun_conversation=debug`。

## 当一切都不奏效

读源码。每个路由 handler 都在其归属 crate 的 `routes.rs`（或
`routes/`）文件里；装配在
`crates/backend/nomifun-app/src/router/routes.rs`。handler 抛出的错误
信息就是 HTTP 响应里那串字面量字符串，所以一次精确 grep 通常几秒就
能落到出错的检查上。

## 另见

- [配置参考](./configuration.zh.md) —— 每个参数与环境变量。
- [API 概览](./api-overview.zh.md) —— 路由、鉴权与 WebSocket 模型导览。
- [常见问题](./faq.zh.md) —— 那些最常见的"X 是这样吗？"的简短回答。
