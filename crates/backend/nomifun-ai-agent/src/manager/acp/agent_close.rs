//! Close-path helpers for `AcpAgentManager`.
//!
//! Centralises the logic that turns a `send_message` failure into a
//! [`CloseReason`] so the next user-facing toast can show something
//! actionable instead of "Bad gateway" / "session closed". Lives in its
//! own file to keep `agent.rs` focused on the manager's high-level
//! lifecycle and to honour the project's per-file size budget.
//!
//! Security note: `peek_stderr_tail` returns raw subprocess stderr that
//! may carry secrets. The only safe consumer is
//! [`stderr_error_extractor::extract_error_message`], which filters
//! through an allowlist before any string reaches the
//! [`CloseReason::ProcessExited::redacted_summary`] field. Raw stderr
//! must NEVER be Display'd into HTTP responses or WebSocket events; it
//! is logged via `tracing` only.

use nomifun_common::AppError;

use crate::manager::acp::AcpAgentManager;
use crate::manager::acp::agent::{exit_status_parts, user_facing_message};
use crate::protocol::error::CloseReason;

/// How many trailing stderr lines we hand to the extractor.
/// 32 lines is well below the 8 KiB ring-buffer cap and comfortably
/// covers a tracing event with its preceding context.
pub(super) const STDERR_PEEK_LINES: usize = 32;

impl AcpAgentManager {
    /// If `err` is the "SDK gave us default Internal error with no data" shape,
    /// peek the child's recent stderr and try to surface a more informative
    /// message. Returns `None` when augmentation does not apply or finds nothing.
    ///
    /// Why string-matching: `AppError::BadGateway(String)` has discarded the
    /// structured `AcpError` by the time we see it. The default-message
    /// signature is narrow and stable enough that matching on the inner string
    /// is cheaper than threading typed errors through the manager API. Keep
    /// this in sync with `AcpError::Display` in
    /// `crates/nomifun-ai-agent/src/protocol/error.rs` if its fallback wording
    /// changes.
    pub(super) async fn augment_with_stderr(&self, err: &AppError) -> Option<String> {
        const SDK_DEFAULT_BAD_GATEWAY_PREFIX: &str = "Bad gateway: Agent internal error (code ";
        let display = err.to_string();
        // Match the Display produced for `AgentInternal` whenever the SDK gave
        // us its default "Internal error" message — with or without `data`.
        // When `data=Some`, Display ends in ") ({json})" which still satisfies
        // `ends_with(')')`, so we augment in that case too. That is intentional:
        // stderr context is generally more user-friendly than the raw `data`
        // JSON, and the operator log retains both via `ErrorChain`.
        // Examples that match:  "Bad gateway: Agent internal error (code -32603)"
        //                       "Bad gateway: Agent internal error (code -32099)"
        // Do NOT match anything that has a real upstream message after the prefix.
        let is_default_internal = display.starts_with(SDK_DEFAULT_BAD_GATEWAY_PREFIX) && display.ends_with(')');
        if !is_default_internal {
            return None;
        }

        // Read the last STDERR_PEEK_LINES lines of the child's stderr (cheap;
        // ring buffer is bounded to 8 KiB ≈ a few hundred lines max).
        let tail = self.process.peek_stderr_tail(STDERR_PEEK_LINES).await;
        super::stderr_error_extractor::extract_error_message(&tail)
    }

    /// Construct a [`CloseReason`] for a `send_message` failure. Captures
    /// whatever lifecycle context is still observable at the call site so
    /// the next user-facing toast can show something better than "Bad
    /// gateway" or "session closed".
    ///
    /// Two branches:
    ///
    /// 1. **Process already exited**: we can read the exit code/signal
    ///    directly from the `CliAgentProcess`. Even if the SDK rolled the
    ///    failure up as a generic `AgentInternal`, the exit metadata is
    ///    the actionable detail. The stderr tail is run through the
    ///    redaction allowlist (see [`stderr_error_extractor`]) so only
    ///    user-safe substrings reach the toast — raw stderr never leaves
    ///    `peek_stderr_tail`.
    /// 2. **Process still alive**: fall back to the existing
    ///    `AgentInternal` stderr-augmentation heuristic for the SDK's
    ///    "default Internal error" shape; otherwise the user-facing form
    ///    of the `AppError` is the best we can do.
    pub(super) async fn build_close_reason_from_error(&self, err: &AppError) -> CloseReason {
        // Branch 1 — process exit detected.
        if let Some(status) = self.process.exit_status() {
            let (exit_code, signal) = exit_status_parts(Some(status));
            let tail = self.process.peek_stderr_tail(STDERR_PEEK_LINES).await;
            // Redaction: extractor returns `None` unless the line matches the
            // allowlist. Empty `redacted_summary` is fine —
            // `CloseReason::user_facing_message` omits the trailing colon then.
            let redacted_summary = super::stderr_error_extractor::extract_error_message(&tail).unwrap_or_default();
            return CloseReason::ProcessExited {
                exit_code,
                signal,
                redacted_summary,
            };
        }

        // Branch 2 — process alive.
        if let Some(augmented) = self.augment_with_stderr(err).await {
            return CloseReason::Failed { display: augmented };
        }
        CloseReason::Failed {
            display: user_facing_message(err),
        }
    }
}

