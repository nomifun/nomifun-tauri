//! The no-LLM stall detector. Two entry points:
//!  * `signal_from_agent_error` — maps an agent error payload to a signal
//!    (conversation path), plus `map_agent_event` for the full event stream.
//!  * `TerminalDetector` — feeds raw PTY bytes through the shared
//!    `AnsiLineScanner` and classifies completed lines via built-in pattern
//!    sets (provider-error signatures / decision prompts / recommended option).
//!
//! Self-echo guard: injected wake/answer text is tagged by the probe with a
//! zero-width marker prefix; lines bearing it are skipped so an injection's own
//! echo cannot be re-detected as a fresh stall.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use nomifun_api_types::{AgentErrorOwnership, AgentStreamErrorData};
use nomifun_terminal::AnsiLineScanner;

use crate::signal::{DecisionKind, DecisionPrompt, DecisionSource, SessionSignal};

/// Map an agent error payload to a signal.
pub fn signal_from_agent_error(d: &AgentStreamErrorData) -> SessionSignal {
    let is_provider = d.ownership == Some(AgentErrorOwnership::UserLlmProvider)
        || d.code.map(crate::config::is_provider_fault).unwrap_or(false);
    if is_provider {
        SessionSignal::ProviderError {
            code: d.code,
            retryable: d.retryable,
            message: d.message.clone(),
        }
    } else {
        SessionSignal::AgentError {
            retryable: d.retryable,
            message: d.message.clone(),
        }
    }
}

/// Built-in provider-error line signatures (lowercased contains-match).
const PROVIDER_ERROR_SIGS: &[&str] = &[
    "http 404",
    "http 424",
    "http 429",
    "http 500",
    "http 502",
    "http 503",
    "http 529",
    "status 500",
    "status 502",
    "status 503",
    "rate limit",
    "rate_limit",
    "overloaded",
    "request timed out",
    "request timeout",
    "gateway timeout",
    "connection refused",
    "invalid api key",
    "invalid_api_key",
    "econnreset",
    "socket hang up",
    "internal server error",
    "bad gateway",
    "service unavailable",
    "upstream error",
];

/// Decision-prompt trailing markers.
const YES_NO_MARKERS: &[&str] = &["(y/n)", "(yes/no)", "[y/n]", "[yes/no]", "y/n?"];

const PROCEED_MARKERS: &[&str] = &[
    "do you want to proceed",
    "do you want to continue",
    "press enter to continue",
    "continue? (",
    "proceed? (",
    "are you sure",
    "confirm? (",
    "[y/n]",
];

/// Returns true if a (lowercased) line looks like a recommended/default option.
fn recommended_marker(line: &str) -> bool {
    let low = line.to_lowercase();
    line.contains('\u{276f}') // ❯
        || line.contains('▶')
        || low.contains("(default)")
        || low.contains("(recommended)")
        || low.contains("[default]")
        || line.contains("（推荐）")
        || line.contains("（默认）")
        || line.contains("(推荐)")
        || line.contains("(默认)")
        || line.contains("[推荐]")
        || line.contains("[默认]")
        || low.trim_start().starts_with("> ")
}

