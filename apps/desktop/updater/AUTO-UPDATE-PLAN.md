# 自动更新 + 版本号联动 —— 设计与交接文档

> 用途：本文件记录"应用内自动更新（Tauri 原生）+ 版本号单一真源"这项工作的**决策、调研结论、已完成项、待办计划、开放问题**，以及**迁移到 macOS 继续**时的注意事项。
> 在 Mac 上 `git pull` 即可获得本文件。当前进度：**版本号联动 + 应用内自动更新代码均已落地（macOS 续做）；剩下的是各平台分别构建签名产物并发首个 GitHub Release。**

---

## 0. 一句话现状

- ✅ **版本号单一真源 + `bun run bump` 一键改版**：已实现、已推送 main，当前版本 `0.1.10`。
- ✅ **Windows 单一 NSIS 安装包 + `build:win` 兼容 PS 5.1**：已上线。
- ✅ **应用内自动更新（代码已写完）**：前端已接通 `@tauri-apps/plugin-updater`，关于页"检查更新"按钮 + 启动静默检查均可用，版本号改读真实值；签名密钥已生成、公钥入库、endpoint 指向 GitHub Releases；新增 `bun run make:latest` 生成/合并清单。详见 §8。
- 🔜 **剩余手动步骤**：在各平台构建机上分别出签名更新产物 → `make:latest` 合并 `latest.json` → 建首个 `vX.Y.Z` GitHub Release 上传产物。见 §8 末。

> 本次（macOS 续做）落地的全部改动见文末 **§8 本次完成**。

---

## 1. 已完成（已合并并推送到 main）

| 主题 | 提交（subject） | 要点 |
|---|---|---|
| 版本号单一真源 | `chore(release): 版本号统一到单一真源 + bun run bump 一键改版（0.1.0→0.1.10）` | 见 §2 |
| Windows 单包 | `fix(build): Windows 仅产单一 NSIS 安装包，收敛分散的打包产物` | `tauri.conf.json` `bundle.targets` 由 `"all"`→`["nsis"]` |
| build:win 兼容 5.1 | `fix(build): bun run build:win 支持无 pwsh 的 PowerShell 5.1 环境` | 启动器优先 pwsh 回退 powershell + `.ps1` 加 UTF-8 BOM + `$IsWindows` 兼容 |

**版本号单一真源机制（已生效，全平台通用）：**
- **唯一真源 = 根 `Cargo.toml` 的 `[workspace.package].version`**。后端 `CARGO_PKG_VERSION` / `app_version` 本就跟随它。
- `apps/desktop/tauri.conf.json` **已删除 `version` 字段** → Tauri 自动继承 workspace 版本（已验证：安装包名随之变 `NomiFun_0.1.10_x64-setup.exe`）。
- 一键改版：`bun run bump <x.y.z> [--tag]`（`scripts/bump-version.mjs`）—— 改 `Cargo.toml` + `package.json` + `ui/package.json` + 同步 `Cargo.lock`，`--tag` 时额外 commit + 打 `vX.Y.Z`（需干净工作树）。已登记进 `scripts/scripts.json`，`bun run help --check` 通过。
- 三平台打包脚本都汇总到 `dist/desktop/`：`build:win`(NSIS .exe) / `build:mac`(.dmg) / `build:linux`(.deb/.AppImage/.rpm)。

---

## 2. 关键决策（已和需求方确认）

1. **更新方式：Tauri 原生自动更新**（GitHub Releases 托管 `latest.json` + 签名安装包；应用内检查→下载→验签→自动安装）。
2. **签名密钥：助手代生成，私钥留在用户机器**——私钥写进 `.gitignore` 忽略的文件、不打印不入库；公钥填进 `tauri.conf.json`；用户负责备份私钥 + 存入发布密钥库。
3. **托管：GitHub Releases**（`latest.json` 与各平台安装包都作为 Release 资产）。仓库 = `git@github.com:nomifun/nomifun-tauri.git`。
4. **`latest.json` 生成：写一个发版脚本，可在各平台分别运行、把本平台条目合并进同一个 `latest.json`**（为将来上 CI 矩阵做准备）。

