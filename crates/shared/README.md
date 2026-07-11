# crates/shared

Cross-layer Rust crates used by both the `agent` and `backend` groups.

Current crates:

| Crate | Role |
| --- | --- |
| `nomifun-net` | Shared outbound HTTP client/proxy behavior. |
| `nomi-redact` | Shared redaction helpers for sensitive text. |
| `nomi-execution` | Backend-neutral process execution contracts and supervision. |

`crates/shared/*` is part of the workspace membership in the root
`Cargo.toml`. Add a crate here only when it genuinely belongs on both sides of
the backend/agent boundary; otherwise keep it in the owning group.

## Process runtime boundary

`nomi-execution` is the single supervised runtime for Wave A command paths.
`Bash`, `exec_command`, and `write_stdin` are schema adapters over one shared
`ProcessSupervisor`; `nomifun-runtime` is a compatibility facade over the same
shared primitives.

OS ownership belongs only in `nomi-execution/src/platform`: Windows Jobs and
ConPTY, Unix process groups and watchdogs, and Unix PTY descriptors must not be
reimplemented in command adapters. Explicit user hand-off launch code is
limited to exact reviewed call sites rather than directory-wide exemptions.

Run `bun run check:process-runtime-boundary` locally. The
`command-reliability.yml` workflow runs this gate and the execution/runtime/tool
contract suites on Windows, macOS, and Linux.
