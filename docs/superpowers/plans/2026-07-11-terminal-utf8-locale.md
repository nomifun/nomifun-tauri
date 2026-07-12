# Terminal UTF-8 Locale Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Guarantee UTF-8 terminal output for macOS and Linux GUI/service launches while retaining explicit per-session locale overrides and the native Windows ConPTY UTF-8 path.

**Architecture:** Normalize terminal-only environment variables at `TerminalService::spawn_pty`, immediately before `portable-pty` receives the command. Keep raw PTY bytes and Base64 transport unchanged, and add a real macOS PTY reproduction plus frontend streaming-decoder boundary coverage.

**Tech Stack:** Rust, `portable-pty`, Tokio test utilities, tempfile, TypeScript, Bun test, xterm.js.

## Global Constraints

- Every NomiFun interactive terminal uses UTF-8 unless its create request explicitly supplies `LC_ALL`, `LC_CTYPE`, or `LANG`.
- Explicit per-session locale values remain authoritative.
- macOS uses `LANG=en_US.UTF-8`, `LC_CTYPE=UTF-8`, and `LC_ALL=en_US.UTF-8` only when a conflicting inherited `LC_ALL` must be repaired.
- Linux uses `LANG=C.UTF-8`, `LC_CTYPE=C.UTF-8`, and `LC_ALL=C.UTF-8` only when a conflicting inherited `LC_ALL` must be repaired.
- Windows receives no Unix locale variables; ConPTY remains the UTF-8 transport authority.
- PTY output remains raw bytes encoded as Base64 over JSON/WebSocket.
- No database or API migration.

---

### Task 1: Terminal environment UTF-8 normalization

**Files:**
- Modify: `crates/backend/nomifun-terminal/src/service.rs`
- Modify: `crates/backend/nomifun-terminal/src/pty.rs`
- Test: `crates/backend/nomifun-terminal/src/service.rs`
- Test: `crates/backend/nomifun-terminal/src/pty.rs`

**Interfaces:**
- Consumes: per-session `HashMap<String, String>` from `CreateTerminalRequest.env` and inherited process locale variables.
- Produces: `apply_emulator_env_defaults_with<F>(env: &mut HashMap<String, String>, inherited: F)` where `F: Fn(&str) -> Option<OsString>`; production uses `std::env::var_os`, tests inject deterministic values.

**Post-review correction:** The implemented interface returns `Option<OsString>` and production uses `std::env::var_os`, so non-Unicode inherited locale values cannot bypass repair. `PtyHandle::spawn` also calls `remove_inherited_locale_vars_shadowing_overrides` before applying session variables, removing inherited `LC_ALL`/`LC_CTYPE` only when their POSIX precedence would shadow an explicit narrower session override. Whitespace-only and whitespace-padded locale names are invalid and covered by regression tests.

- [ ] **Step 1: Add failing pure environment tests**

Add these tests next to the existing emulator-environment tests:

```rust
#[cfg(any(target_os = "macos", target_os = "linux"))]
#[test]
fn emulator_env_defaults_add_utf8_locale_when_inherited_locale_is_missing() {
    let mut env = HashMap::new();
    apply_emulator_env_defaults_with(&mut env, |_| None);
    assert_eq!(env.get("LANG").map(String::as_str), Some(UTF8_LANG));
    assert_eq!(env.get("LC_CTYPE").map(String::as_str), Some(UTF8_CTYPE));
    assert!(!env.contains_key("LC_ALL"));
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
#[test]
fn emulator_env_defaults_repairs_inherited_non_utf8_lc_all() {
    for inherited in ["C", "POSIX", "UTF-8", "zh_CN.GB18030", " ", " C.UTF-8", "C.UTF-8 "] {
        let mut env = HashMap::new();
        apply_emulator_env_defaults_with(&mut env, |key| {
            (key == "LC_ALL").then(|| inherited.into())
        });
        assert_eq!(env.get("LC_ALL").map(String::as_str), Some(UTF8_LANG));
    }
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
#[test]
fn emulator_env_defaults_preserves_inherited_utf8_lc_all_as_override() {
    let mut env = HashMap::new();
    apply_emulator_env_defaults_with(&mut env, |key| {
        (key == "LC_ALL").then(|| "C.UTF-8".into())
    });
    assert_eq!(env.get("LC_ALL").map(String::as_str), Some("C.UTF-8"));
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
#[test]
fn emulator_env_defaults_preserve_any_explicit_session_locale() {
    for key in ["LC_ALL", "LC_CTYPE", "LANG"] {
        let mut env = HashMap::from([(key.to_owned(), "zh_CN.GB18030".to_owned())]);
        apply_emulator_env_defaults_with(&mut env, |name| {
            (name == "LC_ALL").then(|| "C".into())
        });
        assert_eq!(env.get(key).map(String::as_str), Some("zh_CN.GB18030"));
        assert_eq!(
            ["LC_ALL", "LC_CTYPE", "LANG"]
                .iter()
                .filter(|candidate| env.contains_key(**candidate))
                .count(),
            1
        );
    }
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
#[test]
fn utf8_lc_all_detection_requires_a_complete_locale_name() {
    for value in ["C.UTF-8", "en_US.utf8", "zh_CN.UTF-8@variant"] {
        assert!(is_utf8_lc_all(std::ffi::OsStr::new(value)), "expected UTF-8 LC_ALL: {value}");
    }
    for value in ["", "C", "POSIX", "UTF-8", "utf8", "zh_CN.GB18030"] {
        assert!(!is_utf8_lc_all(std::ffi::OsStr::new(value)), "expected invalid/non-UTF-8 LC_ALL: {value}");
    }
}
```