/// Parse a single ANSI-stripped line plus recent context into a decision prompt.
/// `recent` is the bounded scrollback (oldest→newest), used to gather the
/// numbered options preceding a trailing prompt and to find a recommended line.
fn detect_decision(line: &str, recent: &VecDeque<String>) -> Option<DecisionPrompt> {
    let low = line.to_lowercase();
    let trimmed = low.trim_end();

    let is_yes_no = YES_NO_MARKERS
        .iter()
        .any(|m| trimmed.ends_with(m) || trimmed.contains(m));
    let is_proceed = PROCEED_MARKERS.iter().any(|m| low.contains(m));
    let is_menu_line = is_numbered_option(line);
    let has_recent_options = recent.iter().any(|l| is_numbered_option(l));
    // A question (or an inline numeric-choice token), especially when a numbered
    // menu preceded it, is a decision prompt awaiting selection.
    let is_question = (trimmed.contains('?') && (has_numeric_choice(trimmed) || has_recent_options))
        || (has_numeric_choice(trimmed) && has_recent_options);

    if !is_yes_no && !is_proceed && !is_menu_line && !is_question {
        return None;
    }

    // Gather contiguous numbered options from recent lines (+ this one).
    let mut options: Vec<String> = recent.iter().filter(|l| is_numbered_option(l)).cloned().collect();
    if is_menu_line {
        options.push(line.to_string());
    }
    options.dedup();

    // Recommended: a recent (or current) line carrying a recommended marker.
    let recommended = recent
        .iter()
        .rev()
        .chain(std::iter::once(&line.to_string()))
        .find(|l| recommended_marker(l))
        .map(|l| clean_option(l));

    Some(DecisionPrompt {
        text: line.trim().to_string(),
        options: options.iter().map(|o| clean_option(o)).collect(),
        recommended,
        source: DecisionSource::TerminalScan,
        kind: DecisionKind::Options,
        permission: None,
    })
}

/// Detects an inline numeric-choice token like `(1/2)`, `[1-3]`, `(1/2/3)`, and
/// the fullwidth-bracket forms Chinese output uses — `（1/2）`, `［1-3］`. Scans
/// by `char` (not bytes) so multi-byte fullwidth brackets/digits/separators are
/// recognized; an ASCII-only byte scan silently skipped every fullwidth menu.
fn has_numeric_choice(line: &str) -> bool {
    let chars: Vec<char> = line.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let open = chars[i];
        if open == '(' || open == '[' || open == '（' || open == '［' {
            // Scan until the matching close, requiring a digit and a separator.
            let mut j = i + 1;
            let mut saw_digit = false;
            let mut saw_sep = false;
            while j < chars.len() {
                let c = chars[j];
                if matches!(c, ')' | ']' | '）' | '］') {
                    break;
                }
                match c {
                    '0'..='9' | '０'..='９' => saw_digit = true,
                    '/' | '-' | ',' | '／' | '、' | '，' => saw_sep = true,
                    ' ' | '\u{3000}' => {}
                    _ => {
                        saw_digit = false;
                        break;
                    }
                }
                j += 1;
            }
            if saw_digit && saw_sep {
                return true;
            }
        }
        i += 1;
    }
    false
}

/// A numbered menu option like `1) yes`, `2. no`, `❯ 1) yes`, plus the Chinese
/// enumeration forms `1、是` (顿号) and the fullwidth `1）是` / `1．是`. Chinese
/// LLM output overwhelmingly uses `1、` and fullwidth punctuation, so an
/// ASCII-`.`/`)`-only check scored those menus as zero options and IDMM never
/// saw the decision. The selection-intent guard in `detect_chat_decision` still
/// prevents a plain `1、…` step list from being treated as a menu.
fn is_numbered_option(line: &str) -> bool {
    let s = line
        .trim_start_matches(['\u{276f}', '▶', '>', ' ', '\t'])
        .trim_start();
    let mut chars = s.chars();
    match chars.next() {
        Some(c) if c.is_ascii_digit() || ('０'..='９').contains(&c) => {
            matches!(chars.next(), Some('.') | Some(')') | Some('、') | Some('）') | Some('．'))
        }
        _ => false,
    }
}

/// Strip leading selection markers/whitespace from an option line.
fn clean_option(line: &str) -> String {
    line.trim_start_matches(['\u{276f}', '▶', '>', ' ', '\t'])
        .trim()
        .to_string()
}

/// Explicit "reply with the option number" phrasing — the strongest signal that
/// the agent ended its turn waiting for the user to pick a numbered option.
fn has_reply_number_phrase(low: &str) -> bool {
    const SIGS: &[&str] = &[
        "回复编号",
        "回复对应",
        "回复数字",
        "回复序号",
        "回复选项",
        "输入编号",
        "选择编号",
        "告诉我编号",
        "reply with the number",
        "reply with a number",
        "reply with the option",
        "respond with the number",
    ];
    SIGS.iter().any(|s| low.contains(s))
}

