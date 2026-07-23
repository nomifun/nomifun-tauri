//! 能力 Router 打分（纯函数）。
//!
//! Deterministic, side-effect-free scoring of execution participants against a step
//! profile. No LLM, no I/O — given the same inputs, [`score_participant`] and
//! [`rank_participants`] always produce the same outputs. This is the cheap
//! first-pass router the Engine consults before falling back to an
//! LLM planner.
//!
//! Scoring is two-phase:
//! 1. **Hard filters** — if a member cannot satisfy a non-negotiable task
//!    requirement (vision modality, tool use) it is *excluded* entirely
//!    ([`score_participant`] returns `None`).
//! 2. **Soft score** — surviving members accumulate a `f64` score from
//!    capability hits (strength match, reasoning tier, cost tier, modality
//!    coverage). Higher is better.
//!
//! A member with `capability == None` is treated as a baseline agent:
//! no extra modalities (no vision), no tool use, `reasoning == "medium"`,
//! `cost_tier`/`speed_tier == "standard"`, and no declared strengths.

use nomifun_api_types::{ExecutionParticipant, ExecutionStepProfile, ParticipantCapability};

/// A member that survived the hard filters, with its soft score and a
/// human-readable rationale describing which factors contributed.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ScoredCandidate {
    /// Index of the member in the original `members` slice passed to
    /// [`rank_participants`].
    pub participant_index: usize,
    /// Soft score; higher is a better fit. May be negative.
    pub score: f64,
    /// Chinese phrase listing the hit factors, e.g.
    /// `"强项匹配[coding]; 高推理; 视觉就绪"`. Never empty.
    pub rationale: String,
}

/// Baseline capability profile applied when a member declares none.
fn baseline_profile() -> ParticipantCapability {
    ParticipantCapability {
        strengths: Vec::new(),
        modalities: Vec::new(),
        tools: false,
        reasoning: "medium".to_string(),
        cost_tier: "standard".to_string(),
        speed_tier: "standard".to_string(),
    }
}

/// Score a single member against `profile`.
///
/// Returns `None` when a hard filter excludes the member:
/// - `profile.needs_vision` but the member's modalities do not contain
///   `"vision"`.
/// - `profile.kind == "tool"` but the member does not support tools.
///
/// Otherwise returns `Some((score, rationale))` where `rationale` is a
/// non-empty Chinese phrase listing the factors that contributed.
pub(crate) fn score_participant(member: &ExecutionParticipant, profile: &ExecutionStepProfile) -> Option<(f64, String)> {
    if member
        .constraints
        .as_ref()
        .and_then(|constraints| constraints.allowed_profile_kinds.as_ref())
        .is_some_and(|allowed| !allowed.iter().any(|kind| kind == &profile.kind))
    {
        return None;
    }
    let owned;
    let cap = match member.capability.as_ref() {
        Some(c) => c,
        None => {
            owned = baseline_profile();
            &owned
        }
    };

    let has_vision = cap.modalities.iter().any(|m| m == "vision");

    // ---- Hard filters (exclusion) ----
    if profile.needs_vision && !has_vision {
        return None;
    }
    if profile.kind == "tool" && !cap.tools {
        return None;
    }

    // ---- Soft score (accumulate) ----
    let mut score = 0.0_f64;
    let mut factors: Vec<String> = Vec::new();

    // kind ↔ strengths hit.
    if !profile.kind.is_empty() && cap.strengths.iter().any(|s| s == &profile.kind) {
        score += 2.0;
        factors.push(format!("强项匹配[{}]", profile.kind));
    }

    // Reasoning tier preference when high reasoning is needed.
    if profile.needs_high_reasoning {
        match cap.reasoning.as_str() {
            "high" => {
                score += 2.0;
                factors.push("高推理".to_string());
            }
            "low" => {
                score -= 1.0;
                factors.push("低推理(扣分)".to_string());
            }
            // "medium" and any other tier are neutral.
            _ => {}
        }
    }

    // Cost-tier preference: bulk work favors economy; non-bulk (quality)
    // work favors premium.
    if profile.bulk && cap.cost_tier == "economy" {
        score += 1.0;
        factors.push("经济档(批量)".to_string());
    } else if !profile.bulk && cap.cost_tier == "premium" {
        score += 0.5;
        factors.push("高质档".to_string());
    }

    // Modality coverage: vision-ready when the task wants vision.
    if profile.needs_vision && has_vision {
        score += 0.5;
        factors.push("视觉就绪".to_string());
    }

    let rationale = if factors.is_empty() {
        "基础能力可用".to_string()
    } else {
        factors.join("; ")
    };

    Some((score, rationale))
}

