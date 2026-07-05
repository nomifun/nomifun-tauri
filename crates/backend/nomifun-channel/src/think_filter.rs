//! Streaming-safe stripping of inline reasoning (`<think>` / `<thinking>`) from
//! text pushed to IM channels.
//!
//! ## Why this exists (channel-specific)
//!
//! Reasoning models (MiniMax, Qwen, DeepSeek-R1 distills, …) frequently emit
//! their chain-of-thought **inline in the assistant content** wrapped in
//! `<think>…</think>` / `<thinking>…</thinking>` tags, rather than through the
//! provider's separate `reasoning_content` channel. That inline text arrives as
//! ordinary `Text` stream events, so the [`crate::stream_relay::ChannelStreamRelay`]
//! accumulates it into its buffer and would push the whole reasoning dump to the
//! IM chat (long, noisy). Structured thinking events are already dropped by the
//! relay — only this inline form leaks.
//!
//! The desktop UI never shows this reasoning because it filters twice:
//! - backend, on the finished message: `nomifun_conversation::response_middleware::strip_think_tags`
//! - frontend, while streaming / for history: `ui/src/renderer/utils/chat/thinkTagFilter.ts`
//!
//! The IM relay has no frontend, and its raw token stream never went through the
//! completed-message middleware — hence this server-side, streaming-aware filter.
//!
//! ## Streaming model & [`Stage`]
//!
//! The relay always holds the **full accumulated buffer** and re-renders it on
//! each throttled edit / final send, so this function is stateless and
//! idempotent: it is re-run over the whole buffer every time (no cross-delta
//! state machine).
//!
//! The one behaviour that differs between a mid-stream edit and the terminal
//! render is how a **trailing incomplete tag fragment** (`"…<thi"`, `"…</thinki"`)
//! is treated:
//! - [`Stage::Streaming`] hides it — more text is coming and it may complete
//!   into a real tag, so showing a half-tag would flash junk.
//! - [`Stage::Final`] keeps it — the stream is over, so a trailing `<` (or any
//!   partial fragment) is genuine literal content, not the start of a tag.
//!   Truncating it there would silently drop the last character(s) of a real
//!   answer (e.g. `"the less-than symbol is <"`).
//!
//! ## Differences from the two sibling filters (intentional)
//!
//! - Strict tag syntax (no `< think >` whitespace tolerance): providers emit
//!   exact tags, and strict syntax keeps the fast path a plain substring check.
//!   The frontend tolerates spaces only to salvage hand-authored / historical data.
//! - The slow path trims card edges (an IM message is a standalone card; leading
//!   / trailing blank lines are noise). The fast path returns the input verbatim.
//!
//! ## Accepted limitation
//!
//! The MiniMax orphan-close form (opening tag omitted: `"reasoning…</think>answer"`)
//! is indistinguishable from plain text **until** the `</think>` arrives — so an
//! editable-platform throttled edit may briefly flash that reasoning, healed by
//! the next edit. The final card is always clean. This matches the desktop
//! frontend's behaviour.

use std::borrow::Cow;
use std::sync::LazyLock;

use regex::Regex;

/// Which render the strip is feeding — controls trailing-partial-tag handling.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    /// A mid-stream throttled edit / tool-call flush: more text may follow, so
    /// hide a trailing incomplete tag fragment to avoid flashing a half-tag.
    Streaming,
    /// The terminal render (turn finished / stream closed): keep trailing
    /// fragments — they are literal content, not a tag under construction.
    Final,
}

/// Complete `<think>…</think>` / `<thinking>…</thinking>` block (dot-all).
static RE_COMPLETE_BLOCK: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?s)<think(?:ing)?>.*?</think(?:ing)?>").expect("valid think-block regex"));
/// Head-orphan close: everything up to & including the first close tag (dot-all).
static RE_HEAD_ORPHAN_CLOSE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?s)^.*?</think(?:ing)?>").expect("valid head-orphan-close regex"));
/// A bare opening tag.
static RE_OPEN_TAG: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"<think(?:ing)?>").expect("valid open-tag regex"));
/// A bare closing tag.
static RE_CLOSE_TAG: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"</think(?:ing)?>").expect("valid close-tag regex"));
/// Three or more consecutive newlines.
static RE_EXCESS_NEWLINES: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\n{3,}").expect("valid excess-newline regex"));