### 关于"签名密钥"的澄清（避免再混淆）
- 一对 **更新签名密钥**（minisign/Ed25519）：**私钥**只你持有、发版时签名；**公钥**嵌进 App、用于验签。**一把通吃所有平台**（win/mac/linux 共用一把），**和 GitHub / Tauri 官方都无关**。
- 它**只服务于"应用内自动更新"**，不影响手动下载安装、不影响 App 功能、**不能**去掉"未知发布者"警告。
- 私钥**不能复现**，只能**备份带走**；丢了 → 已装的老用户收不到自动更新（需手动重装一次），App 本身不坏。现阶段（尚未对外发过签名更新）丢了几乎无损失。
- 三种别混：① 更新签名密钥（本项目，一把通吃，与 GitHub 无关）；② GitHub 上传凭据（`gh`/token，仅发版上传用，不入 App）；③ 系统代码签名证书（Win Authenticode / mac Developer ID，分平台、去"未知发布者"警告，**另一回事**）。

---

## 3. 调研结论：当前 Tauri 下更新链路是"死"的（实现前必须知道）

- **About 页"检查更新"按钮**：被 `isElectron` 门控，而 `isElectronDesktop()` 在 `ui/src/renderer/utils/platform.ts:28` **硬编码 `false`** → **不渲染**。
- **系统托盘**：Tauri 原生托盘只有 `Show`/`Quit`（`apps/desktop/src/main.rs:690-692`），**无"检查更新"项**；`tray:check-update`（`Layout.tsx:398`）是 Electron 遗留、永不触发。
- **`ipcBridge.update.*` / `autoUpdate.*`**：`ui/src/common/adapter/ipcBridge.ts:566-600` 全是 **stub**（`{success:false}` / noop emitter）。
- **`check_for_updates` Tauri 命令**：`main.rs:150` 存在且用了 `tauri-plugin-updater`，但**前端从未调用它**（全仓只在注释里出现）。
- **结论**：当前既**没有可见的更新入口、也没有任何报错**（所以最初说的"消除报错按钮"是空操作），**更没有任何可用的应用内更新**。要做自动更新，**必须先把 UI 接到 `check_for_updates` / `@tauri-apps/plugin-updater`**。
- **配置现状**：`tauri.conf.json` `plugins.updater.endpoints` = 占位 `https://REPLACE-WITH-YOUR-HOST/...`；`pubkey` = 开发占位值（私钥不在手）；`apps/desktop/updater/latest.json` = 模板（签名/URL 都是占位）。
- **附带 bug**：About 页版本号**硬编码 `ABOUT_SYSTEM_VERSION = '1.0.0'`**（`AboutModalContent.tsx:17,114`），与真实版本不符，实现时应改成读真实版本。
- **次要不一致**：后端 `VersionCheckService` 默认仓库是 `nomifun/nomifun-app`（`nomifun-system/src/version.rs:5`），与实际仓库 `nomifun/nomifun-tauri` 不一致；本方案走 tauri-plugin-updater，不依赖它，但若以后启用该路径需对齐。

---

## 4. 实现计划（在 Mac 上继续）

### A. 把更新 UI 接到 Tauri updater
- 加一个在 Tauri 下**真正可用**的"检查更新"入口：在 About 页放按钮（**去掉 `isElectron` 门控**）；建议另加**启动后静默检查一次**。
- 前端用 `@tauri-apps/plugin-updater` 的 `check()` + `downloadAndInstall()`，或调用现有 `check_for_updates` 命令并补一个下载/安装命令。
- 交互：检测到更高版本 → 弹"发现新版本 X" → 用户确认 → 下载（进度）→ 验签 → 安装 → 重启。
- 把 `UpdateModal.tsx` 现有的 electron-updater stub 流程改造为走 tauri-plugin-updater。
- 修 About 页版本号：改为读真实版本（`@tauri-apps/api/app` 的 `getVersion()` 或后端 `/health`），删掉硬编码 `1.0.0`。

### B. 签名密钥（在"将用于发版的机器"上生成）
```bash
bun x tauri signer generate -w apps/desktop/signing/nomifun-updater.key   # 密码可留空
```
- 私钥文件 `*.key` 已被 `.gitignore` 忽略（`.gitignore:30`）；**不入库、不打印**。
- 把打印出的**公钥**填进 `tauri.conf.json` → `plugins.updater.pubkey`（替换占位 dev 值）。
- **备份私钥** + 存入发布密钥库；发版时设 `TAURI_SIGNING_PRIVATE_KEY`（+ 可选 `TAURI_SIGNING_PRIVATE_KEY_PASSWORD`）供 `bun run build:updater` 用。

