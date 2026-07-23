//! 重复挖矿器（确定性，无 LLM）。
//!
//! 从采集的工具调用事件里挖出"做过多次的多步套路"。只看**工具名序列**（绝不看参数值，
//! 秘密永不入签名），按对话分组 → 折叠连续重复 → 滑窗聚合 → 跨对话计 distinct →
//! 极大窗去重（长窗优先，丢掉被更长且覆盖度不低的窗包含的短窗）。100% 单元可测。

use std::collections::{BTreeMap, BTreeSet};

use nomifun_common::ConversationId;

use crate::collector::CollectedEvent;
use crate::evolution::transcript::TranscriptAnchor;

/// 窗口前后各保留的轮数(给 drafter 上下文);随锚带给重水合层。
const ANCHOR_PAD_TURNS: usize = 2;

/// 一个被挖出的、值得固化为技能的多步套路。
#[derive(Debug, Clone, PartialEq)]
pub struct MinedPattern {
    /// 稳定签名（工具名序列），= [`tool_call_signature`]。
    pub signature: String,
    /// 归一化工具名序列（多步套路的步骤）。
    pub steps: Vec<String>,
    /// 跨所有会话的总出现次数。
    pub count: i64,
    /// 出现该套路的不同会话数。
    pub distinct_sessions: usize,
    /// 几个代表性 event_id（用于技能溯源 provenance）。
    pub example_event_ids: Vec<String>,
    /// 一个代表性实例的重水合定位锚（会话 + 时间窗 + call_ids）。空 conversation_id
    /// = 无法重水合（drafter 降级回工具名步骤）。
    pub anchor: TranscriptAnchor,
}

/// 多步套路的窗口长度边界：至少 2 步，至多 5 步（更长的多为一次性长链，固化价值低）。
const MIN_STEPS: usize = 2;
const MAX_STEPS: usize = 5;

/// 归一化工具名序列 → 稳定签名。工具名不含 `\u{1f}`（单元分隔符），故 join 即稳定且无碰撞。
pub fn tool_call_signature(steps: &[String]) -> String {
    steps.join("\u{1f}")
}