- [ ] **Step 2: Run the tests and verify RED**

Run:

```bash
cargo test -p nomifun-terminal emulator_env_defaults -- --nocapture
```

Expected: compilation fails because `apply_emulator_env_defaults_with`, `UTF8_LANG`, `UTF8_CTYPE`, and `is_utf8_lc_all` do not exist yet.

- [ ] **Step 3: Implement the minimal normalization helper**

Replace the existing environment helper with:

```rust
const EXPLICIT_LOCALE_KEYS: [&str; 3] = ["LC_ALL", "LC_CTYPE", "LANG"];

#[cfg(target_os = "macos")]
const UTF8_LANG: &str = "en_US.UTF-8";
#[cfg(target_os = "macos")]
const UTF8_CTYPE: &str = "UTF-8";
#[cfg(target_os = "linux")]
const UTF8_LANG: &str = "C.UTF-8";
#[cfg(target_os = "linux")]
const UTF8_CTYPE: &str = "C.UTF-8";

fn apply_emulator_env_defaults(env: &mut HashMap<String, String>) {
    apply_emulator_env_defaults_with(env, |key| std::env::var_os(key));
}

fn apply_emulator_env_defaults_with<F>(env: &mut HashMap<String, String>, inherited: F)
where
    F: Fn(&str) -> Option<std::ffi::OsString>,
{
    env.entry("TERM".to_owned())
        .or_insert_with(|| "xterm-256color".to_owned());
    env.entry("COLORTERM".to_owned())
        .or_insert_with(|| "truecolor".to_owned());

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    apply_utf8_locale_defaults(env, inherited);
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    let _ = inherited;
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn apply_utf8_locale_defaults<F>(env: &mut HashMap<String, String>, inherited: F)
where
    F: Fn(&str) -> Option<std::ffi::OsString>,
{
    if EXPLICIT_LOCALE_KEYS.iter().any(|key| env.contains_key(*key)) {
        return;
    }

    env.insert("LANG".to_owned(), UTF8_LANG.to_owned());
    env.insert("LC_CTYPE".to_owned(), UTF8_CTYPE.to_owned());

    if let Some(value) = inherited("LC_ALL").filter(|value| !value.is_empty()) {
        let value = if is_utf8_lc_all(value.as_os_str()) {
            value.into_string().expect("a validated UTF-8 locale is Unicode")
        } else {
            UTF8_LANG.to_owned()
        };
        env.insert("LC_ALL".to_owned(), value);
    }
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn is_utf8_lc_all(value: &std::ffi::OsStr) -> bool {
    let Some(value) = value.to_str() else {
        return false;
    };
    if value.is_empty() || value.trim() != value {
        return false;
    }
    let Some((_, codeset_and_modifier)) = value.rsplit_once('.') else {
        return false;
    };
    codeset_and_modifier
        .split('@')
        .next()
        .unwrap_or_default()
        .replace('-', "")
        .eq_ignore_ascii_case("utf8")
}
```

Keep `spawn_pty` calling `apply_emulator_env_defaults(&mut env)` before lifecycle/MCP environment additions.