### C. GitHub Releases 托管 + endpoint
- `tauri.conf.json` → `plugins.updater.endpoints` 改为 GitHub Releases 的 `latest.json` 稳定地址：
  `https://github.com/nomifun/nomifun-tauri/releases/latest/download/latest.json`
- 每次 Release 上传：各平台安装包 + 各自 `.sig` + 一个 `latest.json` 资产。

### D. 发版脚本：生成/合并 `latest.json`
- 新增 `scripts/make-latest-json.mjs`（并登记进 `scripts/scripts.json`，否则 `bun run help --check` 红灯）：
  - 读单一真源版本；扫描 `dist/desktop`（或 target bundle）里**本平台**的 updater 产物 + `.sig`；把对应 `platforms[<os>-<arch>]` 条目（`url` + `signature`）**合并**进 `latest.json`（在哪台机器跑就补哪个平台，最终汇总成完整清单）。
  - 接进 `build:updater` 之后或一个新的 `release` 脚本。
- 更新 `RELEASING.md` + `apps/desktop/updater/README.md` 为 GitHub-Releases 流程。

### E. 跨平台/多芯片要点（Tauri updater 原生支持，分发由客户端自动完成）
- `latest.json` 的 `platforms` 表按 `<系统>-<芯片>` 分键（`windows-x86_64` / `windows-aarch64` / `darwin-x86_64` / `darwin-aarch64` / `linux-x86_64` …），**每条各有自己的 `url`+`signature`**；用户 App 按自己的 target triple **自动只取对应那条**，不会拿错。
- **一把私钥签所有平台所有芯片**；每个产物各自一个 `.sig`。
- **macOS 的更新产物是 `.app.tar.gz`**（不是 `.dmg`；`.dmg` 仅首次安装用）；Universal 胖包可一个产物同时填 `darwin-x86_64` 和 `darwin-aarch64` 两条。
- **不能交叉编译**：win 包在 Windows 上、mac 包在 macOS 上、linux 包在 Linux 上构建。要么在各平台分别构建再合并 `latest.json`，要么上 **GitHub Actions 矩阵**（长期推荐）。
- **风险**：`latest.json` 漏某个 `系统-芯片` 条目 → 那批用户"静默收不到更新"。支持哪些平台/芯片，就必须有对应几条。

---

## 5. 开放问题（需求方需拍板，决定实现细节）

1. **要支持哪些平台 + 芯片？**（如 Windows x64 / Windows arm64 / macOS Universal（或 x64+arm64）/ Linux x64 / Linux arm64）→ 决定 `latest.json` 条目 + 需在哪些机器构建。
2. **检查节奏**：启动静默检查一次 + 关于页手动按钮？还是只手动？（建议二者都有）
3. **签名私钥是否设密码**：留空更利于自动化（私钥文件本身要保护好）。
4. **确认发布仓库** = `nomifun/nomifun-tauri`（默认按此）。
5. **系统代码签名**（Authenticode / Developer ID + 公证）现在做还是以后？——独立于自动更新，影响"未知发布者"/Gatekeeper，不影响更新链路。

---

## 6. 迁移到 macOS 继续的注意事项

- mac 打包：`bun run build:mac`（默认 Universal `.dmg`）。**自动更新产物需 `bun run build:updater`**（开 `createUpdaterArtifacts`）→ 产出 `.app.tar.gz` + `.sig`。
- mac 代码签名/公证配置在 `apps/desktop/signing/`（`.env.signing.example` + README），**与更新签名是两回事**。
- **签名密钥**：既然后续在 Mac 上推进，可直接**在 Mac 上生成**那把更新密钥（§4-B）。注意"一把私钥签全平台"——以后若在多台机器/CI 上分别构建各平台，**每台构建机都要能拿到同一把私钥**。
- `bun run build:win` **在 Mac 上不能用**（仅 Windows）；Mac 上用 `build:mac`/`build:linux`。
- Windows 那台已验证可出 `NomiFun_0.1.10_x64-setup.exe`；mac/linux 包还没在对应系统上构建过。

---

## 7. 校验当前状态

- 当前版本：`0.1.10`（`Cargo.toml [workspace.package].version`；`tauri.conf.json` 无 version 字段、靠继承）。
- `bun run help --check`、`bun run typecheck` 均通过。

---

## 8. 本次完成（macOS 续做）——§4 A–E 已落地

