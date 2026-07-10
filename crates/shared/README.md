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
