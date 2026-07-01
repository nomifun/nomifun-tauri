#!/usr/bin/env bash
# ============================================================================
# 一键 macOS 发版。自动判定两种场景：
#
#   追加(APPEND)：该版本的 GitHub Release 已存在（通常 Windows 侧先发过）——只补
#                 macOS 产物并把 darwin 条目合并进 latest.json。
#   首发(CREATE)：该版本还没有 Release（macOS 先发）——建 tag、建 Release（带
#                 release note）、上传 macOS 产物。可用 -Version 顺带打版本号。
#
#   bun run release:mac                          # 用当前 Cargo.toml 版本；自动判定模式
#   bun run release:mac -Version 0.1.14          # 先 bump 到 0.1.14，再发（首发常用）
#   bun run release:mac -NotesFile notes.md      # release note（首发必填；正文与 latest.json 共用）
#   bun run release:mac -Notes "- 修复若干问题"   # release note（内联，短说明用）
#   bun run release:mac -DryRun                  # 只读预检并打印计划，不 pull/bump/构建/上传/推送
#   bun run release:mac -NoPush                  # 见下方 -NoPush 语义
#   bun run release:mac -SkipPull                # 跳过 git pull（真实执行时）
#
# -NoPush 语义：
#   APPEND：仍上传产物到已存在的 Release，但不 commit/push latest.json（改动留本地）。
#   CREATE：只本地 bump/构建/合并 latest.json，不 commit/tag/push、不建 Release（供离线预演）。
#
# 前提（一次性配好即可反复用）：
#   1) apps/desktop/signing/nomifun-updater.key —— updater 私钥（keyID F3AA272E60AA7952），
#      gitignored；必须与 tauri.conf.json 内嵌 pubkey 匹配。
#   2) apps/desktop/signing/.env.signing —— Apple Developer ID / notarization 配置。
#   3) apps/desktop/signing/.env.release —— 可选，内含 GH_TOKEN=...；也可使用 gh auth login。
# ============================================================================
set -euo pipefail

if [[ "${NOMIFUN_RELEASE_TEST_UNAME:-$(uname -s)}" != "Darwin" ]]; then
  echo "release:mac 只能在 macOS 上运行。Windows 包用 release:win，Linux 包用 build:linux。" >&2
  exit 1
fi

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
cd "$ROOT"

Repo="nomifun/nomifun-tauri"
Triple="universal-apple-darwin"
KeyFile="${NOMIFUN_RELEASE_KEY_FILE:-apps/desktop/signing/nomifun-updater.key}"
EnvRelease="${NOMIFUN_RELEASE_ENV_FILE:-apps/desktop/signing/.env.release}"
SigningEnv="${NOMIFUN_RELEASE_SIGNING_ENV:-apps/desktop/signing/.env.signing}"
UpdaterConf="apps/desktop/tauri.updater.conf.json"
LatestJson="apps/desktop/updater/latest.json"

Version=""
Notes=""
NotesFile=""
DryRun=0
NoPush=0
SkipPull=0
NotesTmp=""

cleanup() {
  if [[ -n "$NotesTmp" && -f "$NotesTmp" ]]; then
    rm -f "$NotesTmp"
  fi
}
trap cleanup EXIT

fail() {
  echo "$1" >&2
  exit 1
}

while [[ "$#" -gt 0 ]]; do
  case "$1" in
    --)
      shift
      ;;
    -Version|--version|-v)
      shift
      [[ "$#" -gt 0 ]] || fail "缺少 -Version 参数值。"
      Version="$1"
      shift
      ;;
    -Notes|--notes)
      shift
      [[ "$#" -gt 0 ]] || fail "缺少 -Notes 参数值。"
      Notes="$1"
      shift
      ;;
    -NotesFile|--notes-file)
      shift
      [[ "$#" -gt 0 ]] || fail "缺少 -NotesFile 参数值。"
      NotesFile="$1"
      shift
      ;;
    -DryRun|--dry-run)
      DryRun=1
      shift
      ;;
    -NoPush|--no-push)
      NoPush=1
      shift
      ;;
    -SkipPull|--skip-pull)
      SkipPull=1
      shift
      ;;
    *)
      fail "未知参数: $1"
      ;;
  esac
done