/// Full tags a trailing fragment could be a strict prefix of.
const TAG_CANDIDATES: [&str; 4] = ["<think>", "<thinking>", "</think>", "</thinking>"];

/// If the buffer ends with an *incomplete* think tag (`"…<thi"`, `"…</thinki"`),
/// return the byte offset of its leading `<` so the caller can hide it until the
/// tag completes. Returns `None` otherwise (including for complete or non-think
/// `<…>` fragments).
fn trailing_partial_tag_start(text: &str) -> Option<usize> {
    let bytes = text.as_bytes();
    // The longest candidate ("</thinking>") is 11 bytes, so its longest *strict*
    // prefix is 10 — only the last 10 bytes can host a partial tag.
    let scan_from = bytes.len().saturating_sub(10);
    // ASCII '<' (0x3C) never appears inside a multi-byte UTF-8 sequence, so this
    // byte scan lands on a real char boundary (CJK-safe).
    let rel = bytes[scan_from..].iter().rposition(|&b| b == b'<')?;
    let lt = scan_from + rel;
    let frag = &text[lt..];
    TAG_CANDIDATES
        .iter()
        .any(|c| c.len() > frag.len() && c.starts_with(frag))
        .then_some(lt)
}

/// Strip inline reasoning from `text`, streaming-safe. See the module docs for
/// the ordering rationale — each step is load-bearing. `stage` only affects
/// whether a trailing incomplete tag fragment is hidden (see [`Stage`]).
pub fn strip_reasoning(text: &str, stage: Stage) -> Cow<'_, str> {
    let hide_partial_trailing = stage == Stage::Streaming && trailing_partial_tag_start(text).is_some();

    // Fast path: no think markup at all (and, mid-stream, no partial trailing
    // tag) → borrow the input untouched. This is the overwhelming common case.
    if !text.contains("<think") && !text.contains("</think") && !hide_partial_trailing {
        return Cow::Borrowed(text);
    }

    // 1. Remove every complete block. MUST run first — a later step would
    //    otherwise eat legitimate text that precedes a properly-closed block.
    let mut s = RE_COMPLETE_BLOCK.replace_all(text, "").into_owned();

    // 2. MiniMax orphan close (opening tag omitted): drop everything up to &
    //    including the first surviving close tag.
    s = RE_HEAD_ORPHAN_CLOSE.replace(&s, "").into_owned();

    // 3. Unclosed opening tag (reasoning the turn left dangling / still
    //    streaming): hide from the tag to the end. A later delta brings the
    //    close and step 1 takes over; at Final it drops the dangling reasoning.
    if let Some(start) = RE_OPEN_TAG.find(&s).map(|m| m.start()) {
        s.truncate(start);
    }

    // 4. Trailing incomplete tag fragment — mid-stream only (at Final it is
    //    literal content and must survive, e.g. a message ending in "<").
    if stage == Stage::Streaming
        && let Some(cut) = trailing_partial_tag_start(&s)
    {
        s.truncate(cut);
    }

    // 5. Any remaining orphan close tags (text concatenated across tool calls):
    //    drop just the tag, keep the surrounding text.
    if RE_CLOSE_TAG.is_match(&s) {
        s = RE_CLOSE_TAG.replace_all(&s, "").into_owned();
    }

    // 6. Collapse blank lines left behind, then trim the card edges.
    let collapsed = RE_EXCESS_NEWLINES.replace_all(&s, "\n\n");
    Cow::Owned(collapsed.trim().to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_tags_is_borrowed_and_unchanged() {
        for stage in [Stage::Streaming, Stage::Final] {
            let out = strip_reasoning("plain **md** text", stage);
            assert!(matches!(out, Cow::Borrowed(_)), "untagged text must be borrowed, not allocated");
            assert_eq!(out, "plain **md** text");
        }
    }

    #[test]
    fn empty_string() {
        assert_eq!(strip_reasoning("", Stage::Final), "");
        assert_eq!(strip_reasoning("", Stage::Streaming), "");
    }

    #[test]
    fn complete_block_removed() {
        assert_eq!(strip_reasoning("Before<think>internal</think>After", Stage::Final), "BeforeAfter");
        assert_eq!(strip_reasoning("<thinking>deep</thinking>Answer", Stage::Final), "Answer");
        assert_eq!(
            strip_reasoning("Start\n<think>\nline 1\nline 2\n</think>\nEnd", Stage::Final),
            "Start\n\nEnd"
        );
    }

    #[test]
    fn multiple_blocks_removed() {
        assert_eq!(
            strip_reasoning("<think>a</think>mid<thinking>b</thinking>end", Stage::Final),
            "midend"
        );
    }

    #[test]
    fn unclosed_open_tag_hides_tail_in_both_stages() {
        // A complete-but-unclosed open tag is dangling reasoning in either stage.
        for stage in [Stage::Streaming, Stage::Final] {
            assert_eq!(strip_reasoning("Answer<think>partial reasoning", stage), "Answer");
            assert_eq!(strip_reasoning("<think>all of it", stage), "");
        }
    }

    #[test]
    fn minimax_orphan_close_drops_head() {
        assert_eq!(strip_reasoning("reasoning\n</think>\nanswer", Stage::Final), "answer");
    }

    #[test]
    fn multiple_orphan_closes() {
        // First close drops the head; remaining lone closes drop only the tag.
        assert_eq!(strip_reasoning("a</think>b</think>c", Stage::Final), "bc");
    }

    #[test]
    fn complete_block_then_trailing_orphan_close() {
        assert_eq!(strip_reasoning("<think>a</think>text</think>more", Stage::Final), "more");
    }

    #[test]
    fn complete_block_then_unclosed_open() {
        assert_eq!(strip_reasoning("<think>a</think>visible<think>going", Stage::Final), "visible");
    }

    #[test]
    fn excess_newlines_collapsed_and_trimmed() {
        assert_eq!(strip_reasoning("<think>a</think>\n\n\n\nanswer", Stage::Final), "answer");
    }

    #[test]
    fn trailing_partial_tag_hidden_only_while_streaming() {
        // Mid-stream: hide the incomplete fragment (it may complete into a tag).
        assert_eq!(strip_reasoning("Answer <thi", Stage::Streaming), "Answer");
        assert_eq!(strip_reasoning("Answer </thin", Stage::Streaming), "Answer");
        // Final: the fragment is literal content — it MUST survive. This is the
        // regression the adversarial review caught: a real answer ending on a
        // bare "<" (or other tag-prefix) must not lose its trailing char(s).
        assert_eq!(strip_reasoning("the less-than symbol is <", Stage::Final), "the less-than symbol is <");
        assert_eq!(strip_reasoning("Answer <thi", Stage::Final), "Answer <thi");
        // A non-think `<…>` fragment (or word) is never touched in either stage.
        assert_eq!(strip_reasoning("a <thinker> b", Stage::Streaming), "a <thinker> b");
        assert_eq!(strip_reasoning("a <thinker> b", Stage::Final), "a <thinker> b");
    }

    #[test]
    fn cjk_tail_does_not_panic() {
        // The trailing-fragment byte window can split a CJK char; must not panic
        // and must leave the (untagged) content intact.
        let input = "这是一个中文回答结尾";
        assert_eq!(strip_reasoning(input, Stage::Streaming), input);
        assert_eq!(strip_reasoning(input, Stage::Final), input);
    }

    #[test]
    fn progressive_accumulation_only_reveals_final_answer() {
        // Simulate the relay re-running the filter over a growing buffer
        // mid-stream, then the terminal render.
        assert_eq!(strip_reasoning("<thi", Stage::Streaming), "");
        assert_eq!(strip_reasoning("<think>sec", Stage::Streaming), "");
        assert_eq!(strip_reasoning("<think>sec</think>ok", Stage::Final), "ok");
    }
}
