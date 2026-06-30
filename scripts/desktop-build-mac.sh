#!/usr/bin/env bash
# ============================================================================
# 打 macOS 桌面端安装包(.dmg),并汇总到 dist/desktop/。仅能在 macOS 上运行。
#
#   bun run build:mac                 # 默认只打 Universal 一个 DMG(不签名)
#   bun run build:mac --signed        # 默认 Universal,带 Developer ID 签名 + 公证
#   bun run build:mac arm intel       # 显式指定架构(可多选,空格分隔)
#   bun run build:mac --signed intel  # 只打 Intel,且签名+公证
#   bun run build:mac --config '{"bundle":{"createUpdaterArtifacts":true}}'
#                                     # 未知 --xxx 选项会原样透传给 tauri build
#   bun run build:mac arm --config '{"bundle":{"createUpdaterArtifacts":true}}'
#                                     # 架构参数仍放在 tauri build 参数之前
#
# 架构别名:
#   arm / aarch64 / silicon  -> aarch64-apple-darwin   (Apple Silicon 原生)
#   intel / x64 / x86_64     -> x86_64-apple-darwin     (Intel 原生 / M 系 Rosetta)
#   universal / all-arch     -> universal-apple-darwin  (二合一胖包,通吃两种 Mac)
#
# 缺失的 Rust 编译目标会自动 `rustup target add`。
#
# 签名(--signed)说明:
#   密钥/口令全部来自本地 apps/desktop/signing/.env.signing(已 gitignore,绝不入库),
#   与 build:signed 用同一份配置。签名在 tauri build 阶段由环境变量自动完成,公证在
#   每个 target 构建结束后对其 DMG 逐个提交 Apple 并 staple。文件不存在时直接报错。
#
# 注:Windows / Linux 包无法在 macOS 上交叉构建,请到对应系统上分别用
#     bun run build:win / build:linux。
# ============================================================================
set -euo pipefail

if [[ "$(uname -s)" != "Darwin" ]]; then
  echo "❌ build:mac 只能在 macOS 上运行(当前: $(uname -s))。" >&2
  echo "   Windows 包用 build:win,Linux 包用 build:linux,且都需在对应系统上构建。" >&2
  exit 1
fi

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
CONF="apps/desktop/tauri.conf.json"
DIST="$ROOT/dist/desktop"
ENV_FILE="$ROOT/apps/desktop/signing/.env.signing"

# ── 解析参数:架构选择/开关归本脚本,未知 --xxx 起原样透传给 tauri build ─────
SELECT=()
PASSTHRU=()
SIGNED=0
seen_dashdash=0
for arg in "$@"; do
  if [[ "$seen_dashdash" -eq 1 ]]; then
    PASSTHRU+=("$arg")
  elif [[ "$arg" == "--" ]]; then
    seen_dashdash=1
  elif [[ "$arg" == "--signed" ]]; then
    SIGNED=1
  elif [[ "$arg" == --* ]]; then
    PASSTHRU+=("$arg")
    seen_dashdash=1
  else
    SELECT+=("$arg")
  fi
done

# 把别名规整成 rustc target triple
resolve_triple() {
  case "$1" in
    arm|aarch64|silicon|aarch64-apple-darwin)        echo "aarch64-apple-darwin" ;;
    intel|x64|x86_64|x86_64-apple-darwin)            echo "x86_64-apple-darwin" ;;
    universal|all-arch|universal-apple-darwin)       echo "universal-apple-darwin" ;;
    *) echo "❌ 未知架构: $1 (可选: arm / intel / universal)" >&2; exit 1 ;;
  esac
}

TRIPLES=()
if [[ "${#SELECT[@]}" -eq 0 ]]; then
  # 默认只打 Universal 一个胖包:原生通吃 Intel + Apple Silicon,体验与单架构包无异,
  # 只多占下载/磁盘体积,却省掉多轮编译与 Apple 公证等待。需要单架构包时显式指定。
  TRIPLES=(universal-apple-darwin)