/// 从工具调用事件挖掘重复多步套路。
///
/// - `events`：oldest-first；只消费 `source == "tool_calls"` 的 `data.{name, conversation_id, call_id}`。
/// - `min_count`：同一签名跨所有会话的总出现次数下限。
/// - `min_distinct_sessions`：出现该签名的不同会话数下限。
///
/// 返回去重后的极大套路，长窗优先。
pub fn mine_patterns(events: &[CollectedEvent], min_count: i64, min_distinct_sessions: usize) -> Vec<MinedPattern> {
    // 1) 按对话分组，保序收集
    // (tool_name, runtime_call_id, durable_event_id, ts)。
    let mut by_conv: BTreeMap<String, Vec<(String, String, String, i64)>> =
        BTreeMap::new();
    for ev in events {
        if ev.source != "tool_calls" {
            continue;
        }
        let name = ev.data.get("name").and_then(|n| n.as_str()).unwrap_or("");
        if name.is_empty() {
            continue;
        }
        let Some(conv) = ev
            .data
            .get("conversation_id")
            .and_then(|c| c.as_str())
            .and_then(|id| ConversationId::try_from(id).ok())
            .map(ConversationId::into_string)
        else {
            continue;
        };
        let call_id = ev.data.get("call_id").and_then(|c| c.as_str()).unwrap_or("").to_owned();
        by_conv
            .entry(conv)
            .or_default()
            .push((name.to_owned(), call_id, ev.event_id.clone(), ev.ts));
    }

    // 2) 每对话：折叠连续重复 → 序列；滑窗 [MIN_STEPS, MAX_STEPS] 聚合签名。
    struct Agg {
        steps: Vec<String>,
        count: i64,
        sessions: BTreeSet<String>,
        examples: Vec<String>,
        /// 首个观察到的实例锚（一个代表性会话窗口）。
        anchor: Option<TranscriptAnchor>,
    }
    let mut agg: BTreeMap<String, Agg> = BTreeMap::new();
    for (conv, calls) in &by_conv {
        // 折叠连续重复（同一工具连刷多次算一步），保留首次的 (call_id, ts)。
        let mut seq: Vec<(String, String, String, i64)> = Vec::new();
        for (name, call_id, event_id, ts) in calls {
            if seq.last().map(|(n, _, _, _)| n == name).unwrap_or(false) {
                continue;
            }
            seq.push((name.clone(), call_id.clone(), event_id.clone(), *ts));
        }
        let names: Vec<String> = seq.iter().map(|(n, _, _, _)| n.clone()).collect();
        let n = names.len();
        if n < MIN_STEPS {
            continue;
        }
        for len in MIN_STEPS..=MAX_STEPS.min(n) {
            for start in 0..=(n - len) {
                let window = &names[start..start + len];
                let sig = tool_call_signature(window);
                let entry = agg.entry(sig).or_insert_with(|| Agg {
                    steps: window.to_vec(),
                    count: 0,
                    sessions: BTreeSet::new(),
                    examples: Vec::new(),
                    anchor: None,
                });
                entry.count += 1;
                entry.sessions.insert(conv.clone());
                // 首个实例 → 锚（代表性会话窗口）。
                if entry.anchor.is_none() {
                    let slice = &seq[start..start + len];
                    entry.anchor = Some(TranscriptAnchor {
                        conversation_id: conv.clone(),
                        start_ts: slice.first().map(|(_, _, _, t)| *t).unwrap_or(0),
                        end_ts: slice.last().map(|(_, _, _, t)| *t).unwrap_or(0),
                        pad_turns: ANCHOR_PAD_TURNS,
                        call_ids: slice
                            .iter()
                            .filter(|(_, call_id, _, _)| !call_id.is_empty())
                            .map(|(_, call_id, _, _)| call_id.clone())
                            .collect(),
                    });
                }
                if entry.examples.len() < 8 {
                    if let Some((_, _, event_id, _)) = seq.get(start) {
                        if !entry.examples.contains(event_id) {
                            entry.examples.push(event_id.clone());
                        }
                    }
                }
            }
        }
    }

    // 3) 阈值过滤。
    let mut survivors: Vec<MinedPattern> = agg
        .into_iter()
        .filter(|(_, a)| a.count >= min_count && a.sessions.len() >= min_distinct_sessions)
        .map(|(sig, a)| MinedPattern {
            signature: sig,
            steps: a.steps,
            count: a.count,
            distinct_sessions: a.sessions.len(),
            example_event_ids: a.examples,
            anchor: a.anchor.unwrap_or_default(),
        })
        .collect();

    // 4) 极大窗去重：长窗优先；丢掉被某个更长且覆盖度 >= 自身的已留签名包含的短窗。
    survivors.sort_by(|x, y| {
        y.steps
            .len()
            .cmp(&x.steps.len())
            .then(y.distinct_sessions.cmp(&x.distinct_sessions))
            .then(x.signature.cmp(&y.signature))
    });
    let mut kept: Vec<MinedPattern> = Vec::new();
    for p in survivors {
        let subsumed = kept.iter().any(|k| {
            k.steps.len() > p.steps.len()
                && k.distinct_sessions >= p.distinct_sessions
                && is_contiguous_subsequence(&p.steps, &k.steps)
        });
        if !subsumed {
            kept.push(p);
        }
    }
    kept
}

/// `needle` 是否为 `haystack` 的连续子序列。
fn is_contiguous_subsequence(needle: &[String], haystack: &[String]) -> bool {
    if needle.is_empty() || needle.len() > haystack.len() {
        return false;
    }
    haystack.windows(needle.len()).any(|w| w == needle)
}

/// 反思候选的会话序列长度上限（签名不无限膨胀）。
const MAX_REFLECT_STEPS: usize = 8;