/// Selection-intent wording: the agent is asking the user to CHOOSE among
/// options (vs. listing steps or announcing its own pick). Kept specific to
/// avoid matching "我选择了…/I'll use…" (the agent stating its decision).
fn has_select_word(low: &str) -> bool {
    const SIGS: &[&str] = &[
        "请选择",
        "选择哪",
        "选哪",
        "哪一个",
        "哪个方案",
        "哪个选项",
        "你想选",
        "你想用哪",
        "你倾向",
        "你更倾向",
        "你希望用哪",
        "你希望选",
        "你的选择",
        "which option",
        "which approach",
        "which one",
        "which do you",
        "please choose",
        "please select",
        "choose an option",
        "select an option",
        "let me know which",
        "do you prefer",
        "your choice",
        "your preference",
    ];
    SIGS.iter().any(|s| low.contains(s))
}

/// Detect a chat-style decision prompt in an assistant turn's full text.
///
/// Conservative by design — requires BOTH discrete choices (≥2 numbered options
/// or an inline `(1/2/3)` token) AND an intent to have the user choose (an
/// explicit "回复编号"/"reply with the number" phrase, OR a selection word paired
/// with a question / inline token / numbered menu). Plain numbered
/// implementation steps, prose with no options, a single option, and the agent
/// announcing its own pick all return `None` (false-positive guards).
///
/// Used by `ConversationProbe` on turn-end for PLAIN DESKTOP conversations only:
/// channel/companion conversations route such menus to a remote human via the
/// channel `PendingDecisionStore` and must NOT be auto-answered by IDMM.
pub fn detect_chat_decision(text: &str) -> Option<DecisionPrompt> {
    let options: Vec<String> = text
        .lines()
        .filter(|l| is_numbered_option(l))
        .map(clean_option)
        .collect();
    let low = text.to_lowercase();
    let inline_token = has_numeric_choice(&low);
    let has_question = low.contains('?') || low.contains('？');

    let has_menu = options.len() >= 2 || inline_token;
    let has_intent = has_reply_number_phrase(&low)
        || (has_select_word(&low) && (has_question || inline_token || options.len() >= 2));

    if !(has_menu && has_intent) {
        return None;
    }

    let recommended = text.lines().rev().find(|l| recommended_marker(l)).map(clean_option);
    let prompt_line = text
        .lines()
        .rev()
        .map(|l| l.trim())
        .find(|l| !l.is_empty())
        .unwrap_or("")
        .to_string();

    Some(DecisionPrompt {
        text: prompt_line,
        options,
        recommended,
        source: DecisionSource::TextScan,
        kind: DecisionKind::Options,
        permission: None,
    })
}

