# Terminal UTF-8 Locale Design

## Problem

When NomiFun is launched as a macOS GUI application, Finder/launchd can provide a minimal process environment with no character locale. `portable-pty` inherits that environment for terminal children. In a TTY, macOS `ls` classifies UTF-8 filename bytes under the `C` locale as non-printable and replaces each character byte with ASCII `?` before NomiFun receives the output. Base64 transport, WebSocket delivery, `TextDecoder`, xterm.js, and font fallback therefore cannot recover the original filename.

The failure is reproducible outside NomiFun with a real macOS pseudo-terminal:

```text
no LANG/LC_CTYPE: 中文文件名.md -> ???????????????.md
UTF-8 LC_CTYPE:   中文文件名.md -> 中文文件名.md
```

Linux desktop and service launches can inherit the same missing, `C`, or `POSIX` locale. Windows uses ConPTY, whose input/output pseudoconsole streams are defined as UTF-8 and internally translate attached client code pages, so Unix locale variables must not be injected there.

## Encoding Contract

- Every NomiFun interactive terminal uses UTF-8 unless that terminal's create request explicitly supplies `LC_ALL`, `LC_CTYPE`, or `LANG`.
- Explicit per-session locale configuration remains authoritative, including an intentionally non-UTF-8 locale.
- The PTY byte protocol remains binary-safe Base64 over JSON/WebSocket.
- The frontend continues to decode the PTY stream as UTF-8 with one stateful decoder per session.
- The fix applies to shell terminals and agent CLI terminals because both pass through `TerminalService::spawn_pty`.

## Backend Design

Extend the existing terminal-emulator environment normalization in `nomifun-terminal` so it owns the complete environment contract at the PTY spawn boundary.

The helper will first apply the existing `TERM=xterm-256color` and `COLORTERM=truecolor` defaults. On Unix it will then apply a platform-valid UTF-8 character locale when the session did not explicitly provide any locale key:

- macOS: `LANG=en_US.UTF-8` and `LC_CTYPE=UTF-8`.
- Linux: `LANG=C.UTF-8` and `LC_CTYPE=C.UTF-8`.

`LC_CTYPE` changes character classification/encoding without unnecessarily changing numeric, monetary, collation, date, or message categories. If the inherited process has a non-UTF-8 `LC_ALL`, that higher-precedence value would defeat `LC_CTYPE`; in that case the child receives the platform UTF-8 fallback in `LC_ALL` as well. An inherited, syntactically complete UTF-8 `LC_ALL` such as `C.UTF-8` or `en_US.UTF-8` is materialized unchanged into the child overrides so the PTY precedence guard preserves it. Empty values and the `C`/`POSIX` locales are treated as non-UTF-8. A bare `LC_ALL=UTF-8` is also repaired: macOS accepts `UTF-8` as an `LC_CTYPE` alias but silently falls back to `C` when the same bare value is used for `LC_ALL`.

The implementation will make inherited-environment lookup injectable into the pure normalization helper. Production lookup uses `var_os` so a non-Unicode `LC_ALL` is observed and repaired rather than mistaken for absence. Tests can therefore cover missing, malformed, non-Unicode, and conflicting inherited variables without mutating the test process environment, which would be unsafe under parallel Rust tests.

POSIX locale precedence also has to be enforced when an explicit per-session value is narrower than the inherited one. Before applying the session environment, the Unix PTY command builder removes inherited `LC_ALL` when the session supplies `LC_CTYPE`, and removes inherited `LC_ALL` plus `LC_CTYPE` when the session supplies only `LANG`. An explicit session `LC_ALL` remains the highest-precedence value and is applied unchanged. This makes the documented per-session override authoritative rather than merely present in the child environment.

No Unix locale variables are added on Windows. `portable-pty` uses ConPTY there, and ConPTY already guarantees UTF-8 on the pseudoconsole channel.

## Data Flow

```text
CreateTerminalRequest.env
  -> TerminalService::spawn_pty
  -> terminal environment normalization
       -> xterm capability defaults
       -> Unix UTF-8 locale defaults/conflict repair
       -> lifecycle/MCP environment additions
  -> portable_pty::CommandBuilder
  -> child process emits UTF-8 PTY bytes
  -> Base64 terminal.output event
  -> per-session streaming TextDecoder
  -> xterm.js
```

Normalization happens before lifecycle and MCP variables are merged. Those integrations do not own locale variables and are unaffected.

## Error and Compatibility Handling

- Per-session locale keys are never silently rewritten.
- macOS uses its stable `UTF-8` character-locale alias; Linux uses the locale-independent `C.UTF-8` fallback used by current glibc and musl systems.
- Windows remains on its native ConPTY encoding path.
- No lossy backend string conversion is introduced; scrollback and live output remain raw bytes.
- Existing saved terminal rows require no migration. The correction takes effect on the next create or relaunch because environment defaults are applied at spawn time.

## Tests

The change follows red-green TDD:

1. Add pure Rust tests that fail against the current helper for missing locale, inherited `C`/`POSIX`, inherited non-UTF-8 `LC_ALL`, inherited UTF-8 `LC_ALL`, and explicit session overrides.
2. Add a real Unix PTY regression test that creates a filename containing Chinese characters, runs a TTY-aware listing, and asserts that the captured bytes contain the original UTF-8 filename and not a run of `?` characters. The macOS path specifically exercises the production failure shown in the screenshot.
3. Add frontend decoder tests for Chinese text and emoji split at every byte boundary so WebSocket chunking cannot corrupt valid UTF-8.
4. Run the focused Rust and frontend tests, Rust formatting, UI type checking, the `nomifun-terminal` test suite, and the available workspace verification commands.
5. Keep Windows-specific PTY tests/conditional compilation intact; Windows CI is the authoritative runtime verification for ConPTY behavior.

## Non-Goals

- Detecting or transcoding arbitrary legacy terminal encodings in the frontend.
- Changing application-wide process locale.
- Rewriting user-specified per-session locale settings.
- Changing the Base64/WebSocket terminal protocol.
