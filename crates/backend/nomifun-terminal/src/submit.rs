//! Shared PTY submit encoding: turn a block of text into the ordered writes that
//! make a terminal actually EXECUTE it (as if a human typed it + Enter).
//!
//! Two hard rules this encoder enforces (both are fixes for real bugs):
//!   1. A single logical line is NEVER wrapped in bracketed paste — wrapping it
//!      inserts the text without submitting (it sits unrun in the input box).
//!   2. For multi-line text sent to an agent TUI (claude/codex/gemini) the submit
//!      CR is a SEPARATE write after `TERMINAL_SUBMIT_DELAY` — a CR riding in the
//!      same write as the paste-end marker is swallowed by the CLI's paste-burst
//!      detection. A plain shell has no such detector, so its CR rides along.

use std::time::Duration;

/// Beat between writing the bracketed-paste body and the lone submit CR when the
/// target is an agent TUI. See rule 2 above.
pub const TERMINAL_SUBMIT_DELAY: Duration = Duration::from_millis(120);

/// Output-quiescence window used to presume a non-lifecycle terminal (shell /
/// gemini) has finished a turn: this long with no new PTY output → settled.
pub const IDLE_SETTLE_WINDOW: Duration = Duration::from_millis(700);

/// The ordered PTY writes that submit a block of text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SubmitChunks {
    /// One write of exactly these bytes (raw text + CR, or paste-wrapped + CR).
    Single(Vec<u8>),
    /// Two writes: `paste` first, then `cr` after `TERMINAL_SUBMIT_DELAY`.
    PasteThenCr { paste: Vec<u8>, cr: Vec<u8> },
}

/// Encode `text` into submit chunks. `is_agent_tui` must be true when the target
/// CLI has paste-burst detection (claude/codex/gemini) — resolve it with
/// `resolve_agent_family(..).is_some()`.
pub fn encode_submit_chunks(text: &str, is_agent_tui: bool) -> SubmitChunks {
    let trimmed = text.trim_end_matches(|c| c == '\r' || c == '\n');
    if trimmed.contains('\n') && is_agent_tui {
        let mut paste = Vec::with_capacity(trimmed.len() + 13);
        paste.extend_from_slice(b"\x1b[200~");
        paste.extend_from_slice(trimmed.as_bytes());
        paste.extend_from_slice(b"\x1b[201~");
        SubmitChunks::PasteThenCr { paste, cr: vec![b'\r'] }
    } else {
        let mut bytes = Vec::with_capacity(trimmed.len() + 1);
        bytes.extend_from_slice(trimmed.as_bytes());
        bytes.push(b'\r');
        SubmitChunks::Single(bytes)
    }
}

/// Why `await_turn_settle` returned. `Idle` is a heuristic (output went quiet),
/// never a definitive "the agent declared done" — reported honestly as such.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SettleReason {
    /// A structured `TurnEnd` lifecycle event arrived (claude/codex).
    TurnEnd,
    /// No lifecycle signal (shell/gemini); PTY output was quiet for the window.
    Idle,
    /// Neither TurnEnd nor an idle window before the caller's timeout.
    Timeout,
    /// The PTY exited during the wait.
    Exited,
}

#[cfg(test)]
mod tests {
    use super::*;

    const PASTE_START: &[u8] = b"\x1b[200~";
    const PASTE_END: &[u8] = b"\x1b[201~";

    #[test]
    fn single_line_is_raw_plus_cr_for_any_target() {
        // 单行：shell 与 agent 都必须是 raw + CR，一次写，绝不 bracketed-paste。
        for agent in [false, true] {
            assert_eq!(
                encode_submit_chunks("git status", agent),
                SubmitChunks::Single(b"git status\r".to_vec()),
                "agent={agent}"
            );
        }
    }

    #[test]
    fn trailing_crlf_is_stripped_before_routing() {
        // 结尾换行不得把单行误判成多行 paste 路径。
        assert_eq!(
            encode_submit_chunks("ls\r\n", false),
            SubmitChunks::Single(b"ls\r".to_vec())
        );
        assert_eq!(
            encode_submit_chunks("ls\n", true),
            SubmitChunks::Single(b"ls\r".to_vec())
        );
    }

    #[test]
    fn multiline_to_agent_is_paste_then_separate_cr() {
        let out = encode_submit_chunks("line1\nline2", true);
        match out {
            SubmitChunks::PasteThenCr { paste, cr } => {
                assert!(paste.starts_with(PASTE_START));
                assert!(paste.ends_with(PASTE_END));
                assert!(paste.windows(11).any(|w| w == b"line1\nline2"));
                assert_eq!(cr, b"\r".to_vec());
            }
            other => panic!("expected PasteThenCr, got {other:?}"),
        }
    }

    #[test]
    fn multiline_to_shell_is_raw_plus_cr_one_write() {
        let out = encode_submit_chunks("a\nb", false);
        match out {
            SubmitChunks::Single(bytes) => {
                assert_eq!(bytes, b"a\nb\r".to_vec());
                assert!(!bytes.windows(PASTE_START.len()).any(|w| w == PASTE_START));
                assert!(!bytes.windows(PASTE_END.len()).any(|w| w == PASTE_END));
            }
            other => panic!("expected Single, got {other:?}"),
        }
    }
}
