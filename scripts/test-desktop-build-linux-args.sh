#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

if [[ "$(uname -s)" != "Linux" ]]; then
  echo "skip: desktop-build-linux args test requires Linux"
  exit 0
fi

TMP_DIR="$(mktemp -d)"
LOG="$TMP_DIR/bun-args.log"

cleanup() {
  rm -rf "$TMP_DIR"
}
trap cleanup EXIT

mkdir -p "$TMP_DIR/bin"

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

cat > "$TMP_DIR/bin/bun" <<'STUB'
#!/usr/bin/env bash
set -euo pipefail

printf "APPIMAGE_EXTRACT_AND_RUN=%s\n" "${APPIMAGE_EXTRACT_AND_RUN:-}" >> "$NOMIFUN_TEST_BUN_LOG"
for arg in "$@"; do
  printf "<%s>\n" "$arg" >> "$NOMIFUN_TEST_BUN_LOG"
done
printf -- "---\n" >> "$NOMIFUN_TEST_BUN_LOG"
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

chmod +x "$TMP_DIR/bin/rustup" "$TMP_DIR/bin/bun" "$TMP_DIR/bin/pkg-config"

run_build_linux() {
  PATH="$TMP_DIR/bin:$PATH" \
    NOMIFUN_TEST_BUN_LOG="$LOG" \
    bash "$ROOT/scripts/desktop-build-linux.sh" "$@" >/dev/null
}

assert_log_contains() {
  local expected="$1"
  if ! grep -Fxq "$expected" "$LOG"; then
    echo "expected bun args log to contain: $expected" >&2
    echo "actual log:" >&2
    cat "$LOG" >&2
    exit 1
  fi
}

: > "$LOG"
run_build_linux --config apps/desktop/tauri.updater.conf.json
assert_log_contains "<--config>"
assert_log_contains "<apps/desktop/tauri.updater.conf.json>"
assert_log_contains "<--target>"
assert_log_contains "<x86_64-unknown-linux-gnu>"
assert_log_contains "APPIMAGE_EXTRACT_AND_RUN=1"

: > "$LOG"
run_build_linux x64 --config apps/desktop/tauri.updater.conf.json
assert_log_contains "<--config>"
assert_log_contains "<apps/desktop/tauri.updater.conf.json>"
assert_log_contains "<--target>"
assert_log_contains "<x86_64-unknown-linux-gnu>"
assert_log_contains "APPIMAGE_EXTRACT_AND_RUN=1"

echo "desktop-build-linux args: ok"