/// 任务后反思候选（design §5.5）：把"单个会话里一长串多步操作"整体作为一个候选——
/// 即使只出现一次，也在一次复杂任务后反思是否值得固化。每会话至多一条，折叠连续重复后
/// 长度 ≥ `min_steps`（取前 [`MAX_REFLECT_STEPS`] 步作签名），`distinct_sessions=1`
/// （故其 confidence 低、永远走人审，不会被高置信自动激活）。最多返回 `max` 条。
pub fn mine_reflection_candidates(events: &[CollectedEvent], min_steps: usize, max: usize) -> Vec<MinedPattern> {
    let mut by_conv: BTreeMap<String, Vec<(String, String, String, i64)>> =
        BTreeMap::new();
    for ev in events {
        if ev.source != "tool_calls" {
            continue;
        }
        let name = ev.data.get("name").and_then(|n| n.as_str()).unwrap_or("");
        if name.is_empty() {
            continue;
        }
        let Some(conv) = ev
            .data
            .get("conversation_id")
            .and_then(|c| c.as_str())
            .and_then(|id| ConversationId::try_from(id).ok())
            .map(ConversationId::into_string)
        else {
            continue;
        };
        let call_id = ev.data.get("call_id").and_then(|c| c.as_str()).unwrap_or("").to_owned();
        by_conv
            .entry(conv)
            .or_default()
            .push((name.to_owned(), call_id, ev.event_id.clone(), ev.ts));
    }
    let mut out = Vec::new();
    for (conv, calls) in &by_conv {
        if out.len() >= max {
            break;
        }
        let mut seq: Vec<(String, String, String, i64)> = Vec::new();
        for (name, call_id, event_id, ts) in calls {
            if seq.last().map(|(n, _, _, _)| n == name).unwrap_or(false) {
                continue;
            }
            seq.push((name.clone(), call_id.clone(), event_id.clone(), *ts));
        }
        if seq.len() < min_steps {
            continue;
        }
        let take = seq.len().min(MAX_REFLECT_STEPS);
        let taken = &seq[..take];
        let names: Vec<String> = taken.iter().map(|(n, _, _, _)| n.clone()).collect();
        let examples: Vec<String> = taken
            .iter()
            .take(8)
            .map(|(_, _, event_id, _)| event_id.clone())
            .collect();
        let anchor = TranscriptAnchor {
            conversation_id: conv.clone(),
            start_ts: taken.first().map(|(_, _, _, t)| *t).unwrap_or(0),
            end_ts: taken.last().map(|(_, _, _, t)| *t).unwrap_or(0),
            pad_turns: ANCHOR_PAD_TURNS,
            call_ids: taken
                .iter()
                .filter(|(_, call_id, _, _)| !call_id.is_empty())
                .map(|(_, call_id, _, _)| call_id.clone())
                .collect(),
        };
        out.push(MinedPattern {
            signature: tool_call_signature(&names),
            steps: names,
            count: 1,
            distinct_sessions: 1,
            example_event_ids: examples,
            anchor,
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn conversation_fixture(sequence: u64) -> String {
        let raw = format!("0190f5fe-7c00-7a00-8abc-{sequence:012}");
        nomifun_common::ConversationId::try_from(raw.as_str()).unwrap().into_string()
    }

    fn tool_event(conv: &str, name: &str, call_id: &str, ts: i64) -> CollectedEvent {
        CollectedEvent {
            event_id: nomifun_common::generate_id(),
            ts,
            source: "tool_calls".to_owned(),
            name: "tool.call".to_owned(),
            data: json!({ "name": name, "conversation_id": conv, "call_id": call_id }),
        }
    }

    /// 三个会话各做一遍 [grep, read, edit] → 恰好一个套路（极大窗去重掉子窗）。
    #[test]
    fn mines_repeated_three_step_pattern_once() {
        let mut events = Vec::new();
        let mut ts = 0;
        let conversations = [conversation_fixture(1), conversation_fixture(2), conversation_fixture(3)];
        for conv in &conversations {
            for (i, tool) in ["grep", "read", "edit"].iter().enumerate() {
                ts += 1;
                events.push(tool_event(conv, tool, &format!("{conv}-{i}"), ts));
            }
        }
        let patterns = mine_patterns(&events, 3, 2);
        assert_eq!(patterns.len(), 1, "expected exactly one maximal pattern, got {patterns:?}");
        assert_eq!(patterns[0].steps, vec!["grep".to_string(), "read".into(), "edit".into()]);
        assert!(patterns[0].count >= 3);
        assert!(patterns[0].distinct_sessions >= 2);
        // 签名只含工具名，绝无参数/秘密。
        assert!(!patterns[0].signature.contains("SECRET"));
        assert_eq!(patterns[0].signature, "grep\u{1f}read\u{1f}edit");
        // 锚指向一个代表性会话窗口(供重水合定位"那一段")。
        let a = &patterns[0].anchor;
        assert!(conversations.contains(&a.conversation_id), "anchor conv: {a:?}");
        assert!(a.start_ts > 0 && a.end_ts >= a.start_ts, "anchor ts bounds: {a:?}");
        assert_eq!(a.call_ids.len(), 3, "3 步窗 → 3 个 call_id: {a:?}");
        assert!(a.call_ids.iter().all(|c| c.starts_with(&a.conversation_id)), "call_ids 同会话: {a:?}");
    }

    #[test]
    fn provenance_uses_collected_event_ids_not_runtime_call_ids() {
        let conversation = conversation_fixture(20);
        let first_event_id = nomifun_common::generate_id();
        let second_event_id = nomifun_common::generate_id();
        let events = vec![
            CollectedEvent {
                event_id: first_event_id.clone(),
                ts: 1,
                source: "tool_calls".into(),
                name: "tool.call".into(),
                data: json!({
                    "name": "grep",
                    "conversation_id": conversation,
                    "call_id": "runtime-call-1"
                }),
            },
            CollectedEvent {
                event_id: second_event_id,
                ts: 2,
                source: "tool_calls".into(),
                name: "tool.call".into(),
                data: json!({
                    "name": "read",
                    "conversation_id": conversation,
                    "call_id": "runtime-call-2"
                }),
            },
        ];

        let patterns = mine_patterns(&events, 1, 1);
        assert_eq!(patterns.len(), 1);
        assert_eq!(patterns[0].example_event_ids, vec![first_event_id]);
        assert_eq!(
            patterns[0].anchor.call_ids,
            vec!["runtime-call-1".to_owned(), "runtime-call-2".to_owned()]
        );
    }

    /// 反思候选也带定位锚(单会话整段)。
    #[test]
    fn reflection_candidate_carries_anchor() {
        let mut events = Vec::new();
        let conversation = conversation_fixture(4);
        for (i, tool) in ["a", "b", "c", "d", "e"].iter().enumerate() {
            events.push(tool_event(&conversation, tool, &format!("{conversation}-{i}"), (i as i64) + 10));
        }
        let cands = mine_reflection_candidates(&events, 4, 3);
        assert_eq!(cands.len(), 1);
        let a = &cands[0].anchor;
        assert_eq!(a.conversation_id, conversation);
        assert_eq!(a.start_ts, 10);
        assert!(a.end_ts >= a.start_ts);
        assert!(!a.call_ids.is_empty());
    }

    /// 只出现在单个会话的序列被排除（distinct_sessions < 阈值）。
    #[test]
    fn excludes_single_session_sequences() {
        let mut events = Vec::new();
        let conversation = conversation_fixture(5);
        // 反复出现但只在一个会话里 → distinct_sessions = 1
        for i in 0..5 {
            events.push(tool_event(&conversation, "foo", &format!("a{i}"), i * 2));
            events.push(tool_event(&conversation, "bar", &format!("b{i}"), i * 2 + 1));
        }
        let patterns = mine_patterns(&events, 2, 2);
        assert!(patterns.is_empty(), "single-session pattern must be excluded, got {patterns:?}");
    }

    /// 连续重复同一工具被折叠为一步（不会把 [grep,grep,read] 当成三步套路）。
    #[test]
    fn collapses_consecutive_duplicates() {
        let mut events = Vec::new();
        let mut ts = 0;
        for conv in [conversation_fixture(6), conversation_fixture(7)] {
            for tool in ["grep", "grep", "grep", "read"] {
                ts += 1;
                events.push(tool_event(&conv, tool, &format!("{conv}-{ts}"), ts));
            }
        }
        let patterns = mine_patterns(&events, 2, 2);
        assert_eq!(patterns.len(), 1);
        assert_eq!(patterns[0].steps, vec!["grep".to_string(), "read".into()]);
    }

    /// 非 tool_calls 来源被忽略。
    #[test]
    fn ignores_non_tool_call_sources() {
        let events = vec![CollectedEvent {
            event_id: nomifun_common::generate_id(),
            ts: 1,
            source: "companion_dialogues".to_owned(),
            name: "chat".to_owned(),
            data: json!({ "name": "whatever" }),
        }];
        assert!(mine_patterns(&events, 1, 1).is_empty());
    }

    /// 单个长会话 → 一条反思候选（distinct_sessions=1）；过短会话被排除。
    #[test]
    fn reflection_candidate_from_single_long_session() {
        let mut events = Vec::new();
        let mut ts = 0;
        let conversation = conversation_fixture(8);
        for tool in ["grep", "read", "edit", "write"] {
            ts += 1;
            events.push(tool_event(&conversation, tool, &format!("e{ts}"), ts));
        }
        events.push(tool_event(&conversation_fixture(9), "ls", "x", 100)); // 1-step session: excluded
        let cands = mine_reflection_candidates(&events, 4, 5);
        assert_eq!(cands.len(), 1);
        assert_eq!(cands[0].steps, vec!["grep".to_string(), "read".into(), "edit".into(), "write".into()]);
        assert_eq!(cands[0].distinct_sessions, 1);
    }
}
