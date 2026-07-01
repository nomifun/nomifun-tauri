# NomiFun 发版手册

本文面向实际发版操作。需要英文维护说明时看 `RELEASING.md`；日常发桌面版优先按本文执行。

## 核心概念

桌面发版有两类产物：

- **手动安装包**：用户从 GitHub Releases 下载后自己安装，例如 macOS `.dmg`、Windows `.exe` / `.msi`、Linux `.AppImage` / `.deb` / `.rpm`。
- **自动更新产物**：Tauri updater 使用的包、对应 `.sig` 签名、以及合并后的 `latest.json`。

自动更新签名和系统代码签名不是一回事：

- `TAURI_SIGNING_PRIVATE_KEY` 只用于 Tauri updater 验签，证明自动更新包没有被篡改。
- macOS Developer ID / 公证、Windows Authenticode 用于系统信任，影响 Gatekeeper、SmartScreen、未知发布者提示。
- Windows 当前没有 Authenticode 签名时，自动更新验签仍可工作，但手动安装包会有未知发布者 / SmartScreen 风险。

## 版本号

版本号的单一真源是根目录 `Cargo.toml` 的 `[workspace.package].version`。发版前只需要跑：

```bash
VERSION=1.2.3
bun run bump "$VERSION"
```

脚本会同步：

- `Cargo.toml`
- `Cargo.lock`
- 根 `package.json`
- `ui/package.json`

tag 统一使用 `vX.Y.Z`，例如 `v0.1.11`。

## macOS 发版

必须在 Mac 上执行。一键脚本会自动判定两种场景：

- **追加(APPEND)**：该版本的 GitHub Release 已存在（可能 Windows 侧先发过）——只补 macOS
  产物、把 `darwin-x86_64` / `darwin-aarch64` 条目并进 `latest.json`。
- **首发(CREATE)**：该版本还没有 Release（macOS 先发）——建 tag、建 Release（带 release
  note）、上传 macOS 产物；可用 `-Version` 顺带打版本号。

### 一键发版（推荐）

一次性配置好后即可反复一键：

1. 从密钥库拷入 updater 私钥 `apps/desktop/signing/nomifun-updater.key`（keyID
   `F3AA272E60AA7952`，已被 gitignore，必须与 `tauri.conf.json` 内嵌 `pubkey` 匹配）。
2. 配置 `apps/desktop/signing/.env.signing`，填好 Developer ID 签名与 Apple notarization 信息。
3. 运行 `gh auth login`，或复制 `apps/desktop/signing/.env.release.example` 为 `.env.release`
   并填入 `GH_TOKEN=...`。

然后：

```bash
git pull

# 追加（Windows 已先发，补 macOS）：
bun run release:mac

# 首发（macOS 先发）：-Version 顺带 bump，-NotesFile/-Notes 提供 release note（首发必填）
bun run release:mac -Version 0.1.14 -NotesFile notes.md
bun run release:mac -Version 0.1.14 -Notes "- 修复若干问题"

# 通用开关：
bun run release:mac -DryRun     # 只读预检并打印计划（含判定出的模式），不 pull/bump/构建/上传/推送
bun run release:mac -NoPush     # APPEND：仍上传但不提交推送；CREATE：只本地 bump/构建，不建 Release
bun run release:mac -SkipPull   # 真实执行时跳过 git pull
```

**版本号**：单一真源是根 `Cargo.toml [workspace.package].version`。追加模式用当前版本；首发
时若带 `-Version X.Y.Z` 且与当前不同，脚本先跑 `bun run bump` 打版本号，再以 `nomifun`
署名提交、打 tag、建 Release（不带 `-Version` 则按当前版本走）。

**Release note（对 LLM 友好）**：走命令行、不与 CHANGELOG 耦合，且 **GitHub Release 正文与
`latest.json` 的 `notes` 共用同一份**。多行说明建议先写到 `.md` 再用 `-NotesFile` 传入。
**首发若既没 `-NotesFile` 也没 `-Notes`，脚本直接报错，不会建出空说明的 Release**；
追加模式 note 可选。

脚本自动完成：读 `.env.release` 的 `GH_TOKEN`（若存在）或沿用 `gh auth login`、加载 updater
私钥、判定模式、执行 `build:mac --signed --config apps/desktop/tauri.updater.conf.json`、校验
staple / codesign / Gatekeeper、`make:latest` 合并 darwin 条目、上传（追加 `--clobber` / 首发
`gh release create`）、以 `nomifun` 提交 `latest.json`（首发含 bump）回 `main`、拉取 updater
端点校验。任何一步失败都会明确报错并中断。

### 手动分步（等价于一键脚本内部流程）

下面命令会同时产出：

- 手动安装包：`dist/desktop/NomiFun_<version>_universal.dmg`
- 自动更新包：`target/universal-apple-darwin/release/bundle/macos/NomiFun.app.tar.gz`
- 自动更新签名：`target/universal-apple-darwin/release/bundle/macos/NomiFun.app.tar.gz.sig`

