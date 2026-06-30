# ============================================================================
# 打 Windows 桌面端安装包(.msi + .exe/NSIS),汇总到 dist/desktop/。
# 仅能在 Windows 上运行。
#
#   bun run build:win                  # 默认打当前机器架构(通常 x64)
#   bun run build:win x64              # 显式 x86_64
#   bun run build:win arm64           # 显式 aarch64
#   bun run build:win x64 arm64       # 两个都打
#   bun run build:win --signed        # 带 Authenticode 签名(见下)
#   bun run build:win -- --bundles nsis
#                                     # `--` 之后的参数原样透传给 tauri build
#
# 架构别名:
#   x64   / x86_64        -> x86_64-pc-windows-msvc
#   arm64 / aarch64 / arm -> aarch64-pc-windows-msvc
#
# 签名(--signed)说明:
#   Windows 用 Authenticode(无 macOS 那种「公证」)。本脚本从环境变量
#   WINDOWS_CERTIFICATE_THUMBPRINT 读取证书指纹(证书须已装进当前用户的证书库),
#   并通过 --config 注入 tauri 的 bundle.windows.certificateThumbprint。
#   也可改用 tauri.conf.json 里的 signCommand 走自定义签名(如 azuresigntool)。
#   未设置该环境变量时,--signed 直接报错提示。
#
# 注:macOS 包用 build:mac,Linux 包用 build:linux,且都需在对应系统上构建。
# ============================================================================
$ErrorActionPreference = 'Stop'

if (-not $IsWindows) {
  Write-Error "build:win 只能在 Windows 上运行。macOS 包用 build:mac,Linux 包用 build:linux,且都需在对应系统上构建。"
  exit 1
}

$ScriptDir = Split-Path -Parent $MyInvocation.MyCommand.Path
$Root      = Split-Path -Parent $ScriptDir
$Conf      = 'apps/desktop/tauri.conf.json'
$Dist      = Join-Path $Root 'dist/desktop'

# 当前机器架构 -> 原生 triple
$hostArch = $env:PROCESSOR_ARCHITECTURE
switch ($hostArch) {
  'AMD64' { $hostTriple = 'x86_64-pc-windows-msvc' }
  'ARM64' { $hostTriple = 'aarch64-pc-windows-msvc' }
  default { $hostTriple = '' }
}

# ── 解析参数:`--` 之前是架构选择/开关,之后透传给 tauri build ──────────────────
$select   = @()
$passthru = @()
$signed   = $false
$seenDD   = $false
foreach ($arg in $args) {
  if ($seenDD)              { $passthru += $arg }
  elseif ($arg -eq '--')   { $seenDD = $true }
  elseif ($arg -eq '--signed') { $signed = $true }
  else                     { $select += $arg }
}

function Resolve-Triple($a) {
  switch ($a) {
    { $_ -in 'x64','x86_64','x86_64-pc-windows-msvc' }            { return 'x86_64-pc-windows-msvc' }
    { $_ -in 'arm64','aarch64','arm','aarch64-pc-windows-msvc' }  { return 'aarch64-pc-windows-msvc' }
    default { Write-Error "未知架构: $a (可选: x64 / arm64)"; exit 1 }
  }
}

$triples = @()
if ($select.Count -eq 0) {
  if (-not $hostTriple) { Write-Error "无法识别当前架构: $hostArch,请显式指定 x64 或 arm64。"; exit 1 }
  $triples = @($hostTriple)          # 默认只打当前机器架构
} else {
  foreach ($s in $select) { $triples += (Resolve-Triple $s) }
}

# ── 签名:校验证书指纹环境变量,注入 tauri config ─────────────────────────────
$signConfig = @()
if ($signed) {
  $thumb = $env:WINDOWS_CERTIFICATE_THUMBPRINT
  if (-not $thumb) {
    Write-Error @"
❌ --signed 需要环境变量 WINDOWS_CERTIFICATE_THUMBPRINT(证书指纹)。

请先把代码签名证书导入当前用户证书库,然后设置(示例):
  `$env:WINDOWS_CERTIFICATE_THUMBPRINT = 'A1B2C3...'
或在 tauri.conf.json 的 bundle.windows 里配置 certificateThumbprint / signCommand。
"@
    exit 1
  }
  $cfgJson = '{"bundle":{"windows":{"certificateThumbprint":"' + $thumb + '"}}}'
  $signConfig = @('--config', $cfgJson)
  Write-Host "▶ 签名: 已启用 (指纹 $($thumb.Substring(0,[Math]::Min(8,$thumb.Length)))...)"
}

function Ensure-Target($t) {
  $installed = (rustup target list --installed)
  if ($installed -notcontains $t) {
    Write-Host "▶ 安装 Rust target: $t"
    rustup target add $t
  }
}

New-Item -ItemType Directory -Force -Path $Dist | Out-Null

Write-Host "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
Write-Host "将依次构建以下目标: $($triples -join ' ')"
if ($signed) { Write-Host "签名: 开启" } else { Write-Host "签名: 关闭 (本地测试包)" }
Write-Host "产物汇总目录: $Dist"
Write-Host "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"

$collected = @()
foreach ($t in $triples) {
  Ensure-Target $t
  if ($t -ne $hostTriple) {
    Write-Host "⚠️  $t 与当前机器架构($hostTriple)不同,正在尝试交叉编译。"
  }
  Write-Host ""
  Write-Host "▶▶▶ 构建 $t ..."
  $env:CI = 'true'
  & bun x tauri build --config $Conf --target $t @signConfig @passthru
  if ($LASTEXITCODE -ne 0) { Write-Error "构建 $t 失败"; exit $LASTEXITCODE }

  # Windows 产物在 target\<triple>\release\bundle\{msi,nsis}\
  $bundleDir = Join-Path $Root "target/$t/release/bundle"
  if (Test-Path $bundleDir) {
    Get-ChildItem -Path $bundleDir -Recurse -Include '*.msi','*.exe' -File | ForEach-Object {
      Copy-Item $_.FullName -Destination $Dist -Force
      $collected += (Join-Path $Dist $_.Name)
    }
  }
}

Write-Host ""
Write-Host "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
Write-Host "✅ 全部完成,安装包已汇总到 $Dist :"
foreach ($f in $collected) {
  $sizeMB = [Math]::Round((Get-Item $f).Length / 1MB, 1)
  Write-Host ("   {0,-44} {1} MB" -f (Split-Path $f -Leaf), $sizeMB)
}
Write-Host "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
