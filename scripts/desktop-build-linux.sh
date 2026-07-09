#!/usr/bin/env bash
# ============================================================================
# 打 Linux 桌面端安装包(.deb / .AppImage / .rpm),汇总到 dist/desktop/。
# 仅能在 Linux 上运行。
#
#   bun run build:linux               # 默认打当前机器架构(x64 或 arm64)
#   bun run build:linux x64           # 显式 x86_64
#   bun run build:linux arm64         # 显式 aarch64(见下方交叉编译警告)
#   bun run build:linux x64 arm64     # 两个都打
#   bun run build:linux --config apps/desktop/tauri.updater.conf.json
#                                     # 未知 --xxx 选项会原样透传给 tauri build
#   bun run build:linux -- --bundles deb
#                                     # `--` 之后的参数也会原样透传给 tauri build
#
# 架构别名:
#   x64   / x86_64        -> x86_64-unknown-linux-gnu
#   arm64 / aarch64 / arm -> aarch64-unknown-linux-gnu
#
# Linux 没有 macOS 那种「签名 + 公证」体系,故本脚本不涉及签名。
#
# ⚠️ 交叉编译警告:Tauri 的 Linux 包链接 webkit2gtk 等系统库,跨架构构建
#    (在 x64 上打 arm64,或反之)需要目标架构的 sysroot/交叉工具链,仅
#    `rustup target add` 不够。最稳妥是在「目标架构的机器或容器」上原生构建。
#    本脚本只对当前机器架构自动装 rust target;其它架构仅尝试,失败请改用原生环境。
#
# 注:macOS 包用 build:mac,Windows 包用 build:win,且都需在对应系统上构建。
# ============================================================================
set -euo pipefail

if [[ "$(uname -s)" != "Linux" ]]; then
  echo "❌ build:linux 只能在 Linux 上运行(当前: $(uname -s))。" >&2
  echo "   macOS 包用 build:mac,Windows 包用 build:win,且都需在对应系统上构建。" >&2
  exit 1
fi

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
CONF="apps/desktop/tauri.conf.json"
DIST="$ROOT/dist/desktop"

require_linux_build_deps() {
  local missing=()

  if ! command -v pkg-config >/dev/null 2>&1; then
    missing+=("pkg-config")
  else
    pkg-config --exists gbm || missing+=("libgbm-dev (pkg-config: gbm)")
    pkg-config --exists librsvg-2.0 || missing+=("librsvg2-dev (pkg-config: librsvg-2.0)")
    if ! pkg-config --exists ayatana-appindicator3-0.1 && ! pkg-config --exists appindicator3-0.1; then
      missing+=("libayatana-appindicator3-dev 或 libappindicator3-dev (pkg-config: *appindicator3-0.1)")
    fi
  fi

  if [[ "${#missing[@]}" -gt 0 ]]; then
    echo "❌ Linux 打包依赖不完整:" >&2
    local item
    for item in "${missing[@]}"; do
      echo "   - $item" >&2
    done
    cat >&2 <<'EOF'

Debian/Ubuntu 可先安装:
  sudo apt-get install -y pkg-config libgbm-dev libayatana-appindicator3-dev librsvg2-dev

说明:
  - libgbm-dev 提供 -lgbm 链接名与 gbm.pc。
  - libayatana-appindicator3-dev 提供 Tauri 托盘/AppIndicator 打包探测。
  - librsvg2-dev 提供 linuxdeploy GTK 插件需要的 librsvg-2.0.pc。
  - 本脚本会设置 APPIMAGE_EXTRACT_AND_RUN=1，让 linuxdeploy AppImage 在无 FUSE2 的构建机上也能运行。
EOF
    exit 1
  fi
}

# 当前机器架构对应的 triple(用于判断哪个是「原生」)
HOST_ARCH="$(uname -m)"
case "$HOST_ARCH" in
  x86_64)         HOST_TRIPLE="x86_64-unknown-linux-gnu" ;;
  aarch64|arm64)  HOST_TRIPLE="aarch64-unknown-linux-gnu" ;;
  *)              HOST_TRIPLE="" ;;
esac

# ── 解析参数:`--` 之前是架构选择,之后原样透传给 tauri build ──────────────────
SELECT=()
PASSTHRU=()
seen_dashdash=0
for arg in "$@"; do
  if [[ "$seen_dashdash" -eq 1 ]]; then
    PASSTHRU+=("$arg")
  elif [[ "$arg" == "--" ]]; then
    seen_dashdash=1
  elif [[ "$arg" == --* ]]; then
    PASSTHRU+=("$arg")
    seen_dashdash=1
  else
    SELECT+=("$arg")
  fi
done

resolve_triple() {
  case "$1" in
    x64|x86_64|x86_64-unknown-linux-gnu)              echo "x86_64-unknown-linux-gnu" ;;
    arm64|aarch64|arm|aarch64-unknown-linux-gnu)      echo "aarch64-unknown-linux-gnu" ;;
    *) echo "❌ 未知架构: $1 (可选: x64 / arm64)" >&2; exit 1 ;;
  esac
}

TRIPLES=()
if [[ "${#SELECT[@]}" -eq 0 ]]; then
  if [[ -z "$HOST_TRIPLE" ]]; then
    echo "❌ 无法识别当前架构: $HOST_ARCH,请显式指定 x64 或 arm64。" >&2
    exit 1
  fi
  TRIPLES=("$HOST_TRIPLE")   # 默认只打当前机器架构
else
  for s in "${SELECT[@]}"; do
    TRIPLES+=("$(resolve_triple "$s")")
  done
fi

require_linux_build_deps
export APPIMAGE_EXTRACT_AND_RUN="${APPIMAGE_EXTRACT_AND_RUN:-1}"

ensure_target() {
  local t="$1"
  if ! rustup target list --installed | grep -qx "$t"; then
    echo "▶ 安装 Rust target: $t"
    rustup target add "$t"
  fi
}

mkdir -p "$DIST"

echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
echo "将依次构建以下目标: ${TRIPLES[*]}"
echo "产物汇总目录: $DIST"
echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"

COLLECTED=()
for t in "${TRIPLES[@]}"; do
  ensure_target "$t"
  if [[ "$t" != "$HOST_TRIPLE" ]]; then
    echo "⚠️  $t 与当前机器架构($HOST_TRIPLE)不同,正在尝试交叉编译——"
    echo "    若链接 webkit2gtk 等系统库失败,请改到目标架构的原生环境/容器构建。"
  fi
  echo ""
  echo "▶▶▶ 构建 $t ..."
  CI=true bun x tauri build --config "$CONF" --target "$t" ${PASSTHRU[@]+"${PASSTHRU[@]}"}

  # Linux 产物在 target/<triple>/release/bundle/{deb,appimage,rpm}/
  bundle_dir="$ROOT/target/$t/release/bundle"
  while IFS= read -r -d '' pkg; do
    cp -f "$pkg" "$DIST/"
    COLLECTED+=("$DIST/$(basename "$pkg")")
  done < <(find "$bundle_dir" -type f \( -name '*.deb' -o -name '*.AppImage' -o -name '*.rpm' \) -print0 2>/dev/null)
done

echo ""
echo "▶ 清理 Linux 构建后 debug/flycheck 中间产物(保留 release 安装包与 updater 签名)..."
bun scripts/prune-build.mjs --post

echo ""
echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
echo "✅ 全部完成,安装包已汇总到 $DIST :"
for f in "${COLLECTED[@]}"; do
  size="$(du -h "$f" | cut -f1)"
  printf "   %-44s %s\n" "$(basename "$f")" "$size"
done
echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