- [ ] **Step 4: Run the pure tests and verify GREEN**

Run:

```bash
cargo test -p nomifun-terminal emulator_env_defaults -- --nocapture
cargo test -p nomifun-terminal utf8_lc_all_detection_requires_a_complete_locale_name -- --nocapture
```

Expected: all selected tests pass on macOS; the same pure cases compile and pass with Linux constants on Linux CI.

- [ ] **Step 5: Commit the normalization logic**

```bash
git add crates/backend/nomifun-terminal/src/service.rs
git commit -m "fix(terminal): enforce UTF-8 locale defaults"
```

---

### Task 2: Real PTY Unicode filename regression

**Files:**
- Modify: `crates/backend/nomifun-terminal/src/service.rs`
- Test: `crates/backend/nomifun-terminal/src/service.rs`

**Interfaces:**
- Consumes: `apply_emulator_env_defaults_with`, `PtyHandle::spawn`, and the platform `/bin/ls` command.
- Produces: a macOS regression proving the original UTF-8 filename reaches the PTY output callback under a repaired inherited `LC_ALL=C` environment.

- [ ] **Step 1: Prove the original symptom independently**

Run:

```bash
mkdir -p /tmp/nomifun-locale-repro
touch '/tmp/nomifun-locale-repro/中文文件名.md'
env -i HOME="$HOME" PATH=/usr/bin:/bin TERM=xterm-256color /usr/bin/script -q /dev/null /bin/zsh -f -c 'cd /tmp/nomifun-locale-repro && /bin/ls -1'
```

Expected before the fix environment is applied: the TTY listing contains a run of `?` characters instead of `中文文件名.md`.

- [ ] **Step 2: Add the real PTY regression test**

Add this test after the existing real `TERM/COLORTERM` PTY test:

```rust
#[cfg(target_os = "macos")]
#[test]
fn pty_child_lists_unicode_filename_under_repaired_locale() {
    use crate::pty::{PtyHandle, SpawnParams};
    use std::sync::atomic::{AtomicBool, Ordering};

    let dir = tempfile::tempdir().expect("tempdir");
    let filename = "中文文件名.md";
    std::fs::write(dir.path().join(filename), b"content").expect("write unicode file");

    let mut env = HashMap::new();
    apply_emulator_env_defaults_with(&mut env, |key| {
        (key == "LC_ALL").then(|| "C".into())
    });

    let captured = Arc::new(Mutex::new(Vec::<u8>::new()));
    let cap = captured.clone();
    let done = Arc::new(AtomicBool::new(false));
    let done_cb = done.clone();
    let _handle = PtyHandle::spawn(
        SpawnParams {
            program: "/bin/ls".to_owned(),
            args: vec!["-1".to_owned()],
            cwd: dir.path().to_string_lossy().into_owned(),
            env,
            cols: 80,
            rows: 24,
        },
        0,
        move |chunk| cap.lock().unwrap().extend_from_slice(&chunk),
        move |_code, _sb| done_cb.store(true, Ordering::SeqCst),
    )
    .expect("spawn ls");

    for _ in 0..250 {
        if done.load(Ordering::SeqCst) {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    std::thread::sleep(Duration::from_millis(50));

    let output = String::from_utf8(captured.lock().unwrap().clone()).expect("UTF-8 PTY output");
    assert!(output.contains(filename), "unicode filename missing: {output:?}");
    assert!(!output.contains("????"), "filename was replaced before transport: {output:?}");
}
```

- [ ] **Step 3: Verify the real PTY regression passes**

Run:

```bash
cargo test -p nomifun-terminal pty_child_lists_unicode_filename_under_repaired_locale -- --nocapture
```

Expected: one test passes and its captured PTY bytes decode strictly as UTF-8 while containing the Chinese filename.

- [ ] **Step 4: Commit the end-to-end regression**

```bash
git add crates/backend/nomifun-terminal/src/service.rs
git commit -m "test(terminal): cover Unicode filenames in macOS PTY"
```

---

### Task 3: Frontend UTF-8 streaming boundary coverage

**Files:**
- Create: `ui/src/renderer/pages/terminal/terminalEncoding.test.ts`
- Test: `ui/src/renderer/pages/terminal/terminalEncoding.test.ts`

