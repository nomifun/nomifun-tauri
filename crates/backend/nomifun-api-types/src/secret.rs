//! Browser-use credential secret DTOs (P3-X2).
//!
//! A *secret* is a per-pet stored credential the browser engine can inject via a
//! `secret:NAME` reference (origin-bound, fail-closed). **The plaintext value is
//! write-only over the wire**: it is accepted on register and then encrypted into
//! a machine-bound vault — it is **NEVER** returned by any endpoint, never reaches
//! the LLM, the ref table, or logs (DESIGN §16). Listing therefore carries only the
//! non-sensitive metadata (name + the registrable domains it is bound to), mirroring
//! the webhook `has_secret` convention.

use serde::{Deserialize, Serialize};

/// Register (or overwrite) a browser-use credential secret for a pet.
///
/// `allowed_origins` may be bare hosts (`x.com`) or full origins
/// (`https://x.com:443`); the backend reduces each to its eTLD+1 (registrable
/// domain). The same `allowed_origins` also feed the browser's egress domain
/// allowlist (裁决⑤ 共用真值). An empty list, or one with no derivable eTLD+1, is
/// rejected with 400.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RegisterSecretRequest {
    /// The reference name used as `secret:NAME` in a `type`/`set_value` action.
    pub name: String,
    /// The plaintext credential. **Write-only**: encrypted into the vault and
    /// never returned. Required and non-empty.
    pub value: String,
    /// Origins/hosts this secret is bound to (reduced to eTLD+1 server-side).
    pub allowed_origins: Vec<String>,
}

/// A registered secret as returned to clients — **metadata only, never the value**.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SecretListItem {
    /// The secret's reference name.
    pub name: String,
    /// The registrable domains (eTLD+1) this secret is bound to.
    pub allowed_origins: Vec<String>,
}
