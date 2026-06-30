# NomiFun 桌面端自动更新说明

本文只说明自动更新链路。完整发版操作看根目录 `RELEASING.zh-CN.md`。

## 工作方式

应用内自动更新基于 Tauri 原生 updater：

```text
正在运行的 App
  -> 请求 apps/desktop/tauri.conf.json 里的 updater endpoint
  -> 下载 GitHub Releases 上的 latest.json
  -> 判断是否有更高版本
  -> 下载当前平台对应的更新包
  -> 用内置 pubkey 校验 .sig
  -> 安装并重启
```

当前 endpoint：

```text
https://github.com/nomifun/nomifun-tauri/releases/latest/download/latest.json
```

## 密钥区别

自动更新使用一把 Tauri updater 私钥：

```text
apps/desktop/signing/nomifun-updater.key
```

发版时把私钥内容写入环境变量：

```bash
export TAURI_SIGNING_PRIVATE_KEY="$(cat apps/desktop/signing/nomifun-updater.key)"
export TAURI_SIGNING_PRIVATE_KEY_PASSWORD=""
```

这把密钥只负责 updater 验签，不负责系统信任：

- macOS Gatekeeper 仍需要 Developer ID 签名和公证。
- Windows SmartScreen / 未知发布者仍需要 Authenticode 签名。
- 没有 OS 代码签名时，自动更新验签仍可工作，但手动安装体验不够可信。

## 构建自动更新产物

macOS：

```bash
export TAURI_SIGNING_PRIVATE_KEY="$(cat apps/desktop/signing/nomifun-updater.key)"
export TAURI_SIGNING_PRIVATE_KEY_PASSWORD=""

bun run build:mac --config '{"bundle":{"createUpdaterArtifacts":true}}'
bun run make:latest
```

Windows 无 Authenticode 签名：

```powershell
$env:TAURI_SIGNING_PRIVATE_KEY = Get-Content apps/desktop/signing/nomifun-updater.key -Raw
$env:TAURI_SIGNING_PRIVATE_KEY_PASSWORD = ""

bun run build:win -- --config '{"bundle":{"createUpdaterArtifacts":true}}'
bun run make:latest
```

Windows 有 Authenticode 签名：

```powershell
$env:TAURI_SIGNING_PRIVATE_KEY = Get-Content apps/desktop/signing/nomifun-updater.key -Raw
$env:TAURI_SIGNING_PRIVATE_KEY_PASSWORD = ""
$env:WINDOWS_CERTIFICATE_THUMBPRINT = "A1B2C3..."

bun run build:win --signed -- --config '{"bundle":{"createUpdaterArtifacts":true}}'
bun run make:latest
```

Linux：

```bash
export TAURI_SIGNING_PRIVATE_KEY="$(cat apps/desktop/signing/nomifun-updater.key)"
export TAURI_SIGNING_PRIVATE_KEY_PASSWORD=""

bun run build:linux -- --config '{"bundle":{"createUpdaterArtifacts":true}}'
bun run make:latest
```

## latest.json

`bun run make:latest` 会扫描当前机器的 updater 产物和 `.sig`，把对应平台写入：

```text
apps/desktop/updater/latest.json
```

同一个版本如果分多台机器构建，需要把最新的 `latest.json` 带到下一台机器继续合并。最终上传到 GitHub Release 的 `latest.json` 必须包含所有已发布平台。

## GitHub Release 资产

macOS 需要同时上传：

```text
dist/desktop/NomiFun_<version>_universal.dmg
target/universal-apple-darwin/release/bundle/macos/NomiFun.app.tar.gz
target/universal-apple-darwin/release/bundle/macos/NomiFun.app.tar.gz.sig
apps/desktop/updater/latest.json
```

Windows 上传 `bun run make:latest` 打印的 updater 包、`.sig`、`latest.json`。如果还有额外手动安装包，例如 `.msi`，也上传。

如果 Release 已经存在，补平台时用：

```bash
gh release upload "v<version>" <new-assets...>
gh release upload "v<version>" apps/desktop/updater/latest.json --clobber
```

## 验证

```bash
gh release view "v<version>" --json tagName,assets,url
curl -fsSL https://github.com/nomifun/nomifun-tauri/releases/latest/download/latest.json
```

确认 `latest.json` 的版本、平台 key、URL 和 Release 资产一致。