```bash
export TAURI_SIGNING_PRIVATE_KEY="$(cat apps/desktop/signing/nomifun-updater.key)"
export TAURI_SIGNING_PRIVATE_KEY_PASSWORD=""

bun run build:mac --config apps/desktop/tauri.updater.conf.json
bun run make:latest
```

如果是公开分发，建议配置 `apps/desktop/signing/.env.signing` 后使用 Developer ID 签名和公证：

```bash
export TAURI_SIGNING_PRIVATE_KEY="$(cat apps/desktop/signing/nomifun-updater.key)"
export TAURI_SIGNING_PRIVATE_KEY_PASSWORD=""

bun run build:mac --signed --config apps/desktop/tauri.updater.conf.json
bun run make:latest
```

`bun run make:latest` 会把 macOS 的 `darwin-x86_64` 和 `darwin-aarch64` 都写入 `apps/desktop/updater/latest.json`。

## Windows 发版

必须在 Windows 机器上执行。一键脚本会自动判定两种场景：

- **追加(APPEND)**：该版本的 GitHub Release 已存在（通常 macOS 侧先发过）——只补 Windows
  产物、把 windows 条目并进 `latest.json`。
- **首发(CREATE)**：该版本还没有 Release（Windows 先发）——建 tag、建 Release（带 release
  note）、上传 Windows 产物；可用 `-Version` 顺带打版本号。

### 一键发版（推荐）

一次性配置好后即可反复一键：

1. 从密钥库拷入 updater 私钥 `apps/desktop/signing/nomifun-updater.key`（keyID
   `F3AA272E60AA7952`，已被 gitignore，必须与 `tauri.conf.json` 内嵌 `pubkey` 匹配）。
2. 复制 `apps/desktop/signing/.env.release.example` 为 `.env.release`（已被 gitignore），
   填入 `GH_TOKEN=...`（`repo` 权限的经典 PAT，或对本仓库 Contents:read/write 的细粒度 PAT）。

然后：

```powershell
git pull

# 追加（macOS 已先发，补 Windows）：
bun run release:win

# 首发（Windows 先发）：-Version 顺带 bump，-NotesFile/-Notes 提供 release note（首发必填）
bun run release:win -Version 0.1.14 -NotesFile notes.md
bun run release:win -Version 0.1.14 -Notes "- 修复若干问题"

# 通用开关：
bun run release:win -DryRun     # 只读预检并打印计划（含判定出的模式），不 bump/构建/上传/推送
bun run release:win -NoPush     # APPEND：仍上传但不提交推送；CREATE：只本地 bump/构建，不建 Release
bun run release:win -SkipPull   # 跳过 git pull
```

**版本号**：单一真源是根 `Cargo.toml [workspace.package].version`。追加模式用当前版本；首发
时若带 `-Version X.Y.Z` 且与当前不同，脚本先跑 `bun run bump` 打版本号，再以 `nomifun`
署名提交、打 tag、建 Release（不带 `-Version` 则按当前版本走）。

**Release note（对 LLM 友好）**：走命令行、不与 CHANGELOG 耦合，且 **GitHub Release 正文与
`latest.json` 的 `notes` 共用同一份**。多行说明建议先写到 `.md` 再用 `-NotesFile` 传入
（PowerShell 传多行参数不稳，脚本内部也用文件中转）。**首发若既没 `-NotesFile` 也没
`-Notes`，脚本直接报错，不会建出空说明的 Release**；追加模式 note 可选（不传则沿用既有，
`latest.json notes` 由 CHANGELOG 当前版本小节兜底）。

脚本自动完成：读 `.env.release` 的 `GH_TOKEN` 注入 `gh`、加载签名私钥、判定模式、清理旧版本
残留 NSIS 产物、构建更新产物、`make:latest` 合并 windows 条目、上传（追加 `--clobber` / 首发
`gh release create`）、以 `nomifun` 提交 `latest.json`（首发含 bump）回 `main`、拉取 updater
端点校验。任何一步失败都会明确报错并中断。

> 未启用 Authenticode 时，手动安装包仍可能触发 SmartScreen / 未知发布者提示；自动更新验签
> 走 Tauri updater 的 minisign 签名，与 Authenticode 无关，一键脚本已覆盖。

### 手动分步（等价于一键脚本内部流程）

先拉到与当前 Release 一致的代码：

```powershell
git pull
git checkout main
```

如果是在已经发布 macOS 后补 Windows，确认 `Cargo.toml` 版本号与现有 GitHub Release 一致。

> updater 私钥 `apps/desktop/signing/nomifun-updater.key` 已被 gitignore、只存在于密钥库。
> 这台 Windows 构建前需先从密钥库把它拷过来，且必须与 `tauri.conf.json` 内嵌的 `pubkey`
> 匹配（keyID `F3AA272E60AA7952`），否则已安装的客户端会拒绝更新。叠加 `createUpdaterArtifacts`
> 用的是仓库内的 `apps/desktop/tauri.updater.conf.json`，以 `--config <文件路径>` 传入——
> **不要内联 JSON**，PowerShell 5.1 会把内联 `--config '{...}'` 的双引号剥掉变成非法 JSON。

### 当前无 Authenticode 签名的做法