**前端（已接通 tauri-plugin-updater，纯前端、可类型检查）：**
- 新增 `ui/src/common/adapter/tauriUpdater.ts`：封装 `@tauri-apps/plugin-updater`（check/download/install）+ `plugin-process`（relaunch），含进度/速率换算、单一 `Update` 资源句柄全程复用、两次 check 合并为一次网络请求、本地状态 emitter。
- `ui/src/common/adapter/ipcBridge.ts`：`update.*` / `autoUpdate.*` 由 stub 改为走 `tauriUpdater`（`update.check` 给当前版本+发布说明，`autoUpdate.check/download/quitAndInstall` 驱动下载安装）。**复用现有 `UpdateModal.tsx`，未改其 UI**（它在 `recommendedAsset` 缺省时本就走 autoUpdate 路径）。
- 关于页 `AboutModalContent.tsx`：去掉永远为 false 的 `isElectron` 门控 → 改 `isDesktopShell()`，桌面壳下渲染"检查更新"；版本号从硬编码 `1.0.0` 改为读后端 `GET /health` 的真实版本（桌面壳与 WebUI 浏览器都准）。
- `Layout.tsx`：新增**启动后静默检查一次**（仅桌面壳），有新版才弹窗，无版本/离线/出错都静默。

**配置 / 密钥：**
- 已在本机生成更新签名密钥：私钥 `apps/desktop/signing/nomifun-updater.key`（`*.key` 已 gitignore，**未打印、未入库**），公钥写入 `tauri.conf.json` → `plugins.updater.pubkey`。**⚠️ 请尽快备份私钥到发布密钥库**（口令留空，§5-3 默认）。
- `tauri.conf.json` → `plugins.updater.endpoints` 改为 `https://github.com/nomifun/nomifun-tauri/releases/latest/download/latest.json`。
- `.gitignore` 增 `*.key.pub`（公钥本地副本不入库；真源在 tauri.conf.json）。

**发版脚本 / 文档：**
- 新增 `scripts/make-latest-json.mjs` + `bun run make:latest`（已登记 scripts.json，`help --check` 通过）：扫本机 `target/` 更新产物→按 `<os>-<arch>` 合并进 `apps/desktop/updater/latest.json`（universal mac 包自动填两个 darwin 键；合并保留它机条目；报告缺失键）。已用合成产物验证扫描/映射/合并逻辑。
- 重写 `apps/desktop/updater/README.md` + `RELEASING.md` 桌面发版段为 GitHub-Releases + make:latest 流程；`latest.json` 模板改成 GitHub URL 占位样例。

**§5 开放问题——本次采用的默认（可随时改）：**
1. 平台/芯片：代码与 `make:latest` **平台无关**，模板覆盖 win-x64 / darwin-x64 / darwin-aarch64 / linux-x64。建议 macOS 出 **Universal**（一包通吃两芯片）。实际出哪些由你在各构建机上跑决定。
2. 检查节奏：**启动静默检查 + 关于页手动按钮**（两者都做）。
3. 私钥口令：**留空**（利于自动化）。想要口令就重新生成并设 `TAURI_SIGNING_PRIVATE_KEY_PASSWORD`。
4. 仓库：`nomifun/nomifun-tauri`（与 `git remote` 一致）。
5. 系统代码签名：仍**独立于自动更新**，未在本次处理（mac 走 `build:signed`/`build:mac --signed`，见 signing/README）。

**剩余手动步骤（需要真机 + 你的发布账号）：**
1. 各平台构建机：`export TAURI_SIGNING_PRIVATE_KEY="$(cat apps/desktop/signing/nomifun-updater.key)"` → 出签名更新产物（mac: `build:mac --config '{"bundle":{"createUpdaterArtifacts":true}}'`；win/linux: `build:updater`）。
2. 各机跑 `bun run make:latest` 合并 `latest.json`（把该文件在机器间传递/提交）。
3. 建首个 `vX.Y.Z` GitHub Release，上传所有安装包 + `.sig` + `latest.json`。
4. 装一份旧版、发个更高版本，验证"启动静默检查→弹窗→下载→验签→安装→重启"全链路。

> 注：后端 `nomifun-system/src/version.rs` 那套旧 `VersionCheckService`（默认仓库 `nomifun/nomifun-app`）与本方案**无关**（本方案不经它）；前端更新检查已全部走 tauri-plugin-updater。如未来要启用该后端路径再对齐其默认仓库。