/// Detect an open-ended question (纯问答, D6) in an assistant turn's full text:
/// the turn ends on an interrogative but has NO enumerable options (so
/// [`detect_chat_decision`] would return `None`). Returns a `DecisionPrompt`
/// with `kind = OpenQuestion`, empty `options`, and no `permission` — the
/// decision watch's model tier answers it with free text; the rule tier never
/// guesses an open answer (spec §5.4).
///
/// Conservative by design: requires an interrogative cue (`?`/`？` or a
/// question/select intent word) AND the absence of an INLINE discrete-choice
/// token (`(1/2)` / `（1/2）`). Plain prose with no question, a genuine pick-one
/// numbered menu (that's an `Options` decision — already caught by
/// [`detect_chat_decision`] above), and the agent announcing its own next step
/// all return `None`. A multi-part question prompt whose numbered lines are
/// TOPICS rather than mutually-exclusive options IS an open question (the model
/// answers all parts in free text). The caller gates on `work_in_progress` (only
/// an open question DURING an unfinished turn is a stall worth answering).
pub fn detect_chat_open_question(text: &str) -> Option<DecisionPrompt> {
    // If it parses as a discrete-options decision, it is NOT an open question.
    if detect_chat_decision(text).is_some() {
        return None;
    }
    let low = text.to_lowercase();
    // An INLINE discrete-choice token like `(1/2)` / `（1/2）` is an unambiguous
    // pick-one menu marker → not an open question.
    //
    // Do NOT additionally disqualify on the COUNT of numbered lines. A multi-part
    // design prompt — "先问你几个基础设计问题：1. 技术栈偏好… 2. 界面风格… 请告诉我你的
    // 偏好。" — has several numbered TOPICS (each its own question), NOT mutually-
    // exclusive options. The old `numbered_lines >= 2` guard mis-read those as a
    // menu, so the turn matched NEITHER detector: `detect_chat_decision` declined
    // it (no pick-one selection intent) and this returned `None` on the count, so
    // IDMM never saw the pending question and stayed silent (会话 27「中途开启
    // 智能决策不生效」, no decision record at all). A genuine pick-one numbered menu
    // is already excluded by the `detect_chat_decision` check above (it carries the
    // selection intent this multi-question prompt lacks).
    if has_numeric_choice(&low) {
        return None;
    }
    let has_question = low.contains('?') || low.contains('？');
    // An interrogative cue: an explicit question mark, OR an asking-intent word
    // (covers "你希望…", "需要我…", "should I…" phrasings that may omit the mark).
    if !(has_question || has_open_intent(&low)) {
        return None;
    }
    // The trailing non-empty line is the question prompt.
    let prompt_line = text
        .lines()
        .rev()
        .map(|l| l.trim())
        .find(|l| !l.is_empty())
        .unwrap_or("")
        .to_string();
    if prompt_line.is_empty() {
        return None;
    }
    Some(DecisionPrompt {
        text: prompt_line,
        options: vec![],
        recommended: None,
        source: DecisionSource::TextScan,
        kind: DecisionKind::OpenQuestion,
        permission: None,
    })
}

/// Asking-intent wording for an OPEN question (no enumerable options): the agent
/// is asking the user something, not announcing its own plan. Kept specific to
/// avoid matching "我先去实现…/I'll now…" (the agent stating its next step).
///
/// `pub(crate)` so the terminal turn-end helper (`probe.rs`) can reuse the exact
/// same open-intent word set when gating on whether the TRAILING content line is
/// interrogative — one source of truth (a mark-less "你希望…" trailing line must
/// gate the same way the chat detector treats it).
pub(crate) fn has_open_intent(low: &str) -> bool {
    const SIGS: &[&str] = &[
        "请问",
        "你希望",
        "你想要",
        "你想让",
        "你打算",
        "需要我",
        "要我",
        "你倾向",
        "你觉得",
        "你能否告诉",
        "能否告诉我",
        "告诉我你",
        "想确认一下",
        "想跟你确认",
        "what would you like",
        "what do you want",
        "how would you like",
        "should i",
        "do you want me to",
        "could you tell me",
        "can you tell me",
        "let me know what",
        "what should",
    ];
    SIGS.iter().any(|s| low.contains(s))
}

/// Terminal byte → signal scanner. Holds a bounded scrollback for context.
pub struct TerminalDetector {
    scanner: AnsiLineScanner,
    recent: VecDeque<String>,
    /// Text IDMM recently injected into this PTY, shared with `TerminalProbe`.
    /// A completed output line equal to a pending entry is the echo of our own
    /// injection (the CLI echoing the keystrokes we sent) — skip it and pop the
    /// entry so it cannot be re-detected as a fresh stall. Replaces the old
    /// zero-width-tag scheme, which corrupted the bytes the CLI actually read.
    recent_injections: Arc<Mutex<VecDeque<String>>>,
}

impl Default for TerminalDetector {
    fn default() -> Self {
        Self::new()
    }
}

impl TerminalDetector {
    const MAX_RECENT: usize = 400;

    pub fn new() -> Self {
        Self {
            scanner: AnsiLineScanner::new(),
            recent: VecDeque::new(),
            recent_injections: Arc::new(Mutex::new(VecDeque::new())),
        }
    }