#[cfg(test)]
mod tests {
    //! Compositional tests for `build_close_reason_from_error`.
    //!
    //! We can't construct a real `AcpAgentManager` in a unit test (it
    //! needs the ACP SDK + catalog plumbing), but we CAN exercise the
    //! same branch logic against a real `CliAgentProcess`. Keep these
    //! helpers aligned with the production implementation: same
    //! `exit_status` branch, same peek size, same stderr extractor call.

    use std::sync::Arc;
    use std::time::Duration;

    use nomifun_common::AppError;

    use crate::capability::cli_process::CliAgentProcess;
    use crate::manager::acp::agent::{exit_status_parts, user_facing_message};
    use crate::protocol::error::CloseReason;

    /// Spawn a sh subprocess that writes `stderr_payload` to stderr then
    /// exits with `exit_code`. Used to simulate ACP CLI crashes/exits.
    async fn spawn_with_stderr_and_exit(stderr_payload: &str, exit_code: u8) -> Arc<CliAgentProcess> {
        // Heredoc protects apostrophes etc.; escape any literal `'` first.
        let payload = stderr_payload.replace('\'', "'\\''");
        let script = format!("cat <<'EOF' >&2\n{payload}\nEOF\nexit {exit_code}");
        let config = crate::capability::cli_process::tests::simple_script_config(&script);
        let proc = CliAgentProcess::spawn(config).await.unwrap();
        tokio::time::timeout(Duration::from_secs(5), proc.wait_for_exit())
            .await
            .unwrap();
        // Give the stderr reader a beat to flush its ring buffer.
        tokio::time::sleep(Duration::from_millis(100)).await;
        Arc::new(proc)
    }

    /// Mirror of `AcpAgentManager::build_close_reason_from_error` against a
    /// bare `CliAgentProcess` — the same two branches, the same peek size,
    /// the same extractor module path. Keep this aligned with the production
    /// helper or these tests stop reflecting reality.
    async fn build_close_reason_via_process(proc: &Arc<CliAgentProcess>, err: &AppError) -> CloseReason {
        if let Some(status) = proc.exit_status() {
            let (exit_code, signal) = exit_status_parts(Some(status));
            let tail = proc.peek_stderr_tail(super::STDERR_PEEK_LINES).await;
            let redacted_summary =
                crate::manager::acp::stderr_error_extractor::extract_error_message(&tail).unwrap_or_default();
            return CloseReason::ProcessExited {
                exit_code,
                signal,
                redacted_summary,
            };
        }
        // Branch 2 — process alive. Inline the augment_with_stderr logic so
        // this helper does not need to construct a real `AcpAgentManager`.
        const SDK_DEFAULT_BAD_GATEWAY_PREFIX: &str = "Bad gateway: Agent internal error (code ";
        let display = err.to_string();
        let is_default_internal = display.starts_with(SDK_DEFAULT_BAD_GATEWAY_PREFIX) && display.ends_with(')');
        if is_default_internal {
            let tail = proc.peek_stderr_tail(super::STDERR_PEEK_LINES).await;
            if let Some(extracted) = crate::manager::acp::stderr_error_extractor::extract_error_message(&tail) {
                return CloseReason::Failed { display: extracted };
            }
        }
        CloseReason::Failed {
            display: user_facing_message(err),
        }
    }