/// Rank every member against `profile`, dropping those excluded by hard
/// filters.
///
/// The result is sorted by `score` descending, then by `participant_index`
/// ascending as a deterministic, stable tie-break. Each
/// [`ScoredCandidate::participant_index`] refers to the member's position in the
/// original `members` slice.
///
/// When every member is excluded the returned vec is empty — callers should
/// fall back (e.g. to an LLM planner).
pub(crate) fn rank_participants(members: &[ExecutionParticipant], profile: &ExecutionStepProfile) -> Vec<ScoredCandidate> {
    let mut scored: Vec<ScoredCandidate> = members
        .iter()
        .enumerate()
        .filter_map(|(participant_index, member)| {
            score_participant(member, profile).map(|(score, rationale)| ScoredCandidate {
                participant_index,
                score,
                rationale,
            })
        })
        .collect();

    // Sort by score desc; tie-break by participant_index asc. We sort by
    // participant_index first (ascending), then by score descending with a
    // *stable* sort, so equal scores keep their participant_index-ascending order.
    scored.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.participant_index.cmp(&b.participant_index))
    });

    scored
}

#[cfg(test)]
mod tests {
    use super::*;
    use nomifun_api_types::ParticipantConstraints;

    /// Build a member at a given index with an optional capability profile.
    fn member_with(cap: Option<ParticipantCapability>) -> ExecutionParticipant {
        ExecutionParticipant {
            participant_id: "0190f5fe-7c00-7a00-8000-000000000031".to_owned(),
            execution_id: "0190f5fe-7c00-7a00-8000-000000000032".to_owned(),
            source_agent_id: "0190f5fe-7c00-7a00-8000-000000000114".to_owned(),
            preset_id: None,
            preset_revision: None,
            preset_snapshot: None,
            provider_id: None,
            model: None,
            role: None,
            capability: cap,
            constraints: None,
            sort_order: 0,
            description: None,
            system_prompt: None,
            enabled_skills: Vec::new(),
            disabled_builtin_skills: Vec::new(),
            introduced_in_revision: 1,
            retired_in_revision: None,
            created_at: 0,
        }
    }

    fn cap(
        strengths: &[&str],
        modalities: &[&str],
        tools: bool,
        reasoning: &str,
        cost_tier: &str,
    ) -> ParticipantCapability {
        ParticipantCapability {
            strengths: strengths.iter().map(|s| s.to_string()).collect(),
            modalities: modalities.iter().map(|s| s.to_string()).collect(),
            tools,
            reasoning: reasoning.to_string(),
            cost_tier: cost_tier.to_string(),
            speed_tier: "standard".to_string(),
        }
    }

    fn profile(
        kind: &str,
        needs_vision: bool,
        needs_high_reasoning: bool,
        bulk: bool,
    ) -> ExecutionStepProfile {
        ExecutionStepProfile {
            kind: kind.to_string(),
            needs_vision,
            needs_long_context: false,
            needs_high_reasoning,
            bulk,
        }
    }

    // (a) Hard filter: needs_vision task + member without vision → excluded.
    #[test]
    fn needs_vision_without_vision_is_excluded() {
        let m = member_with(Some(cap(&[], &["text"], false, "medium", "standard")));
        let p = profile("analysis", true, false, false);
        assert!(score_participant(&m, &p).is_none());
    }

    // (a) Hard filter: needs_vision task + member WITH vision → Some.
    #[test]
    fn needs_vision_with_vision_is_scored() {
        let m = member_with(Some(cap(&[], &["text", "vision"], false, "medium", "standard")));
        let p = profile("analysis", true, false, false);
        let scored = score_participant(&m, &p);
        assert!(scored.is_some());
        // Vision-ready contributes to score and rationale.
        let (score, rationale) = scored.unwrap();
        assert!(score >= 0.5);
        assert!(rationale.contains("视觉就绪"));
    }

    // Hard filter: kind=="tool" without tool support → excluded; with → Some.
    #[test]
    fn tool_kind_requires_tools() {
        let no_tools = member_with(Some(cap(&[], &[], false, "medium", "standard")));
        let with_tools = member_with(Some(cap(&[], &[], true, "medium", "standard")));
        let p = profile("tool", false, false, false);
        assert!(score_participant(&no_tools, &p).is_none());
        assert!(score_participant(&with_tools, &p).is_some());
    }

    #[test]
    fn profile_kind_allow_list_is_a_hard_routing_constraint() {
        let mut member = member_with(None);
        member.constraints = Some(ParticipantConstraints {
            max_concurrency: None,
            allowed_profile_kinds: Some(vec!["research".to_owned()]),
        });

        assert!(
            score_participant(&member, &profile("research", false, false, false)).is_some()
        );
        assert!(
            score_participant(&member, &profile("coding", false, false, false)).is_none()
        );
    }