read_workspace_version() {
  local in_section=0 line trimmed
  while IFS= read -r line; do
    trimmed="${line#"${line%%[![:space:]]*}"}"
    trimmed="${trimmed%"${trimmed##*[![:space:]]}"}"
    if [[ "$trimmed" == \[* ]]; then
      [[ "$trimmed" == "[workspace.package]" ]] && in_section=1 || in_section=0
      continue
    fi
    if [[ "$in_section" -eq 1 && "$line" =~ ^[[:space:]]*version[[:space:]]*=[[:space:]]*\"([^\"]+)\" ]]; then
      printf "%s\n" "${BASH_REMATCH[1]}"
      return 0
    fi
  done < Cargo.toml
  return 1
}

load_release_env() {
  [[ -n "${GH_TOKEN:-}" || ! -f "$EnvRelease" ]] && return 0
  local line t
  while IFS= read -r line; do
    t="${line#"${line%%[![:space:]]*}"}"
    t="${t%"${t##*[![:space:]]}"}"
    [[ -z "$t" || "$t" == \#* ]] && continue
    if [[ "$t" =~ ^GH_TOKEN[[:space:]]*=[[:space:]]*(.+)$ ]]; then
      export GH_TOKEN="${BASH_REMATCH[1]}"
      GH_TOKEN="${GH_TOKEN%\"}"
      GH_TOKEN="${GH_TOKEN#\"}"
      export GH_TOKEN
    elif [[ "$t" =~ ^(ghp_|github_pat_) ]]; then
      export GH_TOKEN="$t"
    fi
  done < "$EnvRelease"
}

commit_paths() {
  local message="$1"
  shift
  local p
  for p in "$@"; do
    [[ -e "$p" ]] && git add "$p"
  done
  if git diff --cached --quiet; then
    echo "  无待提交改动，跳过 commit。"
    return 0
  fi
  git -c user.name=nomifun -c user.email=nomifun@users.noreply.github.com commit -m "$message"
}

validate_manifest() {
  node - "$LatestJson" "$TargetVersion" <<'NODE'
const fs = require('node:fs');
const [path, version] = process.argv.slice(2);
const manifest = JSON.parse(fs.readFileSync(path, 'utf8'));
if (manifest.version !== version) {
  console.error(`latest.json version(${manifest.version}) != ${version}`);
  process.exit(1);
}
for (const key of ['darwin-x86_64', 'darwin-aarch64']) {
  const entry = manifest.platforms?.[key];
  if (!entry) {
    console.error(`latest.json 缺少 ${key} 条目。`);
    process.exit(1);
  }
  if (!String(entry.url || '').includes(`/releases/download/v${version}/`)) {
    console.error(`${key} url 未指向 v${version}: ${entry.url}`);
    process.exit(1);
  }
}
NODE
}

load_release_env

gh_bin="$(command -v gh || true)"
[[ -n "$gh_bin" ]] || fail "未找到 gh CLI。安装：https://cli.github.com/"

[[ -f "$KeyFile" ]] || fail "缺少 updater 私钥 $KeyFile（从密钥库拷入，keyID F3AA272E60AA7952）。"
[[ -f "$SigningEnv" ]] || fail "缺少 macOS 签名配置 $SigningEnv（复制 .env.signing.example 后填入 Developer ID / notarization 信息）。"
[[ -f "$UpdaterConf" ]] || fail "缺少 updater overlay 配置 $UpdaterConf。"

if [[ "$DryRun" -eq 0 && "$SkipPull" -eq 0 ]]; then
  echo "▶ git pull --ff-only origin main"
  git pull --ff-only origin main || fail "git pull 失败（可能本地有分叉或未提交改动）。处理后重试，或加 -SkipPull。"
elif [[ "$DryRun" -eq 1 ]]; then
  echo "▶ -DryRun：跳过 git pull，保持只读预检。"
fi

CurVer="$(read_workspace_version || true)"
[[ -n "$CurVer" ]] || fail "无法从 Cargo.toml 读取 [workspace.package].version。"
if [[ -n "$Version" ]]; then
  TargetVersion="$Version"
else
  TargetVersion="$CurVer"
fi
Tag="v$TargetVersion"
NeedBump=0
[[ "$TargetVersion" != "$CurVer" ]] && NeedBump=1

login="$("$gh_bin" api user --jq '.login' 2>/dev/null || true)"
if [[ -z "$login" ]]; then
  fail "gh 未登录或 GH_TOKEN 无效。请运行 gh auth login，或在 $EnvRelease 里填 GH_TOKEN=...。"
fi

if "$gh_bin" release view "$Tag" --repo "$Repo" --json tagName >/dev/null 2>&1; then
  Mode="APPEND"
  ModeDesc="Release 已存在，补 macOS 产物"
else
  Mode="CREATE"
  ModeDesc="Release 不存在，macOS 首发"
fi

NotesContent=""
if [[ -n "$NotesFile" ]]; then
  [[ -f "$NotesFile" ]] || fail "找不到 -NotesFile 指定的文件: $NotesFile"
  NotesContent="$(cat "$NotesFile")"
elif [[ -n "$Notes" ]]; then
  NotesContent="$Notes"
fi

if [[ "$Mode" == "CREATE" && -z "${NotesContent//[[:space:]]/}" ]]; then
  fail "首发(CREATE)需要 release note。请用 -NotesFile <md>（推荐，多行）或 -Notes \"...\" 提供；GitHub Release 正文与 latest.json notes 共用这一份。"
fi

Tar="target/$Triple/release/bundle/macos/NomiFun.app.tar.gz"
Sig="$Tar.sig"
Dmg="dist/desktop/NomiFun_${TargetVersion}_universal.dmg"
App="target/$Triple/release/bundle/macos/NomiFun.app"

echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
echo "一键 macOS 发版计划"
echo "  模式      : ${Mode}  （${ModeDesc}）"
echo "  账号      : ${login}"
echo "  版本      : ${TargetVersion}   (tag ${Tag})"
if [[ "$NeedBump" -eq 1 ]]; then
  echo "  版本变更  : ${CurVer} → ${TargetVersion}（将执行 bun run bump）"
else
  echo "  版本变更  : 无（沿用当前 ${CurVer}）"
fi
echo "  仓库      : ${Repo}"
echo "  目标产物  : ${Dmg} + ${Tar} (+ .sig)"
if [[ "$Mode" == "CREATE" ]]; then
  if [[ -n "$NotesFile" ]]; then
    echo "  release note: 文件 ${NotesFile}（首发建 Release 用）"
  else
    echo "  release note: 内联 -Notes（首发建 Release 用）"
  fi
elif [[ -n "$NotesContent" ]]; then
  echo "  release note: 提供了，将同时更新 Release 正文与 latest.json notes"
else
  echo "  release note: 未提供，latest.json notes 由 CHANGELOG 当前版本小节兜底"
fi
if [[ "$NoPush" -eq 1 ]]; then
  echo "  推送      : -NoPush（见脚本头部语义说明）"
else
  echo "  推送      : 开启 (author=nomifun -> origin main)"
fi
echo "  签名      : Developer ID 签名 + Apple notarization（build:mac --signed）"
echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"

if [[ "$DryRun" -eq 1 ]]; then
  echo "✅ -DryRun：前置检查全部通过（gh / token / 模式 / 私钥 / 签名配置 / 版本 / note），未执行 pull/bump/构建/上传/推送。"
  exit 0
fi

export TAURI_SIGNING_PRIVATE_KEY="$(cat "$KeyFile")"
export TAURI_SIGNING_PRIVATE_KEY_PASSWORD=""

if [[ "$NeedBump" -eq 1 ]]; then
  echo "▶ bun run bump ${TargetVersion} ..."
  bun run bump "$TargetVersion" || fail "bump 失败。"
  CurVer="$(read_workspace_version || true)"
  [[ "$CurVer" == "$TargetVersion" ]] || fail "bump 后版本仍为 ${CurVer}，期望 ${TargetVersion}。"
fi

if [[ -n "${NotesContent//[[:space:]]/}" ]]; then
  NotesTmp="$(mktemp "${TMPDIR:-/tmp}/nomifun-relnotes-${TargetVersion}.XXXXXX.md")"
  printf "%s\n" "$NotesContent" > "$NotesTmp"
fi

echo "▶ 构建 macOS 签名/公证产物（Rust release，耗时较长）..."
bun run build:mac --signed --config "$UpdaterConf" || fail "构建失败。"
[[ -f "$Dmg" ]] || fail "构建后未找到手动安装包: ${Dmg}"
[[ -f "$Tar" ]] || fail "构建后未找到 updater 包: ${Tar}"
[[ -f "$Sig" ]] || fail "构建后未找到 updater 签名: ${Sig}"

echo "▶ 校验 macOS 签名与公证状态 ..."
xcrun stapler validate "$Dmg" || fail "DMG staple 校验失败。"
codesign --verify --deep --strict --verbose=2 "$App" || fail ".app codesign 校验失败。"
spctl -a -vv -t install "$Dmg" || fail "spctl Gatekeeper 校验失败。"

echo "▶ 合并 latest.json ..."
if [[ -n "$NotesTmp" ]]; then
  bun scripts/make-latest-json.mjs --notes-file "$NotesTmp" || fail "make:latest 失败。"
else
  bun scripts/make-latest-json.mjs || fail "make:latest 失败。"
fi
validate_manifest

if [[ "$Mode" == "CREATE" ]]; then
  if [[ "$NoPush" -eq 1 ]]; then
    echo "  -NoPush（CREATE）：已本地 bump/构建/合并 latest.json，但不提交/建 Release。"
    echo "  待办：git 提交 bump+latest.json、打 tag ${Tag}、push、gh release create。"
  else
    echo "▶ 提交并打 tag ${Tag}（author=nomifun）..."
    commit_paths "chore(release): $Tag" \
      Cargo.toml Cargo.lock package.json ui/package.json apps/desktop/tauri.conf.json "$LatestJson"
    if git rev-parse -q --verify "refs/tags/$Tag" >/dev/null 2>&1; then
      echo "  tag ${Tag} 已存在，复用。"
    else
      git tag "$Tag" || fail "git tag ${Tag} 失败。"
    fi
    git push origin main || fail "git push main 失败。"
    git push origin "$Tag" || fail "git push tag 失败。"

    echo "▶ 创建 Release ${Tag} 并上传 macOS 产物 ..."
    "$gh_bin" release create "$Tag" --repo "$Repo" "$Tar" "$Sig" "$Dmg" "$LatestJson" --title "$Tag" --notes-file "$NotesTmp" || fail "gh release create 失败。"
  fi
else
  echo "▶ 上传 macOS 资产到 Release ${Tag}（--clobber）..."
  "$gh_bin" release upload "$Tag" --repo "$Repo" "$Tar" "$Sig" "$Dmg" "$LatestJson" --clobber || fail "上传失败。"
  if [[ -n "$NotesTmp" ]]; then
    echo "▶ 更新 Release 正文（-Notes/-NotesFile 提供了新说明）..."
    "$gh_bin" release edit "$Tag" --repo "$Repo" --notes-file "$NotesTmp" || echo "⚠️  gh release edit 更新正文失败（不阻断）。" >&2
  fi
  if [[ "$NoPush" -eq 1 ]]; then
    echo "  -NoPush（APPEND）：跳过提交/推送，latest.json 改动留在本地。"
  else
    echo "▶ 提交 latest.json 回 main（author=nomifun）..."
    commit_paths "chore(release): add macOS updater entry to ${Tag} latest.json" "$LatestJson"
    git push origin main || fail "push 失败。"
  fi
fi

if [[ ! ( "$Mode" == "CREATE" && "$NoPush" -eq 1 ) ]]; then
  echo "▶ 发布后校验（updater 端点）..."
  endpoint="https://github.com/${Repo}/releases/latest/download/latest.json"
  node - "$endpoint" "$TargetVersion" <<'NODE'
const [endpoint, version] = process.argv.slice(2);
fetch(endpoint, { redirect: 'follow' })
  .then((res) => {
    if (!res.ok) throw new Error(`${res.status} ${res.statusText}`);
    return res.json();
  })
  .then((manifest) => {
    const platforms = Object.keys(manifest.platforms || {});
    console.log(`  version   : ${manifest.version}`);
    console.log(`  platforms : ${platforms.join(', ')}`);
    console.log(`  macOS     : ${manifest.platforms?.['darwin-aarch64']?.url || '(missing)'}`);
    if (manifest.version !== version) {
      console.warn(`published version(${manifest.version}) != ${version}（CDN 缓存延迟或 latest 非本版本）。`);
    }
    for (const key of ['darwin-x86_64', 'darwin-aarch64']) {
      if (!manifest.platforms?.[key]) console.warn(`published latest.json 缺少 ${key}。`);
    }
  })
  .catch((err) => {
    console.warn(`校验拉取失败: ${err.message}（CDN 缓存延迟，可稍后手动核对）。`);
  });
NODE
fi

echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
echo "✅ macOS 发版完成（${Mode}）：${Tag}"
echo "   Release: https://github.com/${Repo}/releases/tag/${Tag}"
echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
