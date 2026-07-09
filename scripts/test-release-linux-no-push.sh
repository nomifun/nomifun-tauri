#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

if [[ "$(uname -s)" != "Linux" ]]; then
  echo "skip: release-linux test requires Linux"
  exit 0
fi

TMP_DIR="$(mktemp -d)"
WORK="$TMP_DIR/work"
LOG="$TMP_DIR/calls.log"

cleanup() {
  rm -rf "$TMP_DIR"
}
trap cleanup EXIT

mkdir -p "$WORK/scripts" "$WORK/apps/desktop/signing" "$WORK/apps/desktop/updater" "$WORK/apps/desktop" "$TMP_DIR/bin"
cp "$ROOT/scripts/release-linux.sh" "$WORK/scripts/release-linux.sh"
cp "$ROOT/scripts/desktop-build-linux.sh" "$WORK/scripts/desktop-build-linux.sh"
chmod +x "$WORK/scripts/release-linux.sh" "$WORK/scripts/desktop-build-linux.sh"

cat > "$WORK/Cargo.toml" <<'TOML'
[workspace.package]
version = "9.9.9"
TOML

printf "{}\n" > "$WORK/apps/desktop/tauri.updater.conf.json"
printf "fake updater key\n" > "$WORK/apps/desktop/signing/nomifun-updater.key"

cat > "$TMP_DIR/bin/gh" <<'STUB'
#!/usr/bin/env bash
set -euo pipefail

printf "gh %s\n" "$*" >> "$NOMIFUN_TEST_LOG"
if [[ "${1:-}" == "api" && "${2:-}" == "user" ]]; then
  printf "tester\n"
  exit 0
fi
if [[ "${1:-}" == "release" && "${2:-}" == "view" ]]; then
  exit 1
fi

echo "unexpected gh invocation: $*" >&2
exit 1
STUB

cat > "$TMP_DIR/bin/rustup" <<'STUB'
#!/usr/bin/env bash
set -euo pipefail

if [[ "$*" == "target list --installed" ]]; then
  printf "x86_64-unknown-linux-gnu\n"
  exit 0
fi
if [[ "${1:-}" == "target" && "${2:-}" == "add" ]]; then
  exit 0
fi

echo "unexpected rustup invocation: $*" >&2
exit 1
STUB

cat > "$TMP_DIR/bin/pkg-config" <<'STUB'
#!/usr/bin/env bash
set -euo pipefail

if [[ "${1:-}" == "--exists" ]]; then
  case "${2:-}" in
    gbm|librsvg-2.0|ayatana-appindicator3-0.1|appindicator3-0.1) exit 0 ;;
  esac
fi

echo "unexpected pkg-config invocation: $*" >&2
exit 1
STUB

cat > "$TMP_DIR/bin/bun" <<'STUB'
#!/usr/bin/env bash
set -euo pipefail

printf "bun %s\n" "$*" >> "$NOMIFUN_TEST_LOG"

if [[ "${1:-}" == "run" && "${2:-}" == "build:linux" ]]; then
  echo "release-linux should call scripts/desktop-build-linux.sh directly" >&2
  exit 42
fi

if [[ "${1:-}" == "x" && "${2:-}" == "tauri" && "${3:-}" == "build" ]]; then
  target=""
  args=("$@")
  for ((i = 0; i < ${#args[@]}; i++)); do
    if [[ "${args[$i]}" == "--target" ]]; then
      target="${args[$((i + 1))]:-}"
      break
    fi
  done
  [[ -n "$target" ]] || { echo "tauri build missing --target" >&2; exit 1; }
  mkdir -p "target/$target/release/bundle/deb" \
    "target/$target/release/bundle/appimage" \
    "target/$target/release/bundle/rpm"
  printf "deb\n" > "target/$target/release/bundle/deb/nomifun_9.9.9_amd64.deb"
  printf "appimage\n" > "target/$target/release/bundle/appimage/nomifun_9.9.9_amd64.AppImage"
  printf "rpm\n" > "target/$target/release/bundle/rpm/nomifun-9.9.9-1.x86_64.rpm"
  exit 0
fi

if [[ "${1:-}" == "scripts/prune-build.mjs" ]]; then
  exit 0
fi

if [[ "${1:-}" == "scripts/make-latest-json.mjs" ]]; then
  mkdir -p apps/desktop/updater dist/desktop
  printf "sig\n" > dist/desktop/nomifun_9.9.9_amd64.AppImage.sig
  cat > apps/desktop/updater/latest.json <<'JSON'
{
  "version": "9.9.9",
  "notes": "test",
  "platforms": {
    "linux-x86_64": {
      "signature": "fake",
      "url": "https://github.com/nomifun/nomifun-tauri/releases/download/v9.9.9/nomifun_9.9.9_amd64.AppImage"
    }
  }
}
JSON
  exit 0
fi

echo "unexpected bun invocation: $*" >&2
exit 1
STUB

chmod +x "$TMP_DIR/bin/gh" "$TMP_DIR/bin/rustup" "$TMP_DIR/bin/pkg-config" "$TMP_DIR/bin/bun"

(
  cd "$WORK"
  PATH="$TMP_DIR/bin:$PATH" \
    GH_TOKEN=fake-token \
    NOMIFUN_TEST_LOG="$LOG" \
    NOMIFUN_RELEASE_KEY_FILE="apps/desktop/signing/nomifun-updater.key" \
    bash scripts/release-linux.sh -SkipPull -NoPush -Notes "test release" > "$TMP_DIR/release.out"
)

grep -q "bun x tauri build" "$LOG"
grep -q -- "--config apps/desktop/tauri.updater.conf.json" "$LOG"
grep -q -- "-NoPush（CREATE）" "$TMP_DIR/release.out"

echo "release-linux no-push: ok"