    // None capability is treated as baseline (no vision, no tools).
    #[test]
    fn none_profile_is_baseline() {
        let baseline = member_with(None);
        // Excluded from a vision task.
        assert!(score_participant(&baseline, &profile("analysis", true, false, false)).is_none());
        // Excluded from a tool task.
        assert!(score_participant(&baseline, &profile("tool", false, false, false)).is_none());
        // Allowed for a plain task, with a non-empty rationale.
        let (_, rationale) = score_participant(&baseline, &profile("writing", false, false, false))
            .expect("baseline should pass a plain task");
        assert!(!rationale.is_empty());
    }

    // (b) coding strengths ranks above non-coding.
    #[test]
    fn coding_strength_outranks_non_coding() {
        let coder = member_with(Some(cap(&["coding"], &[], false, "medium", "standard")));
        let writer = member_with(Some(cap(&["writing"], &[], false, "medium", "standard")));
        let p = profile("coding", false, false, false);
        let (coder_score, coder_rationale) = score_participant(&coder, &p).unwrap();
        let (writer_score, _) = score_participant(&writer, &p).unwrap();
        assert!(coder_score > writer_score);
        assert!(coder_rationale.contains("强项匹配[coding]"));
    }

    // (c) high-reasoning preference: high > low.
    #[test]
    fn high_reasoning_preferred_over_low() {
        let high = member_with(Some(cap(&[], &[], false, "high", "standard")));
        let low = member_with(Some(cap(&[], &[], false, "low", "standard")));
        let p = profile("analysis", false, true, false);
        let (high_score, high_rationale) = score_participant(&high, &p).unwrap();
        let (low_score, _) = score_participant(&low, &p).unwrap();
        assert!(high_score > low_score);
        assert!(high_rationale.contains("高推理"));
    }

    // bulk favors economy cost tier; non-bulk favors premium.
    #[test]
    fn cost_tier_matches_bulk_intent() {
        let economy = member_with(Some(cap(&[], &[], false, "medium", "economy")));
        let premium = member_with(Some(cap(&[], &[], false, "medium", "premium")));

        // Bulk task: economy wins.
        let bulk = profile("writing", false, false, true);
        let (eco_bulk, _) = score_participant(&economy, &bulk).unwrap();
        let (prem_bulk, _) = score_participant(&premium, &bulk).unwrap();
        assert!(eco_bulk > prem_bulk);

        // Non-bulk (quality) task: premium wins.
        let quality = profile("writing", false, false, false);
        let (eco_q, _) = score_participant(&economy, &quality).unwrap();
        let (prem_q, _) = score_participant(&premium, &quality).unwrap();
        assert!(prem_q > eco_q);
    }

    // (d) rank_participants sorts desc + stable tie-break (lower index first).
    #[test]
    fn rank_participants_sorts_desc_with_stable_tiebreak() {
        let members = vec![
            // index 0: tie score (no hits) — should appear before index 2.
            member_with(Some(cap(&[], &[], false, "medium", "standard"))),
            // index 1: coding hit → highest.
            member_with(Some(cap(&["coding"], &[], false, "medium", "standard"))),
            // index 2: tie score (no hits) — same as index 0.
            member_with(Some(cap(&[], &[], false, "medium", "standard"))),
        ];
        let p = profile("coding", false, false, false);
        let ranked = rank_participants(&members, &p);

        assert_eq!(ranked.len(), 3);
        // Highest score first.
        assert_eq!(ranked[0].participant_index, 1);
        // Tie-break: lower original index first.
        assert_eq!(ranked[1].participant_index, 0);
        assert_eq!(ranked[2].participant_index, 2);
        // Descending scores.
        assert!(ranked[0].score >= ranked[1].score);
        assert!(ranked[1].score >= ranked[2].score);
        // Tied members have equal score.
        assert_eq!(ranked[1].score, ranked[2].score);
    }

    // (e) all members excluded → empty vec.
    #[test]
    fn all_excluded_yields_empty() {
        let members = vec![
            member_with(Some(cap(&[], &["text"], false, "medium", "standard"))),
            member_with(None),
        ];
        // Vision task, neither member has vision.
        let p = profile("analysis", true, false, false);
        let ranked = rank_participants(&members, &p);
        assert!(ranked.is_empty());
    }

    // (f) rationale is non-empty and lists hit factors.
    #[test]
    fn rationale_lists_hit_factors() {
        let m = member_with(Some(cap(
            &["coding"],
            &["vision"],
            true,
            "high",
            "premium",
        )));
        let p = profile("coding", true, true, false);
        let (_, rationale) = score_participant(&m, &p).unwrap();
        assert!(!rationale.is_empty());
        assert!(rationale.contains("强项匹配[coding]"));
        assert!(rationale.contains("高推理"));
        assert!(rationale.contains("视觉就绪"));
        assert!(rationale.contains("高质档"));
        // Factors are joined with "; ".
        assert!(rationale.contains("; "));
    }
}
