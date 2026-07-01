# ============================================================================
# 一键 Windows 发版。自动判定两种场景：
#
#   追加(APPEND)：该版本的 GitHub Release 已存在（通常 macOS 侧先发过）——只补 Windows
#                 产物并把 windows 条目合并进 latest.json。
#   首发(CREATE)：该版本还没有 Release（Windows 先发）——建 tag、建 Release（带 release
#                 note）、上传 Windows 产物。可用 -Version 顺带打版本号。
#
#   bun run release:win                          # 用当前 Cargo.toml 版本；自动判定模式
#   bun run release:win -Version 0.1.14          # 先 bump 到 0.1.14，再发（首发常用）
#   bun run release:win -NotesFile notes.md      # release note（首发必填；正文与 latest.json 共用）
#   bun run release:win -Notes "- 修复若干问题"   # release note（内联，短说明用）
#   bun run release:win -DryRun                  # 只读预检并打印计划，不 bump/构建/上传/推送
#   bun run release:win -NoPush                  # 见下方 -NoPush 语义
#   bun run release:win -SkipPull                # 跳过 git pull
#
# LLM 友好：release note 走命令行，不与 CHANGELOG 耦合。多行说明建议先写到一个 .md 文件，
#   再用 -NotesFile 传入（PowerShell 传多行参数不稳，脚本内部也用文件中转）。首发若既没
#   -NotesFile 也没 -Notes，脚本会直接报错，不会建出空说明的 Release。
#
# -NoPush 语义：
#   APPEND：仍上传产物到已存在的 Release，但不 commit/push latest.json（改动留本地）。
#   CREATE：只本地 bump/构建/合并 latest.json，不 commit/tag/push、不建 Release（供离线预演）。
#
# 前提（一次性配好即可反复用）：
#   1) apps/desktop/signing/nomifun-updater.key —— updater 私钥（keyID F3AA272E60AA7952），
#      gitignored；必须与 tauri.conf.json 内嵌 pubkey 匹配。
#   2) apps/desktop/signing/.env.release —— 内含 GH_TOKEN=...（repo 或 Contents:rw 的 PAT），
#      gitignored；见 .env.release.example。也可先设 $env:GH_TOKEN。
#
# 说明：本轮不启用 Authenticode；自动更新验签走 Tauri updater 的 minisign 签名，脚本已覆盖。
# ============================================================================
param(
  [string]$Version,
  [string]$Notes,
  [string]$NotesFile,
  [switch]$DryRun,
  [switch]$NoPush,
  [switch]$SkipPull
)
$ErrorActionPreference = 'Stop'

try { [Console]::OutputEncoding = [System.Text.Encoding]::UTF8 } catch {}

if ($null -ne $IsWindows) { $onWindows = [bool]$IsWindows } else { $onWindows = ($env:OS -eq 'Windows_NT') }
if (-not $onWindows) { Write-Error "release:win 只能在 Windows 上运行。macOS 用 build:mac，Linux 用 build:linux。"; exit 1 }

$ScriptDir = Split-Path -Parent $MyInvocation.MyCommand.Path
$Root      = Split-Path -Parent $ScriptDir
Set-Location $Root

$Repo        = 'nomifun/nomifun-tauri'
$Triple      = 'x86_64-pc-windows-msvc'
$KeyFile     = 'apps/desktop/signing/nomifun-updater.key'
$EnvRelease  = 'apps/desktop/signing/.env.release'
$UpdaterConf = 'apps/desktop/tauri.updater.conf.json'
$LatestJson  = 'apps/desktop/updater/latest.json'

function Fail($msg) { Write-Error $msg; exit 1 }

# ── gh CLI ──────────────────────────────────────────────────────────────────
function Resolve-Gh {
  $cmd = Get-Command gh -ErrorAction SilentlyContinue
  if ($cmd) { return $cmd.Source }
  $fallback = Join-Path $env:ProgramFiles 'GitHub CLI\gh.exe'
  if (Test-Path $fallback) { return $fallback }
  return $null
}
$gh = Resolve-Gh
if (-not $gh) { Fail "未找到 gh CLI。安装：winget install --id GitHub.cli -e --source winget" }

# ── GH_TOKEN：优先环境变量，否则从 .env.release 读 ───────────────────────────
if (-not $env:GH_TOKEN) {
  if (Test-Path $EnvRelease) {
    foreach ($line in Get-Content $EnvRelease) {
      $t = $line.Trim()
      if ($t -eq '' -or $t.StartsWith('#')) { continue }
      if ($t -match '^\s*GH_TOKEN\s*=\s*(.+)$')      { $env:GH_TOKEN = $Matches[1].Trim() }
      elseif ($t -match '^(ghp_|github_pat_)')        { $env:GH_TOKEN = $t }
    }
  }
}
if (-not $env:GH_TOKEN) { Fail "缺少 GH_TOKEN。请在 $EnvRelease 里填 GH_TOKEN=...（见 .env.release.example），或先设 `$env:GH_TOKEN。" }

