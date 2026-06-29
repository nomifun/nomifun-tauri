//! Terminal session auto-titling: turn the first interaction into a short,
//! work-content title. The mechanical [`default_name`](crate::service) is too
//! fixed ("Shell"/"Claude"/raw command); this lets a session's sidebar entry
//! read like what the user is actually doing.
//!
//! Two seams (wired in [`crate::service`]):
//! - agent CLIs (claude/codex): the first `TurnEnd` lifecycle event carries the
//!   assistant's first message → summarized by an LLM via [`TerminalTitleCompleter`].
//! - plain shell / no model / LLM failure: the first line the user types, taken
//!   verbatim and truncated by [`fallback_title`].
//!
//! Layering mirrors knowledge autogen: the trait lives here (the lower crate),
//! the provider-backed `LiveTerminalTitleCompleter` lives in `nomifun-ai-agent`,
//! and `nomifun-app` wires it. A terminal with no completer (tests / webui-only)
//! still auto-titles via the fallback — the feature never hard-depends on an LLM.

use nomifun_common::AppError;

/// Summarize terminal content into a short title. Implemented in
/// `nomifun-ai-agent` (`LiveTerminalTitleCompleter`) over the default provider/
/// model; absent in hosts without a provider layer, where the fallback is used.
#[async_trait::async_trait]
pub trait TerminalTitleCompleter: Send + Sync {
    /// Return a short (≤ ~20 chars) work-content title for `content`, or an
    /// error if no model is configured / the call fails (caller then falls back).
    async fn summarize(&self, content: &str) -> Result<String, AppError>;
}

/// Default character cap for a generated title (CJK-aware: counts `char`s, not
/// bytes, so ~20 Chinese characters fit).
pub const TITLE_MAX_CHARS: usize = 24;

/// Build a fallback title from raw user input: strip ANSI, take the first
/// non-empty line, drop control chars, trim, and cap to `max_chars` characters.
/// Returns an empty string when there is nothing usable (caller skips the write).
pub fn fallback_title(input: &str, max_chars: usize) -> String {
    let cleaned = crate::ansi::strip_ansi(input.as_bytes());
    let line = cleaned
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("");
    clamp_title(line, max_chars)
}

/// Normalize a candidate title (from an LLM or a raw line) into a clean,
/// single-line, length-capped string: strip control chars, collapse internal
/// whitespace runs, strip wrapping quotes/backticks, trim, and cap to
/// `max_chars` characters. Empty in → empty out.
pub fn clamp_title(raw: &str, max_chars: usize) -> String {
    // Collapse all whitespace (incl. newlines) to single spaces; drop other
    // control chars. Keeps multibyte (CJK) intact since we operate on `char`s.
    let mut collapsed = String::with_capacity(raw.len());
    let mut prev_space = false;
    for c in raw.chars() {
        if c.is_whitespace() {
            if !prev_space {
                collapsed.push(' ');
                prev_space = true;
            }
        } else if !c.is_control() {
            collapsed.push(c);
            prev_space = false;
        }
    }
    let trimmed = collapsed
        .trim()
        .trim_matches(|c| c == '"' || c == '\'' || c == '`')
        .trim();
    trimmed.chars().take(max_chars).collect::<String>().trim().to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fallback_takes_first_nonempty_line_trimmed() {
        assert_eq!(fallback_title("  npm run build  \n more", 40), "npm run build");
        assert_eq!(fallback_title("\n\n  git status\n", 40), "git status");
    }

    #[test]
    fn fallback_strips_ansi_and_control_chars() {
        // ESC[31m red ESC[0m + a stray CR — only the visible text survives.
        let input = "\u{1b}[31mdeploy prod\u{1b}[0m\r";
        assert_eq!(fallback_title(input, 40), "deploy prod");
    }

    #[test]
    fn fallback_caps_to_max_chars_counting_chars_not_bytes() {
        // 10 Chinese chars capped to 6 → 6 chars (not bytes).
        let s = fallback_title("部署生产环境的脚本任务", 6);
        assert_eq!(s.chars().count(), 6);
    }

    #[test]
    fn fallback_empty_input_is_empty() {
        assert_eq!(fallback_title("   \n\t", 40), "");
        assert_eq!(fallback_title("", 40), "");
    }

    #[test]
    fn clamp_strips_quotes_and_collapses_whitespace() {
        assert_eq!(clamp_title("  \"Fix   the\nlogin bug\"  ", 40), "Fix the login bug");
        assert_eq!(clamp_title("`build`", 40), "build");
    }
}