    /// Construct sharing a `recent_injections` queue with the probe, so lines
    /// echoing IDMM's own injected answers/nudges are skipped.
    pub fn with_echo_guard(recent_injections: Arc<Mutex<VecDeque<String>>>) -> Self {
        Self {
            scanner: AnsiLineScanner::new(),
            recent: VecDeque::new(),
            recent_injections,
        }
    }

    /// Whether a completed line is the echo of a recently-injected answer/nudge.
    /// Pops the matched entry so each injection only suppresses one echo line.
    fn is_injection_echo(&self, line: &str) -> bool {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            return false;
        }
        let Ok(mut pending) = self.recent_injections.lock() else {
            return false;
        };
        if let Some(pos) = pending.iter().position(|e| e == trimmed) {
            pending.remove(pos);
            true
        } else {
            false
        }
    }

    /// Feed a raw PTY chunk; return signals derived from completed lines.
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<SessionSignal> {
        let mut out = Vec::new();
        for line in self.scanner.feed(bytes) {
            // Self-echo guard: skip lines that echo our own injection.
            if self.is_injection_echo(&line) {
                self.push_recent(line);
                continue;
            }
            let low = line.to_lowercase();
            if PROVIDER_ERROR_SIGS.iter().any(|s| low.contains(s)) {
                out.push(SessionSignal::ProviderError {
                    code: None,
                    retryable: None,
                    message: line.clone(),
                });
            } else if let Some(dp) = detect_decision(&line, &self.recent) {
                out.push(SessionSignal::Decision(dp));
            }
            self.push_recent(line);
        }
        out
    }

    fn push_recent(&mut self, line: String) {
        if self.recent.len() >= Self::MAX_RECENT {
            self.recent.pop_front();
        }
        self.recent.push_back(line);
    }

    /// The recent scrollback joined newest-last, truncated to `max_chars` from
    /// the tail (keeps the most recent output for sidecar context).
    pub fn scrollback(&self, max_chars: usize) -> String {
        let joined = self.recent.iter().cloned().collect::<Vec<_>>().join("\n");
        crate::util::tail_chars(&joined, max_chars)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nomifun_api_types::{AgentErrorCode, AgentErrorOwnership};

    fn err(code: AgentErrorCode, ownership: AgentErrorOwnership) -> AgentStreamErrorData {
        AgentStreamErrorData::classified("boom", code, ownership, None, true, false, None)
    }

    #[test]
    fn agent_error_provider_vs_other() {
        let p = signal_from_agent_error(&err(
            AgentErrorCode::UserLlmProviderGatewayError,
            AgentErrorOwnership::UserLlmProvider,
        ));
        assert!(matches!(p, SessionSignal::ProviderError { .. }));

        let a = signal_from_agent_error(&err(
            AgentErrorCode::UserAgentNotInstalled,
            AgentErrorOwnership::UserAgent,
        ));
        assert!(matches!(a, SessionSignal::AgentError { .. }));
    }

    #[test]
    fn provider_500_line_classified() {
        let mut d = TerminalDetector::new();
        let sigs = d.feed(b"Error: HTTP 500 Internal Server Error from provider\n");
        assert_eq!(sigs.len(), 1);
        assert!(matches!(sigs[0], SessionSignal::ProviderError { .. }));
    }

    #[test]
    fn rate_limit_and_424_lines_classified() {
        let mut d = TerminalDetector::new();
        assert!(matches!(
            d.feed(b"429 rate limit exceeded\n")[0],
            SessionSignal::ProviderError { .. }
        ));
        let mut d2 = TerminalDetector::new();
        assert!(matches!(
            d2.feed(b"received HTTP 424 from upstream\n")[0],
            SessionSignal::ProviderError { .. }
        ));
    }

    #[test]
    fn yes_no_prompt_detected() {
        let mut d = TerminalDetector::new();
        let sigs = d.feed(b"Do you want to proceed? (y/n)\n");
        assert_eq!(sigs.len(), 1);
        assert!(matches!(sigs[0], SessionSignal::Decision(_)));
    }

    #[test]
    fn numbered_menu_parsed_with_recommended() {
        let mut d = TerminalDetector::new();
        // Options arrive, then a trailing prompt; the ❯ marks the recommended one.
        d.feed(b"Select an option:\n");
        d.feed("\u{276f} 1) yes\n".as_bytes());
        d.feed(b"  2) no\n");
        let sigs = d.feed(b"Your choice? (1/2)\n");
        let decision = sigs
            .iter()
            .find_map(|s| match s {
                SessionSignal::Decision(dp) => Some(dp.clone()),
                _ => None,
            })
            .expect("a decision signal");
        assert!(decision.options.iter().any(|o| o.contains("1) yes")));
        assert!(decision.options.iter().any(|o| o.contains("2) no")));
        assert_eq!(decision.recommended.as_deref(), Some("1) yes"));
    }

    #[test]
    fn plain_output_no_signal() {
        let mut d = TerminalDetector::new();
        assert!(d.feed(b"compiling module foo\nok\n").is_empty());
    }

    #[test]
    fn self_echo_guard_skips_injected_lines() {
        let recent = Arc::new(Mutex::new(VecDeque::new()));
        recent.lock().unwrap().push_back("do you want to proceed? (y/n)".to_string());
        let mut d = TerminalDetector::with_echo_guard(recent);
        // The echoed injection (equal to a pending entry) is skipped, not detected.
        assert!(d.feed(b"do you want to proceed? (y/n)\n").is_empty());
        // The entry was consumed, so a genuine later prompt IS detected.
        let sigs = d.feed(b"do you want to proceed? (y/n)\n");
        assert_eq!(sigs.len(), 1);
        assert!(matches!(sigs[0], SessionSignal::Decision(_)));
    }

    #[test]
    fn scrollback_truncates_to_tail() {
        let mut d = TerminalDetector::new();
        d.feed(b"line-aaaa\nline-bbbb\nline-cccc\n");
        let tail = d.scrollback(9);
        assert_eq!(tail.len(), 9);
        assert!(tail.ends_with("cccc"));
    }

    // ── detect_chat_decision: prose/markdown decision prompts in chat turns ──

    #[test]
    fn chat_decision_numbered_with_reply_number_phrase() {
        // The canonical "方案 1/2/3、请回复编号" desktop decision.
        let text = "我设计了两套方案：\n\
                    1) Canvas 渲染：性能好，开发量大\n\
                    2) DOM + CSS：开发快，性能一般\n\
                    请回复编号告诉我你的选择。";
        let dp = detect_chat_decision(text).expect("a chat decision");
        assert_eq!(dp.source, DecisionSource::TextScan);
        assert!(dp.options.iter().any(|o| o.contains("Canvas")));
        assert!(dp.options.iter().any(|o| o.contains("DOM")));
        assert_eq!(dp.options.len(), 2);
    }

    #[test]
    fn chat_decision_question_plus_select_word() {
        let text = "1. 用 React\n2. 用原生 JS\n你想用哪个？";
        let dp = detect_chat_decision(text).expect("a chat decision");
        assert_eq!(dp.options.len(), 2);
    }

    #[test]
    fn chat_decision_inline_numeric_token_with_select_word() {
        // No newline-numbered options, but an inline (1/2/3) token + 请选择.
        let text = "请选择构建方式 (1/2/3)。";
        assert!(detect_chat_decision(text).is_some());
    }

    #[test]
    fn chat_decision_recommended_marker_chinese() {
        let text = "1) 方案A（推荐）：稳妥\n2) 方案B：激进\n请选择哪个？";
        let dp = detect_chat_decision(text).expect("a chat decision");
        assert!(
            dp.recommended.as_deref().unwrap_or("").contains("方案A"),
            "recommended should be the marked option A; got {:?}",
            dp.recommended
        );
    }

    #[test]
    fn chat_decision_english_which_option() {
        let text = "1) Server-side render\n2) Client-side render\nWhich option do you prefer?";
        assert!(detect_chat_decision(text).is_some());
    }

    // ── false-positive guards (must return None) ──

    #[test]
    fn chat_decision_plain_numbered_steps_is_none() {
        // An implementation plan with numbered steps is NOT a decision: no
        // selection intent. This is the highest-risk false positive.
        let text = "实现步骤：\n1) 初始化画布\n2) 渲染西瓜\n3) 处理切割手势\n我现在开始实现。";
        assert!(
            detect_chat_decision(text).is_none(),
            "numbered implementation steps must not be a decision"
        );
    }

    #[test]
    fn chat_decision_prose_no_options_is_none() {
        assert!(detect_chat_decision("好的，我先去实现这个功能。").is_none());
    }

    #[test]
    fn chat_decision_agent_announcing_its_own_choice_is_none() {
        // Agent stating what it picked (no question, no reply-number phrase, no
        // select cue) must not be hijacked.
        let text = "我会用方案 1) Canvas 来实现，因为性能更好。现在开始。";
        assert!(detect_chat_decision(text).is_none());
    }

    #[test]
    fn chat_decision_single_option_question_is_none() {
        // A single numbered line + a question is too weak to be a menu.
        let text = "1) 继续\n要继续吗？";
        assert!(detect_chat_decision(text).is_none());
    }

    // ── detect_chat_open_question: D6 纯问答(interrogative, no options) ──

    #[test]
    fn open_question_interrogative_no_options_detected() {
        // An open-ended question with no enumerable options → OpenQuestion.
        let text = "我看了下你的需求。你希望这个导出功能支持哪些文件格式？";
        let dp = detect_chat_open_question(text).expect("an open question");
        assert_eq!(dp.kind, DecisionKind::OpenQuestion);
        assert!(dp.options.is_empty());
        assert!(dp.permission.is_none());
        assert_eq!(dp.source, DecisionSource::TextScan);
    }

    #[test]
    fn open_question_english_should_i_detected() {
        let text = "I finished the migration script. What naming convention should I use for the new columns?";
        assert!(detect_chat_open_question(text).is_some());
    }

    #[test]
    fn open_question_with_options_is_none() {
        // A numbered menu is an Options decision, NOT an open question.
        let text = "1) 用 React\n2) 用原生 JS\n你想用哪个？";
        assert!(
            detect_chat_open_question(text).is_none(),
            "an enumerable-options decision must not be classified as an open question"
        );
        // And it IS a normal options decision.
        assert!(detect_chat_decision(text).is_some());
    }

    #[test]
    fn open_question_non_question_prose_is_none() {
        // The agent announcing its own next step is not an open question.
        assert!(detect_chat_open_question("好的，我先去实现这个功能。").is_none());
        assert!(detect_chat_open_question("我会用 Canvas 来实现，现在开始。").is_none());
    }

    #[test]
    fn open_question_multi_part_design_prompt_is_open_question() {
        // REGRESSION (会话 27「中途开启智能决策不生效 / 完全没有决策记录」): a multi-part
        // design questionnaire — several NUMBERED TOPICS (each a question, some with
        // suggested bullet sub-options) ending on an open-intent statement — is an
        // OPEN QUESTION the model tier should answer in free text. It is NOT a
        // pick-one menu: there is no "回复编号"/选择 intent, so detect_chat_decision
        // declines it. The old `numbered_lines >= 2` guard here mis-read the topic
        // numbers as a menu and returned None, so the turn matched NEITHER detector
        // and IDMM stayed silent. It must now classify as OpenQuestion.
        let text = "好的！我会一步步和你确认设计，然后写出贪吃蛇游戏。\n\n\
                    先问你几个基础设计问题，我们再往下细化：\n\n\
                    1. **技术栈偏好**：你想用什么来写？\n\
                    \u{20}  - 推荐：**HTML5 + JavaScript**\n\
                    \u{20}  - 或 **Python + Pygame**\n\n\
                    2. **界面风格**：\n\
                    \u{20}  - 复古像素风\n\
                    \u{20}  - 现代简约风\n\n\
                    3. **核心规则**：\n\
                    \u{20}  - 撞墙死，还是穿墙继续？\n\
                    \u{20}  - 是否显示分数和最高分？\n\n\
                    请告诉我你的偏好，我们一个一个敲定，然后我再开始写代码。";
        assert!(
            detect_chat_decision(text).is_none(),
            "a multi-question design prompt has no pick-one selection intent → not an Options decision"
        );
        let dp = detect_chat_open_question(text).expect("a multi-part design prompt is an open question");
        assert_eq!(dp.kind, DecisionKind::OpenQuestion);
        assert!(dp.options.is_empty(), "an open question carries no enumerable options");
        assert!(dp.permission.is_none());
        assert_eq!(dp.source, DecisionSource::TextScan);
    }

    #[test]
    fn open_question_multi_part_without_question_mark_via_open_intent() {
        // The same shape but with NO `？` anywhere — the trailing "请告诉我你…" /
        // "你希望…" open-intent cue must still classify it as an open question
        // (numbered topics must not disqualify it).
        let text = "我们先定几个方向：\n\
                    1. 配色\n\
                    2. 字体\n\
                    3. 布局\n\
                    告诉我你的偏好，我再继续。";
        assert!(detect_chat_decision(text).is_none());
        assert!(
            detect_chat_open_question(text).is_some(),
            "numbered TOPICS + an open-intent trailing line is an open question, not a menu"
        );
    }

    // ── Chinese-convention menu formats (REGRESSION GUARD) ──
    // Chinese LLM output overwhelmingly uses the enumeration comma "1、" and
    // fullwidth punctuation ("1）", "（1/2）") rather than the ASCII "1." / "1)" /
    // "(1/2)" the detector was originally written for. These turns are real
    // "选择项" the user sees, but the strict ASCII-only detector scored them as
    // zero options → SessionSignal::Done → IDMM never intervened. This is the
    // "选择项出现了但不介入" gap for Chinese desktop chats.

    #[test]
    fn chat_decision_chinese_dunhao_numbered_menu() {
        // 顿号编号 "1、" + "请回复编号"(no question mark) — the canonical Chinese
        // numbered menu. Must be an Options decision with both options parsed.
        let text = "我们先确定渲染方案：\n\
                    1、Canvas 渲染\n\
                    2、DOM + CSS\n\
                    请回复编号告诉我你的选择。";
        let dp = detect_chat_decision(text).expect("a 顿号-separated chat decision");
        assert_eq!(dp.options.len(), 2, "顿号 '1、/2、' lines must count as numbered options");
        assert!(dp.options.iter().any(|o| o.contains("Canvas")));
        assert!(dp.options.iter().any(|o| o.contains("DOM")));
    }

    #[test]
    fn chat_decision_fullwidth_paren_numbered_menu() {
        // 全角右括号编号 "1）".
        let text = "1）用 React\n2）用原生 JS\n你想用哪个？";
        let dp = detect_chat_decision(text).expect("a fullwidth-paren chat decision");
        assert_eq!(dp.options.len(), 2);
    }

    #[test]
    fn chat_decision_fullwidth_inline_token() {
        // 全角括号内联选项 "（1/2）" + 请选择 — has_numeric_choice must accept the
        // fullwidth bracket.
        let text = "请选择构建方式（1/2）。";
        assert!(
            detect_chat_decision(text).is_some(),
            "fullwidth （1/2） inline token must be recognized as a menu"
        );
    }

    #[test]
    fn chat_decision_dunhao_steps_without_intent_is_none() {
        // FALSE-POSITIVE GUARD: 顿号 enumeration is now a numbered option, but a
        // plain step list with NO selection intent must still NOT be a decision.
        let text = "实现步骤：\n1、初始化画布\n2、渲染蛇身\n3、处理键盘\n我现在开始实现。";
        assert!(
            detect_chat_decision(text).is_none(),
            "顿号 step list with no selection intent must not be a decision"
        );
    }
}