else
  for s in "${SELECT[@]}"; do
    TRIPLES+=("$(resolve_triple "$s")")
  done
fi

# ── 确保所需 Rust target 已安装(universal 需要底层两个 target 都在) ──────────
ensure_target() {
  local t="$1"
  if ! rustup target list --installed | grep -qx "$t"; then
    echo "▶ 安装 Rust target: $t"
    rustup target add "$t"
  fi
}
for t in "${TRIPLES[@]}"; do
  if [[ "$t" == "universal-apple-darwin" ]]; then
    ensure_target aarch64-apple-darwin
    ensure_target x86_64-apple-darwin
  else
    ensure_target "$t"
  fi
done

# ── 签名:加载本地密钥并做基本校验(逻辑与 build:signed 对齐) ───────────────────
HAS_NOTARY=0
if [[ "$SIGNED" -eq 1 ]]; then
  if [[ ! -f "$ENV_FILE" ]]; then
    cat >&2 <<EOF
❌ 找不到本地签名配置: $ENV_FILE

请先创建它(不会入库):
  cp apps/desktop/signing/.env.signing.example apps/desktop/signing/.env.signing
然后按文件内注释 / apps/desktop/signing/README.md 填入你的签名 + 公证信息。
EOF
    exit 1
  fi

  # set -a 让 source 进来的变量自动 export 给子进程(tauri build 据此自动签名)
  set -a
  # shellcheck disable=SC1090
  source "$ENV_FILE"
  set +a

  # notarytool 要求 .p8 用绝对路径;相对仓库根的路径补成绝对路径
  if [[ -n "${APPLE_API_KEY_PATH:-}" && "${APPLE_API_KEY_PATH:0:1}" != "/" ]]; then
    export APPLE_API_KEY_PATH="$ROOT/$APPLE_API_KEY_PATH"
  fi

  if [[ -z "${APPLE_SIGNING_IDENTITY:-}" && -z "${APPLE_CERTIFICATE:-}" ]]; then
    echo "❌ 既没设 APPLE_SIGNING_IDENTITY,也没设 APPLE_CERTIFICATE,无法签名。" >&2
    exit 1
  fi

  if [[ -n "${APPLE_API_KEY:-}" && -n "${APPLE_API_ISSUER:-}" && -n "${APPLE_API_KEY_PATH:-}" ]]; then
    HAS_NOTARY=1
    if [[ ! -f "$APPLE_API_KEY_PATH" ]]; then
      echo "❌ 找不到 App Store Connect API Key: $APPLE_API_KEY_PATH" >&2
      exit 1
    fi
    if [[ "$APPLE_API_KEY_PATH" != *.p8 ]]; then
      echo "❌ APPLE_API_KEY_PATH 必须指向 AuthKey_*.p8,当前是: $APPLE_API_KEY_PATH" >&2
      exit 1
    fi
  elif [[ -n "${APPLE_ID:-}" && -n "${APPLE_PASSWORD:-}" && -n "${APPLE_TEAM_ID:-}" ]]; then
    HAS_NOTARY=1
  fi
  if [[ "$HAS_NOTARY" -eq 0 ]]; then
    echo "⚠️  未配置公证(notarization)变量:会签名但不公证。" >&2
    echo "    别人下载后仍会被 Gatekeeper 拦(提示「无法验证开发者」)。" >&2
  fi

  echo "▶ 签名身份: ${APPLE_SIGNING_IDENTITY:-(用 .p12: APPLE_CERTIFICATE)}"
  [[ "$HAS_NOTARY" -eq 1 ]] && echo "▶ 公证: 已启用,每个 target 构建后自动提交 Apple 并 staple"
fi

submit_for_notarization() {
  local artifact="$1"
  if [[ -n "${APPLE_API_KEY:-}" && -n "${APPLE_API_ISSUER:-}" && -n "${APPLE_API_KEY_PATH:-}" ]]; then
    xcrun notarytool submit "$artifact" \
      --key "$APPLE_API_KEY_PATH" \
      --key-id "$APPLE_API_KEY" \
      --issuer "$APPLE_API_ISSUER" \
      --wait
  else
    xcrun notarytool submit "$artifact" \
      --apple-id "$APPLE_ID" \
      --password "$APPLE_PASSWORD" \
      --team-id "$APPLE_TEAM_ID" \
      --wait
  fi
}