# ── updater 私钥 ─────────────────────────────────────────────────────────────
if (-not (Test-Path $KeyFile)) { Fail "缺少 updater 私钥 $KeyFile（从密钥库拷入，keyID F3AA272E60AA7952）。" }
$env:TAURI_SIGNING_PRIVATE_KEY = Get-Content $KeyFile -Raw
$env:TAURI_SIGNING_PRIVATE_KEY_PASSWORD = ""

# ── 同步代码 ─────────────────────────────────────────────────────────────────
if (-not $SkipPull) {
  Write-Host "▶ git pull --ff-only origin main"
  git pull --ff-only origin main
  if ($LASTEXITCODE -ne 0) { Fail "git pull 失败（可能本地有分叉或未提交改动）。处理后重试，或加 -SkipPull。" }
}

# ── 版本号：单一真源 = 根 Cargo.toml [workspace.package].version ──────────────
function Read-WorkspaceVersion {
  $inSection = $false
  foreach ($line in Get-Content 'Cargo.toml') {
    $t = $line.Trim()
    if ($t.StartsWith('[')) { $inSection = ($t -eq '[workspace.package]'); continue }
    if ($inSection -and ($line -match '^\s*version\s*=\s*"([^"]+)"')) { return $Matches[1] }
  }
  return $null
}
$CurVer = Read-WorkspaceVersion
if (-not $CurVer) { Fail "无法从 Cargo.toml 读取 [workspace.package].version。" }
if ($Version) { $TargetVersion = $Version } else { $TargetVersion = $CurVer }
$Tag = "v$TargetVersion"
$NeedBump = ($TargetVersion -ne $CurVer)

# ── token 有效性 ─────────────────────────────────────────────────────────────
$login = & $gh api user --jq '.login'
if ($LASTEXITCODE -ne 0) { Fail "GH_TOKEN 无效或无权限。请更新 $EnvRelease 里的 token。" }

# ── 模式判定：该版本的 Release 是否已存在 ────────────────────────────────────
# 注意：PS 5.1 下 $ErrorActionPreference='Stop' + 对原生命令重定向 stderr 会把 gh 的
# "release not found" 包装成终止性错误。这里临时切 Continue，只看退出码。
$ErrorActionPreference = 'Continue'
& $gh release view $Tag --repo $Repo --json tagName 1>$null 2>$null
$releaseExists = ($LASTEXITCODE -eq 0)
$ErrorActionPreference = 'Stop'
if ($releaseExists) { $Mode = 'APPEND' } else { $Mode = 'CREATE' }

# ── release note：-NotesFile 优先，其次 -Notes；CREATE 必填 ───────────────────
$NotesContent = $null
if ($NotesFile) {
  if (-not (Test-Path $NotesFile)) { Fail "找不到 -NotesFile 指定的文件: $NotesFile" }
  $NotesContent = (Get-Content $NotesFile -Raw).Trim()
} elseif ($Notes) {
  $NotesContent = $Notes.Trim()
}
if ($Mode -eq 'CREATE' -and (-not $NotesContent)) {
  Fail "首发(CREATE)需要 release note。请用 -NotesFile <md>（推荐，多行）或 -Notes ""..."" 提供；GitHub Release 正文与 latest.json notes 共用这一份。"
}

$NsisDir = "target/$Triple/release/bundle/nsis"
$Exe     = "$NsisDir/NomiFun_${TargetVersion}_x64-setup.exe"
$Sig     = "$Exe.sig"

# ── 计划 ─────────────────────────────────────────────────────────────────────
Write-Host "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
Write-Host "一键 Windows 发版计划"
Write-Host "  模式      : $Mode  （$(if ($Mode -eq 'APPEND') { 'Release 已存在，补 Windows 产物' } else { 'Release 不存在，Windows 首发' })）"
Write-Host "  账号      : $login"
Write-Host "  版本      : $TargetVersion   (tag $Tag)"
if ($NeedBump) { Write-Host "  版本变更  : $CurVer → $TargetVersion（将执行 bun run bump）" } else { Write-Host "  版本变更  : 无（沿用当前 $CurVer）" }
Write-Host "  仓库      : $Repo"
Write-Host "  目标产物  : $Exe (+ .sig)"
if ($Mode -eq 'CREATE') { Write-Host "  release note: $(if ($NotesFile) { "文件 $NotesFile" } else { '内联 -Notes' })（首发建 Release 用）" }
elseif ($NotesContent) { Write-Host "  release note: 提供了，将同时更新 Release 正文与 latest.json notes" }
else { Write-Host "  release note: 未提供，沿用既有（latest.json notes 由 CHANGELOG 当前版本小节兜底）" }
if ($NoPush) { Write-Host "  推送      : -NoPush（见脚本头部语义说明）" } else { Write-Host "  推送      : 开启 (author=nomifun -> origin main)" }
Write-Host "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"

