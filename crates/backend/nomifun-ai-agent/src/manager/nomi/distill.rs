//! Post-session memory distillation workflow for the nomi engine.
//!
//! This is the async/LLM half of spec-G: the pure functions live in
//! `nomi_memory::distill`. Here we gate on an opt-in flag, redact the
//! transcript (gate 1), call the provider once (with a single parse retry),
//! redact each distilled entry (gate 2), and synchronously commit the small
//! file update before the owning turn may publish its terminal event.
//!
//! Discipline: distillation is an exact child of the accepted turn. Every
//! failure path still degrades silently (debug/warn log, never `emit_error`),
//! but there is no detached task and therefore no provider call or filesystem
//! mutation after the turn reaches `Finished`.

use std::path::PathBuf;
use std::sync::Arc;

use nomi_config::config::Config;
use nomi_memory::distill::{
    DistillOutput, apply_distilled, build_distill_prompt, parse_distill_output, DISTILL_SYSTEM,
};
use nomi_redact::redact_secrets_owned;

use crate::factory::provider_config::{one_shot_completion, user_message};

/// Token ceiling for the distillation completion. codex Phase1 runs
/// low-effort; nomi's `one_shot_completion` already sends no reasoning_effort,
/// and a small ceiling keeps the cost of each distilled session bounded.
const DISTILL_MAX_TOKENS: u32 = 2048;

/// Environment-variable gate. Distillation adds one extra LLM call per normal
/// work session (token cost), so it is OFF unless explicitly enabled — this
/// avoids surprising users with unexpected spend. nomi-config has no memory
/// section today, so an env flag is the lowest-risk gate (same pattern as the
/// `NOMIFUN_COMPUTER_USE` / `NOMIFUN_BROWSER_USE` host flags).
const DISTILL_ENABLED_ENV: &str = "NOMIFUN_MEMORY_DISTILL";

/// Whether distillation is enabled for this host. `"1"` / `"true"`
/// (case-insensitive) enable it; anything else (including unset) keeps it off.
pub(super) fn distill_enabled() -> bool {
    std::env::var(DISTILL_ENABLED_ENV)
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
}

/// Run distillation as an exact child of one accepted turn. `true` means the
/// child completed and the caller may proceed toward its terminal transition;
/// `false` means cancellation won and the provider future was dropped before
/// any later filesystem stage could start.
pub(super) async fn run_distill_exact_turn(
    cancel: &tokio_util::sync::CancellationToken,
    cfg: Arc<Config>,
    dir: PathBuf,
    transcript: String,
) -> bool {
    await_exact_turn_child(cancel, run_distill(cfg, dir, transcript)).await
}

async fn await_exact_turn_child(
    cancel: &tokio_util::sync::CancellationToken,
    child: impl std::future::Future<Output = ()>,
) -> bool {
    tokio::pin!(child);
    tokio::select! {
        biased;
        _ = cancel.cancelled() => false,
        _ = &mut child => !cancel.is_cancelled(),
    }
}

/// Run one post-session distillation. Caller has already decided this turn is
/// eligible (not companion, origin empty, `distill_dir` set) and that the gate
/// is on. `transcript` is the engine's role-tagged history snapshot.
async fn run_distill(cfg: Arc<Config>, dir: PathBuf, transcript: String) {
    // Gate 1: redact the transcript before it is uploaded to the provider.
    let transcript = redact_secrets_owned(transcript);
    if transcript.trim().is_empty() {
        return;
    }
    let prompt = build_distill_prompt(&transcript);

    // One parse retry (the model occasionally wraps JSON in prose); a provider
    // failure does not burn the retry. Mirrors the companion learner's policy.
    let mut parsed: Option<DistillOutput> = None;
    for _ in 0..2 {
        match one_shot_completion(&cfg, DISTILL_SYSTEM, vec![user_message(&prompt)], DISTILL_MAX_TOKENS).await {
            Ok(raw) => match parse_distill_output(&raw) {
                Ok(out) => {
                    parsed = Some(out);
                    break;
                }
                Err(e) => tracing::debug!(error = %e, "distill output unparseable"),
            },
            Err(e) => {
                tracing::debug!(error = %e, "distill provider call failed");
                break; // provider failure: don't retry
            }
        }
    }

    let Some(mut out) = parsed else {
        return;
    };
    if out.memories.is_empty() {
        return; // no-op gate hit: nothing worth keeping
    }

    // Gate 2: redact every distilled field before it touches disk.
    for m in &mut out.memories {
        m.content = redact_secrets_owned(std::mem::take(&mut m.content));
        m.description = redact_secrets_owned(std::mem::take(&mut m.description));
    }

    // `apply_distilled` is a small synchronous atomic file update. Keep it in
    // this future instead of `spawn_blocking`: dropping a JoinHandle cannot
    // cancel a started blocking closure, which previously allowed a late write
    // after cancellation/Finished. Once this section starts it has no await
    // point, so the write completes before cancellation or terminal emission
    // can be observed on this runtime thread.
    match apply_distilled(&dir, &out) {
        Ok(n) if n > 0 => {
            tracing::info!(written = n, dir = %dir.display(), "session distilled to file-based memory")
        }
        Ok(_) => {} // all candidates deduped / filtered
        Err(e) => tracing::warn!(error = %e, "distill apply failed"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn distill_enabled_reads_env() {
        // We avoid mutating the process env in a parallel test run; just assert
        // the default (unset in CI) is off. Explicit on/off parsing is covered
        // by the simple string comparison in `distill_enabled`.
        let key = DISTILL_ENABLED_ENV;
        if std::env::var(key).is_err() {
            assert!(!distill_enabled());
        }
    }

    #[tokio::test]
    async fn exact_turn_child_completes_before_terminal_and_runs_once() {
        let cancel = tokio_util::sync::CancellationToken::new();
        let phases = Arc::new(std::sync::Mutex::new(Vec::new()));
        let child_runs = Arc::new(AtomicUsize::new(0));
        let child_phases = Arc::clone(&phases);
        let child_runs_ref = Arc::clone(&child_runs);

        let completed = await_exact_turn_child(&cancel, async move {
            child_runs_ref.fetch_add(1, Ordering::SeqCst);
            child_phases.lock().unwrap().push("distill-apply");
        })
        .await;
        assert!(completed);
        phases.lock().unwrap().push("finish");

        assert_eq!(child_runs.load(Ordering::SeqCst), 1);
        assert_eq!(
            phases.lock().unwrap().as_slice(),
            ["distill-apply", "finish"],
            "the terminal boundary must be strictly after the exact child"
        );
    }

    #[tokio::test]
    async fn cancelling_exact_turn_child_leaves_no_late_effect() {
        let cancel = tokio_util::sync::CancellationToken::new();
        let late_write = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (_release_tx, release_rx) = tokio::sync::oneshot::channel::<()>();
        let child_cancel = cancel.clone();
        let child_late_write = Arc::clone(&late_write);

        let child = tokio::spawn(async move {
            await_exact_turn_child(&child_cancel, async move {
                let _ = started_tx.send(());
                let _ = release_rx.await;
                child_late_write.store(true, Ordering::SeqCst);
            })
            .await
        });
        started_rx.await.expect("child started");
        cancel.cancel();
        assert!(!child.await.expect("join exact child"));
        tokio::task::yield_now().await;
        assert!(
            !late_write.load(Ordering::SeqCst),
            "dropping the provider child on cancellation must prevent any later apply"
        );
    }
}