# 对指定目录下的 DMG 逐个公证 + staple(仅在 --signed 且配了公证时执行)
notarize_dmg_dir() {
  local dmg_dir="$1"
  if [[ "$(uname -s)" != "Darwin" || "$HAS_NOTARY" -eq 0 || ! -d "$dmg_dir" ]]; then
    return
  fi
  while IFS= read -r -d '' dmg; do
    if xcrun stapler validate "$dmg" >/dev/null 2>&1; then
      echo "▶ DMG 已有公证票据: $dmg"
      continue
    fi
    echo "▶ 公证 DMG: $dmg"
    submit_for_notarization "$dmg"
    echo "▶ Staple DMG: $dmg"
    xcrun stapler staple "$dmg"
    xcrun stapler validate "$dmg"
  done < <(find "$dmg_dir" -maxdepth 1 -type f -name '*.dmg' -print0)
}

mkdir -p "$DIST"

echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
echo "将依次构建以下目标: ${TRIPLES[*]}"
[[ "$SIGNED" -eq 1 ]] && echo "签名: 开启 (公证: $([[ "$HAS_NOTARY" -eq 1 ]] && echo 开启 || echo 关闭))" || echo "签名: 关闭 (本地测试包)"
echo "产物汇总目录: $DIST"
echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"

COLLECTED=()
for t in "${TRIPLES[@]}"; do
  echo ""
  echo "▶▶▶ 构建 $t ..."
  # bundle.targets 在 tauri.conf.json 里被钉成 ["nsis"](仅给 Windows 用),
  # macOS 上那是无效目标 —— 不覆盖的话 tauri 只编出二进制、不产 .app/.dmg。
  # 用第二个 --config 覆盖成 macOS 的 app+dmg(与 build:updater 同款叠加写法)。
  CI=true bun x tauri build --config "$CONF" \
    --config '{"bundle":{"targets":["app","dmg"]}}' \
    --target "$t" ${PASSTHRU[@]+"${PASSTHRU[@]}"}

  # tauri 把 DMG 放在 target/<triple>/release/bundle/dmg/*.dmg
  dmg_dir="$ROOT/target/$t/release/bundle/dmg"

  # 先公证(staple 会原地改写 DMG),再拷贝到汇总目录,保证收的是带票据的包
  notarize_dmg_dir "$dmg_dir"

  while IFS= read -r -d '' dmg; do
    cp -f "$dmg" "$DIST/"
    COLLECTED+=("$DIST/$(basename "$dmg")")
  done < <(find "$dmg_dir" -maxdepth 1 -type f -name '*.dmg' -print0 2>/dev/null)
done

# COLLECTED 为空 = 这一轮没产出任何 DMG(多半 bundle.targets 不含 dmg)。
# bash 3.2 下 `set -u` 还会让空数组展开直接报 unbound variable,所以这里既兜底
# 又把真正的失败原因说清楚,而不是假装「✅ 全部完成」。
if [[ "${#COLLECTED[@]}" -eq 0 ]]; then
  echo "" >&2
  echo "❌ 没有收集到任何 DMG —— tauri build 没在 macOS 上产出安装包。" >&2
  echo "   回看上面 tauri build 的输出:应出现 Bundling …dmg / Finished N bundles。" >&2
  echo "   若没有,多半是 bundle.targets 不含 dmg(本脚本本应已用 --config 覆盖)。" >&2
  exit 1
fi

echo ""
echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
echo "✅ 全部完成,DMG 已汇总到 $DIST :"
for f in "${COLLECTED[@]}"; do
  size="$(du -h "$f" | cut -f1)"
  printf "   %-40s %s\n" "$(basename "$f")" "$size"
done
echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