if ($DryRun) {
  Write-Host "✅ -DryRun：前置检查全部通过（gh / token / 模式 / 私钥 / 版本 / note），未执行 bump/构建/上传/推送。"
  exit 0
}

# ── bump（版本变更时）────────────────────────────────────────────────────────
if ($NeedBump) {
  Write-Host "▶ bun run bump $TargetVersion ..."
  & bun run bump $TargetVersion
  if ($LASTEXITCODE -ne 0) { Fail "bump 失败。" }
  $CurVer = Read-WorkspaceVersion
  if ($CurVer -ne $TargetVersion) { Fail "bump 后版本仍为 $CurVer，期望 $TargetVersion。" }
}

# ── 临时 note 文件：gh 与 make:latest 共用，避免 PowerShell 多行参数问题 ──────
$NotesTmp = $null
if ($NotesContent) {
  $NotesTmp = Join-Path $env:TEMP ("nomifun-relnotes-$TargetVersion.md")
  [System.IO.File]::WriteAllText($NotesTmp, $NotesContent, [System.Text.UTF8Encoding]::new($false))
}

# ── 清理旧版本残留产物（避免 make:latest 选到旧 .sig）────────────────────────
if (Test-Path $NsisDir) {
  Get-ChildItem -Path $NsisDir -File |
    Where-Object { ($_.Extension -eq '.exe' -or $_.Extension -eq '.sig') -and ($_.Name -notlike "*$TargetVersion*") } |
    ForEach-Object { Write-Host "  清理旧产物: $($_.Name)"; Remove-Item $_.FullName -Force }
}

# ── 构建 ─────────────────────────────────────────────────────────────────────
Write-Host "▶ 构建 Windows 自动更新产物（Rust release，耗时较长）..."
& bun run build:win --config $UpdaterConf
if ($LASTEXITCODE -ne 0) { Fail "构建失败。" }
if (-not (Test-Path $Exe)) { Fail "构建后未找到产物: $Exe" }
if (-not (Test-Path $Sig)) { Fail "构建后未找到 updater 签名: $Sig" }

# ── 合并 latest.json ─────────────────────────────────────────────────────────
Write-Host "▶ 合并 latest.json ..."
if ($NotesTmp) { & bun scripts/make-latest-json.mjs --notes-file $NotesTmp } else { & bun scripts/make-latest-json.mjs }
if ($LASTEXITCODE -ne 0) { Fail "make:latest 失败。" }

$manifest = Get-Content $LatestJson -Raw | ConvertFrom-Json
if ($manifest.version -ne $TargetVersion) { Fail "latest.json version($($manifest.version)) != $TargetVersion。" }
if (-not $manifest.platforms.'windows-x86_64') { Fail "latest.json 缺少 windows-x86_64 条目，合并异常。" }

# ── 发布 ─────────────────────────────────────────────────────────────────────
function Commit-Latest($message, $extraPaths) {
  git add $LatestJson
  foreach ($p in $extraPaths) { if (Test-Path $p) { git add $p } }
  git diff --cached --quiet
  if ($LASTEXITCODE -eq 0) { Write-Host "  无待提交改动，跳过 commit。"; return }
  git -c user.name=nomifun commit -m $message
  if ($LASTEXITCODE -ne 0) { Fail "commit 失败。" }
}

# 推送 main，兼容构建期间远端被并发推进：被拒则 rebase 一次并重试；有 tag 时把 tag 移到
# rebase 后的提交。rebase 冲突则 abort 并让用户手动处理（产物已在 target/，可 -SkipPull 复用）。
function Push-Main([string]$tagToMove) {
  git push origin main
  if ($LASTEXITCODE -eq 0) { return }
  Write-Host "  push 被拒（构建期间远端有新提交），rebase 后重试一次..."
  git -c user.name=nomifun pull --rebase origin main
  if ($LASTEXITCODE -ne 0) {
    $ErrorActionPreference = 'Continue'; git rebase --abort 2>$null; $ErrorActionPreference = 'Stop'
    Fail "rebase 遇冲突已 abort。请手动 git pull --rebase 解决后重跑（产物已在 target/，可加 -SkipPull 复用）。"
  }
  if ($tagToMove) {
    git tag -f $tagToMove
    if ($LASTEXITCODE -ne 0) { Fail "重定位 tag $tagToMove 失败。" }
  }
  git push origin main
  if ($LASTEXITCODE -ne 0) { Fail "rebase 后 push 仍失败（可能再次并发），请重跑。" }
}