    /// ELECTRON-1K0 happy path: agent exits with non-zero before/in-flight
    /// of a request, the SDK rolls the failure up as a generic JSON-RPC
    /// "Internal error", and the close-path captures the upstream exit
    /// code AND extracts the user-safe stderr summary so the toast can
    /// show something better than "Bad gateway".
    #[tokio::test]
    async fn agent_exits_non_zero_before_initialize_yields_process_exited_with_exit_code() {
        let stderr =
            "\u{1b}[2m2026-05-13T20:01:21Z\u{1b}[0m \u{1b}[31mERROR\u{1b}[0m codex_acp::thread: usage limit exceeded";
        let proc = spawn_with_stderr_and_exit(stderr, 1).await;

        let err = AppError::BadGateway("Agent internal error (code -32603)".into());
        let reason = build_close_reason_via_process(&proc, &err).await;
        match reason {
            CloseReason::ProcessExited {
                exit_code,
                redacted_summary,
                ..
            } => {
                assert_eq!(exit_code, Some(1), "must capture upstream exit code");
                assert!(
                    redacted_summary.to_lowercase().contains("usage limit"),
                    "redacted summary must surface allowlisted stderr; got {redacted_summary}"
                );
                assert!(
                    !redacted_summary.contains("\u{1b}["),
                    "ANSI escapes must be stripped from redacted summary"
                );
            }
            other => panic!("expected ProcessExited, got {other:?}"),
        }
    }

    /// Stderr with no allowlisted keyword must not leak into the toast,
    /// but we still capture the exit code so the user sees *something*
    /// actionable beyond "session closed".
    #[tokio::test]
    async fn crash_without_allowlisted_stderr_keeps_exit_code_only() {
        let stderr = "ERROR widget_loader: failed to load module 'foo' due to internal logic bug";
        let proc = spawn_with_stderr_and_exit(stderr, 42).await;

        let err = AppError::BadGateway("Agent internal error (code -32603)".into());
        let reason = build_close_reason_via_process(&proc, &err).await;
        match reason {
            CloseReason::ProcessExited {
                exit_code,
                redacted_summary,
                ..
            } => {
                assert_eq!(exit_code, Some(42), "must capture upstream exit code");
                assert!(
                    redacted_summary.is_empty(),
                    "non-allowlisted stderr must not leak; got {redacted_summary:?}"
                );
                let msg = CloseReason::ProcessExited {
                    exit_code,
                    signal: None,
                    redacted_summary,
                }
                .user_facing_message();
                assert!(msg.contains("exit code 42"), "got {msg}");
            }
            other => panic!("expected ProcessExited, got {other:?}"),
        }
    }

    /// Stderr containing fake credential material must NOT reach the
    /// redacted summary — the allowlist filter only lets through the
    /// allowlisted line, and the SDK error path must not leak raw stderr.
    /// Regression guard for the close-path security rule (stderr ≠ HTTP).
    #[tokio::test]
    async fn crash_with_secret_in_stderr_does_not_leak_into_summary() {
        // "credentials" is allowlisted (upstream stacks emit "credentials
        // expired" / "invalid credentials" as the actionable line) — the
        // allowlist limits leakage to matching lines, but a non-matching
        // line carrying a secret must NOT reach the summary.
        let secret_bearer_line = "DEBUG net: req: GET /v1/x token=eyJabcdefXYZSECRETXYZ";
        let actionable_line = "ERROR auth: rate limit exceeded for tenant alpha";
        let payload = format!("{secret_bearer_line}\n{actionable_line}");
        let proc = spawn_with_stderr_and_exit(&payload, 2).await;

        let err = AppError::BadGateway("Agent internal error (code -32603)".into());
        let reason = build_close_reason_via_process(&proc, &err).await;
        match reason {
            CloseReason::ProcessExited { redacted_summary, .. } => {
                assert!(
                    !redacted_summary.contains("eyJabcdefXYZSECRETXYZ"),
                    "non-allowlisted secret line must never appear in summary; got {redacted_summary}"
                );
                assert!(
                    !redacted_summary.contains("token="),
                    "non-allowlisted token field must never appear in summary; got {redacted_summary}"
                );
            }
            other => panic!("expected ProcessExited, got {other:?}"),
        }
    }

    /// Manual cancel during a prompt is NOT a crash — the close reason
    /// must reflect "user cancelled" so the toast text differs from the
    /// process-died case. Pins the discriminator the toast layer uses to
    /// pick its copy.
    #[test]
    fn manual_cancel_close_reason_renders_distinctly_from_agent_crash() {
        let cancel_msg = CloseReason::UserCancel.user_facing_message();
        let crash_msg = CloseReason::ProcessExited {
            exit_code: Some(1),
            signal: None,
            redacted_summary: String::new(),
        }
        .user_facing_message();
        assert_ne!(cancel_msg, crash_msg, "toast copy must distinguish cancel from crash");
        assert!(cancel_msg.to_lowercase().contains("cancel"));
        assert!(crash_msg.contains("exit code 1"));
    }
}
