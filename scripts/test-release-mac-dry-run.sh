#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

if [[ "$(uname -s)" != "Darwin" ]]; then
  echo "skip: release-mac dry-run test requires macOS"
  exit 0
fi

TMP_DIR="$(mktemp -d)"
cleanup() {
  rm -rf "$TMP_DIR"
}
trap cleanup EXIT

mkdir -p "$TMP_DIR/bin" "$TMP_DIR/signing"
printf "fake updater key\n" > "$TMP_DIR/signing/nomifun-updater.key"
printf "APPLE_SIGNING_IDENTITY=Developer ID Application: Test\n" > "$TMP_DIR/signing/.env.signing"
printf "GH_TOKEN=fake-token\n" > "$TMP_DIR/signing/.env.release"

cat > "$TMP_DIR/bin/git" <<'STUB'
#!/usr/bin/env bash
set -euo pipefail

printf "git %s\n" "$*" >> "$NOMIFUN_TEST_LOG"
if [[ "${1:-}" == "pull" ]]; then
  exit 0
fi
echo "unexpected git invocation: $*" >&2
exit 1
STUB

cat > "$TMP_DIR/bin/gh" <<'STUB'
#!/usr/bin/env bash
set -euo pipefail

printf "gh %s\n" "$*" >> "$NOMIFUN_TEST_LOG"
if [[ "${1:-}" == "api" && "${2:-}" == "user" ]]; then
  printf "tester\n"
  exit 0
fi
if [[ "${1:-}" == "release" && "${2:-}" == "view" ]]; then
  [[ "${NOMIFUN_TEST_RELEASE_EXISTS:-1}" == "1" ]] && exit 0 || exit 1
fi
echo "unexpected gh invocation: $*" >&2
exit 1
STUB

chmod +x "$TMP_DIR/bin/git" "$TMP_DIR/bin/gh"

run_release_mac() {
  PATH="$TMP_DIR/bin:$PATH" \
    GH_TOKEN=fake-token \
    NOMIFUN_TEST_LOG="$TMP_DIR/calls.log" \
    NOMIFUN_RELEASE_KEY_FILE="$TMP_DIR/signing/nomifun-updater.key" \
    NOMIFUN_RELEASE_ENV_FILE="$TMP_DIR/signing/.env.release" \
    NOMIFUN_RELEASE_SIGNING_ENV="$TMP_DIR/signing/.env.signing" \
    bash "$ROOT/scripts/release-mac.sh" "$@"
}

: > "$TMP_DIR/calls.log"
NOMIFUN_TEST_RELEASE_EXISTS=1 run_release_mac -DryRun -SkipPull > "$TMP_DIR/append.out"
grep -q "模式      : APPEND" "$TMP_DIR/append.out"
grep -q "✅ -DryRun" "$TMP_DIR/append.out"
if grep -q "build:mac" "$TMP_DIR/calls.log"; then
  echo "dry-run unexpectedly attempted a build" >&2
  exit 1
fi

set +e
NOMIFUN_TEST_RELEASE_EXISTS=0 run_release_mac -Version 9.9.9 -DryRun -SkipPull > "$TMP_DIR/create.out" 2>&1
status=$?
set -e
if [[ "$status" -eq 0 ]]; then
  echo "CREATE dry-run without notes should fail" >&2
  cat "$TMP_DIR/create.out" >&2
  exit 1
fi
grep -q "首发(CREATE)需要 release note" "$TMP_DIR/create.out"

echo "release-mac dry-run: ok"