if ($Mode -eq 'CREATE') {
  if ($NoPush) {
    Write-Host "  -NoPush（CREATE）：已本地 bump/构建/合并 latest.json，但不提交/建 Release。"
    Write-Host "  待办：git 提交 bump+latest.json、打 tag $Tag、push、gh release create。"
  } else {
    Write-Host "▶ 提交并打 tag $Tag（author=nomifun）..."
    Commit-Latest "chore(release): $Tag" @('Cargo.toml', 'Cargo.lock', 'package.json', 'ui/package.json', 'apps/desktop/tauri.conf.json')
    $ErrorActionPreference = 'Continue'
    git rev-parse -q --verify "refs/tags/$Tag" 1>$null 2>$null
    $tagExists = ($LASTEXITCODE -eq 0)
    $ErrorActionPreference = 'Stop'
    if (-not $tagExists) {
      git tag $Tag
      if ($LASTEXITCODE -ne 0) { Fail "git tag $Tag 失败。" }
    } else {
      Write-Host "  tag $Tag 已存在，复用。"
    }
    Push-Main $Tag
    git push origin $Tag
    if ($LASTEXITCODE -ne 0) { Fail "git push tag 失败。" }

    Write-Host "▶ 创建 Release $Tag 并上传 Windows 产物 ..."
    & $gh release create $Tag --repo $Repo $Exe $Sig $LatestJson --title $Tag --notes-file $NotesTmp
    if ($LASTEXITCODE -ne 0) { Fail "gh release create 失败。" }
  }
} else {
  Write-Host "▶ 上传 Windows 资产到 Release $Tag（--clobber）..."
  & $gh release upload $Tag --repo $Repo $Exe $Sig $LatestJson --clobber
  if ($LASTEXITCODE -ne 0) { Fail "上传失败。" }
  if ($NotesTmp) {
    Write-Host "▶ 更新 Release 正文（-Notes/-NotesFile 提供了新说明）..."
    & $gh release edit $Tag --repo $Repo --notes-file $NotesTmp
    if ($LASTEXITCODE -ne 0) { Write-Warning "gh release edit 更新正文失败（不阻断）。" }
  }
  if ($NoPush) {
    Write-Host "  -NoPush（APPEND）：跳过提交/推送，latest.json 改动留在本地。"
  } else {
    Write-Host "▶ 提交 latest.json 回 main（author=nomifun）..."
    Commit-Latest "chore(release): add Windows x64 updater entry to $Tag latest.json" @()
    Push-Main $null
  }
}

# ── 发布后校验 ───────────────────────────────────────────────────────────────
if (-not ($Mode -eq 'CREATE' -and $NoPush)) {
  Write-Host "▶ 发布后校验（updater 端点）..."
  $endpoint = "https://github.com/$Repo/releases/latest/download/latest.json"
  try {
    $resp = Invoke-WebRequest -Uri $endpoint -UseBasicParsing -MaximumRedirection 10
    # release-assets CDN 用 octet-stream 返回，$resp.Content 可能是字节数组，需解码成字符串。
    $raw  = if ($resp.Content -is [byte[]]) { [System.Text.Encoding]::UTF8.GetString($resp.Content) } else { [string]$resp.Content }
    $pub  = $raw | ConvertFrom-Json
    Write-Host "  version   : $($pub.version)"
    Write-Host "  platforms : $($pub.platforms.PSObject.Properties.Name -join ', ')"
    Write-Host "  windows   : $($pub.platforms.'windows-x86_64'.url)"
    if ($pub.version -ne $TargetVersion) { Write-Warning "published version($($pub.version)) != $TargetVersion（CDN 缓存延迟或 latest 非本版本）。" }
    if (-not $pub.platforms.'windows-x86_64') { Write-Warning "published latest.json 缺少 windows-x86_64。" }
  } catch {
    Write-Warning "校验拉取失败: $($_.Exception.Message)（CDN 缓存延迟，可稍后手动核对）。"
  }
}

if ($NotesTmp) { Remove-Item $NotesTmp -Force -ErrorAction SilentlyContinue }

Write-Host "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
Write-Host "✅ Windows 发版完成（$Mode）：$Tag"
Write-Host "   Release: https://github.com/$Repo/releases/tag/$Tag"
Write-Host "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