**Interfaces:**
- Consumes: `encodeStringToBase64`, `decodeBase64ToString`, and `createStreamingDecoder` from `terminalEncoding.ts`.
- Produces: regression coverage proving that binary-safe Base64 and the stateful decoder preserve CJK and emoji across arbitrary WebSocket chunk boundaries.

- [ ] **Step 1: Add the decoder characterization tests**

Create the test file:

```typescript
/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */
import { describe, expect, test } from 'bun:test';
import { createStreamingDecoder, decodeBase64ToString, encodeStringToBase64 } from './terminalEncoding';

function bytesToBase64(bytes: Uint8Array): string {
  let binary = '';
  bytes.forEach((byte) => {
    binary += String.fromCharCode(byte);
  });
  return btoa(binary);
}

describe('terminal UTF-8 encoding', () => {
  test('round-trips Chinese text and emoji through Base64', () => {
    const text = '中文文件名.md 🍜';
    expect(decodeBase64ToString(encodeStringToBase64(text))).toBe(text);
  });

  test('decodes every possible two-chunk byte split without replacement', () => {
    const text = '终端中文与 emoji：🍜🚀';
    const bytes = new TextEncoder().encode(text);
    for (let split = 1; split < bytes.length; split += 1) {
      const decode = createStreamingDecoder();
      const output =
        decode(bytesToBase64(bytes.slice(0, split))) + decode(bytesToBase64(bytes.slice(split)));
      expect(output).toBe(text);
      expect(output).not.toContain('\uFFFD');
    }
  });

  test('decodes a stream split into individual bytes', () => {
    const text = '逐字节：中文🍜';
    const decode = createStreamingDecoder();
    const output = [...new TextEncoder().encode(text)]
      .map((byte) => decode(bytesToBase64(Uint8Array.of(byte))))
      .join('');
    expect(output).toBe(text);
  });
});
```

- [ ] **Step 2: Run the decoder tests**

Run:

```bash
bun test ui/src/renderer/pages/terminal/terminalEncoding.test.ts
```

Expected: three tests pass. No production frontend change is expected because the per-session streaming decoder already implements the required behavior; this task locks it down against regression.

- [ ] **Step 3: Commit the frontend regression coverage**

```bash
git add ui/src/renderer/pages/terminal/terminalEncoding.test.ts
git commit -m "test(terminal): cover split UTF-8 output chunks"
```

---

### Task 4: Full verification and final review

**Files:**
- Verify: `crates/backend/nomifun-terminal/src/service.rs`
- Verify: `ui/src/renderer/pages/terminal/terminalEncoding.test.ts`
- Verify: `docs/superpowers/specs/2026-07-11-terminal-utf8-locale-design.md`

**Interfaces:**
- Consumes: all implementation and test artifacts from Tasks 1-3.
- Produces: fresh evidence for formatting, unit/integration behavior, frontend typing, and workspace compilation.

- [ ] **Step 1: Format Rust and verify no formatting drift**

Run:

```bash
cargo fmt --all
cargo fmt --all --check
```

Expected: the check exits 0 with no diff.

- [ ] **Step 2: Run all terminal backend tests**

Run:

```bash
cargo test -p nomifun-terminal -- --nocapture
```

Expected: all `nomifun-terminal` unit and integration tests pass with zero failures.

- [ ] **Step 3: Run all terminal frontend tests and UI type checking**

Run:

```bash
bun test ui/src/renderer/pages/terminal
bun run typecheck
```

Expected: all terminal UI tests pass and TypeScript exits 0.

- [ ] **Step 4: Run workspace compile checks**

Run:

```bash
cargo check --workspace --all-targets
bun run check
```

Expected: Rust workspace compilation and repository UI/static checks exit 0. Only Apple Rust targets are installed locally, so Linux and Windows runtime confirmation remains covered by target-gated code review and their CI runners rather than a false local emulation claim.

- [ ] **Step 5: Verify the final diff and requirements**

Run:

```bash
git diff --check 37d52dec..HEAD
git diff --stat 37d52dec..HEAD
git status --short
git log -4 --oneline
```

Expected: no whitespace errors; only the approved terminal implementation, tests, specification, and plan are present; the recent commits describe those changes.

- [ ] **Step 6: Commit plan/status changes if formatting touched tracked files**

If `cargo fmt --all` changed an approved task file after its task commit, stage only that file and commit it:

```bash
git add crates/backend/nomifun-terminal/src/service.rs
git commit -m "style(terminal): format UTF-8 locale fix"
```