```powershell
$env:TAURI_SIGNING_PRIVATE_KEY = Get-Content apps/desktop/signing/nomifun-updater.key -Raw
$env:TAURI_SIGNING_PRIVATE_KEY_PASSWORD = ""

bun run build:win --config apps/desktop/tauri.updater.conf.json
bun run make:latest
```

这会生成 Windows 自动更新产物和 `.sig`。这种模式下：

- 自动更新验签可以工作。
- 手动安装包不是系统代码签名包，用户可能看到 SmartScreen / 未知发布者提示。
- 适合内部测试或临时发布，不等同于公开可信 Windows 安装包。

### 以后补 Authenticode 签名

拿到 Windows 代码签名证书后，先把证书导入当前用户证书库，再设置证书指纹（`--signed`
注入指纹仍走内联 JSON，需在 pwsh 7+ 下运行）：

```powershell
$env:TAURI_SIGNING_PRIVATE_KEY = Get-Content apps/desktop/signing/nomifun-updater.key -Raw
$env:TAURI_SIGNING_PRIVATE_KEY_PASSWORD = ""
$env:WINDOWS_CERTIFICATE_THUMBPRINT = "A1B2C3..."

bun run build:win --signed --config apps/desktop/tauri.updater.conf.json
bun run make:latest
```

这才是更接近 macOS Developer ID 签名 / 公证的公开分发状态。

## Linux 发版

如果发布 Linux，必须在 Linux 机器上执行：

```bash
export TAURI_SIGNING_PRIVATE_KEY="$(cat apps/desktop/signing/nomifun-updater.key)"
export TAURI_SIGNING_PRIVATE_KEY_PASSWORD=""

bun run build:linux --config apps/desktop/tauri.updater.conf.json
bun run make:latest
```

Linux 不走 macOS 公证或 Windows Authenticode，但仍需要 Tauri updater `.sig`。

## 合并 latest.json

`bun run make:latest` 的逻辑是：

- 扫描当前机器生成的 updater 产物和 `.sig`。
- 写入当前平台对应的 `platforms[...]` 条目。
- 保留已有的真实平台条目。

因此多平台发版时，要让 `apps/desktop/updater/latest.json` 在各平台之间传递或提交回仓库。缺失某个平台条目时，该平台用户不会收到自动更新。

典型顺序：

1. Mac 生成 macOS 条目。
2. Windows 拉取包含 macOS 条目的 `latest.json`。
3. Windows 跑 `bun run make:latest` 后补 Windows 条目。
4. 把最终 `latest.json` 上传到 GitHub Release，并提交回 `main`。

## 创建 GitHub Release

如果是首次创建某个版本：

```bash
git add Cargo.toml Cargo.lock package.json ui/package.json apps/desktop/updater/latest.json
git commit -m "chore(release): v$VERSION"
git tag "v$VERSION"
git push origin main "v$VERSION"

gh release create "v$VERSION" \
  target/universal-apple-darwin/release/bundle/macos/NomiFun.app.tar.gz \
  target/universal-apple-darwin/release/bundle/macos/NomiFun.app.tar.gz.sig \
  dist/desktop/NomiFun_${VERSION}_universal.dmg \
  apps/desktop/updater/latest.json \
  --title "v$VERSION" \
  --notes "发布说明"
```

如果 Release 已存在，只是补传 Windows 或 Linux 资产：

```bash
gh release upload "v$VERSION" <new-assets...>
gh release upload "v$VERSION" apps/desktop/updater/latest.json --clobber
```

`--clobber` 用于替换已有的 `latest.json`，确保 GitHub Release 上的清单包含最新平台条目。

## 上传哪些文件

macOS 至少上传：

```text
dist/desktop/NomiFun_<version>_universal.dmg
target/universal-apple-darwin/release/bundle/macos/NomiFun.app.tar.gz
target/universal-apple-darwin/release/bundle/macos/NomiFun.app.tar.gz.sig
apps/desktop/updater/latest.json
```

Windows 上传 `bun run make:latest` 打印的 updater 包、`.sig`、`latest.json`。如果 `dist/desktop/` 里还有未包含的手动安装包，例如 `.msi`，也一起上传。

Linux 上传对应安装包、`.sig`、`latest.json`。

## 发布后验证

```bash
gh release view "v$VERSION" --json tagName,assets,url
curl -fsSL https://github.com/nomifun/nomifun-tauri/releases/latest/download/latest.json
```

确认：

- GitHub Release 资产里包含手动安装包、updater 包、`.sig`、`latest.json`。
- `latest.json` 的 `version` 等于本次版本。
- 每个已发布平台都有 `platforms[...]` 条目。
- 每个 URL 都指向同一个 `v$VERSION` Release。

## v0.1.11 当前状态

`v0.1.11` 已完成 macOS：

- 已上传 `NomiFun_0.1.11_universal.dmg`。
- 已上传 `NomiFun.app.tar.gz` 和 `NomiFun.app.tar.gz.sig`。
- `latest.json` 目前只有 `darwin-x86_64` 和 `darwin-aarch64`。

Windows 还需要在 Windows 机器上继续构建、上传 Windows 资产，并用 `--clobber` 替换 Release 上的 `latest.json`。
