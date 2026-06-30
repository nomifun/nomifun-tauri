#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

if [[ "$(uname -s)" != "Darwin" ]]; then
  echo "skip: desktop-build-mac args test requires macOS"
  exit 0
fi

TMP_DIR="$(mktemp -d)"
FAKE_DMG="$TMP_DIR/NomiFun-test.dmg"
LOG="$TMP_DIR/bun-args.log"

cleanup() {
  rm -rf "$TMP_DIR"
  rm -f "$ROOT/dist/desktop/$(basename "$FAKE_DMG")"
}
trap cleanup EXIT

mkdir -p "$TMP_DIR/bin"
printf "fake dmg\n" > "$FAKE_DMG"

cat > "$TMP_DIR/bin/rustup" <<'STUB'
#!/usr/bin/env bash
set -euo pipefail

if [[ "$*" == "target list --installed" ]]; then
  printf "aarch64-apple-darwin\nx86_64-apple-darwin\n"
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

for arg in "$@"; do
  printf "<%s>\n" "$arg" >> "$NOMIFUN_TEST_BUN_LOG"
done
printf -- "---\n" >> "$NOMIFUN_TEST_BUN_LOG"
STUB

cat > "$TMP_DIR/bin/find" <<'STUB'
#!/usr/bin/env bash
set -euo pipefail

for arg in "$@"; do
  if [[ "$arg" == "*.dmg" ]]; then
    printf "%s\0" "$NOMIFUN_TEST_DMG"
    exit 0
  fi
done

/usr/bin/find "$@"
STUB

chmod +x "$TMP_DIR/bin/rustup" "$TMP_DIR/bin/bun" "$TMP_DIR/bin/find"

run_build_mac() {
  PATH="$TMP_DIR/bin:$PATH" \
    NOMIFUN_TEST_DMG="$FAKE_DMG" \
    NOMIFUN_TEST_BUN_LOG="$LOG" \
    bash "$ROOT/scripts/desktop-build-mac.sh" "$@" >/dev/null
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
run_build_mac --config '{"bundle":{"createUpdaterArtifacts":true}}'
assert_log_contains "<--config>"
assert_log_contains '<{"bundle":{"createUpdaterArtifacts":true}}>'
assert_log_contains "<--target>"
assert_log_contains "<universal-apple-darwin>"

: > "$LOG"
run_build_mac arm --config '{"bundle":{"createUpdaterArtifacts":true}}'
assert_log_contains "<--config>"
assert_log_contains '<{"bundle":{"createUpdaterArtifacts":true}}>'
assert_log_contains "<--target>"
assert_log_contains "<aarch64-apple-darwin>"

echo "desktop-build-mac args: ok"
