//! Pure evaluation for non-Agent control steps. Scheduling and persistence stay
//! in the Engine; these functions only reduce dependency outputs into a decision.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

use nomifun_api_types::{
    ExecutionAttempt, ExecutionStep, JudgeAggregation, LoopStopPolicy, StepControlPolicy,
    VerificationPolicy,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub(crate) struct LoopRuntimeState {
    pub iteration: usize,
    #[serde(default)]
    pub output_hashes: Vec<u64>,
}

#[derive(Debug, Clone)]
pub(crate) enum ControlResolution {
    Complete {
        summary: String,
        runtime_state: Option<Value>,
    },
    Fail {
        summary: String,
        error: String,
        runtime_state: Option<Value>,
    },
    Repeat {
        body_step_id: String,
        runtime_state: Value,
    },
}

pub(crate) fn evaluate(
    step: &ExecutionStep,
    dependency_steps: &[&ExecutionStep],
    attempts: &[ExecutionAttempt],
) -> ControlResolution {
    match step.control_policy.as_ref() {
        Some(StepControlPolicy::Verify { vote }) => evaluate_verify(vote, dependency_steps, attempts),
        Some(StepControlPolicy::Judge {
            aggregation,
            candidate_count,
        }) => evaluate_judge(*aggregation, *candidate_count, dependency_steps, attempts),
        Some(StepControlPolicy::Loop {
            max_iterations,
            stop,
        }) => evaluate_loop(*max_iterations, stop, dependency_steps, attempts, step),
        None => ControlResolution::Fail {
            summary: "控制步骤缺少策略".to_owned(),
            error: "missing control policy".to_owned(),
            runtime_state: None,
        },
    }
}

fn latest_output<'a>(step_id: &str, attempts: &'a [ExecutionAttempt]) -> Option<&'a str> {
    attempts
        .iter()
        .filter(|attempt| attempt.step_id == step_id)
        .max_by_key(|attempt| attempt.attempt_no)
        .and_then(|attempt| attempt.output_summary.as_deref())
}

fn evaluate_verify(
    policy: &VerificationPolicy,
    dependencies: &[&ExecutionStep],
    attempts: &[ExecutionAttempt],
) -> ControlResolution {
    let mut verdicts = Vec::with_capacity(dependencies.len());
    for step in dependencies {
        let Some(verdict) = latest_output(&step.step_id, attempts).and_then(parse_pass) else {
            return ControlResolution::Fail {
                summary: format!(
                    "验证失败：依赖步骤 '{}' 没有有效 PASS/FAIL 结果",
                    step.title
                ),
                error: format!(
                    "missing or invalid verification verdict for step {}",
                    step.step_id
                ),
                runtime_state: None,
            };
        };
        verdicts.push(verdict);
    }
    let passed = verdicts.iter().filter(|value| **value).count();
    let required = match policy {
        VerificationPolicy::Majority => verdicts.len() / 2 + 1,
        VerificationPolicy::Unanimous => verdicts.len(),
        VerificationPolicy::AtLeast { count } => *count,
    };
    let summary = format!(
        "验证结果：{}（通过 {passed}/{}，要求 {required}）",
        if !verdicts.is_empty() && passed >= required {
            "PASS"
        } else {
            "FAIL"
        },
        verdicts.len(),
    );
    if !verdicts.is_empty() && passed >= required {
        ControlResolution::Complete {
            summary,
            runtime_state: None,
        }
    } else {
        ControlResolution::Fail {
            summary,
            error: "verification gate failed".to_owned(),
            runtime_state: None,
        }
    }
}

fn parse_pass(output: &str) -> Option<bool> {
    first_json_object(output)
        .and_then(|object| serde_json::from_str::<Value>(&object).ok())
        .and_then(|value| value.get("pass").and_then(Value::as_bool))
        .or_else(|| {
            let normalized = output.trim().to_ascii_lowercase();
            normalized.ends_with("pass").then_some(true).or_else(|| {
                normalized.ends_with("fail").then_some(false)
            })
        })
}

fn evaluate_judge(
    aggregation: JudgeAggregation,
    configured_candidates: Option<usize>,
    dependencies: &[&ExecutionStep],
    attempts: &[ExecutionAttempt],
) -> ControlResolution {
    let mut raw_ballots = Vec::with_capacity(dependencies.len());
    for step in dependencies {
        let Some(ballot) = latest_output(&step.step_id, attempts).and_then(parse_ballot) else {
            return ControlResolution::Fail {
                summary: format!("裁决失败：依赖步骤 '{}' 没有有效选票", step.title),
                error: format!("missing or invalid judge ballot for step {}", step.step_id),
                runtime_state: None,
            };
        };
        raw_ballots.push(ballot);
    }
    let candidate_count = configured_candidates
        .or_else(|| raw_ballots.iter().map(Vec::len).max())
        .unwrap_or(0);
    if candidate_count == 0 || raw_ballots.is_empty() {
        return ControlResolution::Fail {
            summary: "裁决失败：没有有效选票".to_owned(),
            error: "no valid judge ballots".to_owned(),
            runtime_state: None,
        };
    }

    let mut totals = vec![0.0; candidate_count];
    let mut counts = vec![0_usize; candidate_count];
    match aggregation {
        JudgeAggregation::Mean => {
            for ballot in &raw_ballots {
                for (index, score) in ballot.iter().take(candidate_count).enumerate() {
                    if let Some(score) = score.filter(|score| score.is_finite()) {
                        totals[index] += score;
                        counts[index] += 1;
                    }
                }
            }
            for index in 0..candidate_count {
                if counts[index] > 0 {
                    totals[index] /= counts[index] as f64;
                } else {
                    totals[index] = f64::NEG_INFINITY;
                }
            }
        }
        JudgeAggregation::Borda => {
            for ballot in &raw_ballots {
                let mut ranked: Vec<(usize, f64)> = ballot
                    .iter()
                    .take(candidate_count)
                    .enumerate()
                    .filter_map(|(index, score)| score.filter(|score| score.is_finite()).map(|score| (index, score)))
                    .collect();
                ranked.sort_by(|left, right| {
                    right
                        .1
                        .partial_cmp(&left.1)
                        .unwrap_or(std::cmp::Ordering::Equal)
                        .then(left.0.cmp(&right.0))
                });
                let ranked_len = ranked.len();
                for (rank, (index, _)) in ranked.into_iter().enumerate() {
                    totals[index] += (ranked_len - rank - 1) as f64;
                    counts[index] += 1;
                }
            }
        }
    }
    let Some((winner, score)) = totals
        .iter()
        .copied()
        .enumerate()
        .filter(|(_, score)| score.is_finite())
        .max_by(|left, right| {
            left.1
                .partial_cmp(&right.1)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| right.0.cmp(&left.0))
        })
    else {
        return ControlResolution::Fail {
            summary: "裁决失败：选票没有有效分数".to_owned(),
            error: "judge ballots contain no scores".to_owned(),
            runtime_state: None,
        };
    };
    ControlResolution::Complete {
        summary: format!(
            "裁决结果：候选 #{winner} 胜出（{}={score:.4}，有效选票 {}）",
            match aggregation {
                JudgeAggregation::Mean => "mean",
                JudgeAggregation::Borda => "borda",
            },
            raw_ballots.len(),
        ),
        runtime_state: Some(serde_json::json!({
            "winner_index": winner,
            "scores": totals,
            "ballot_count": raw_ballots.len(),
        })),
    }
}

fn parse_ballot(output: &str) -> Option<Vec<Option<f64>>> {
    let value: Value = serde_json::from_str(&first_json_object(output)?).ok()?;
    match value.get("scores")? {
        Value::Array(scores) => Some(scores.iter().map(Value::as_f64).collect()),
        Value::Object(scores) => {
            let max_index = scores.keys().filter_map(|key| key.parse::<usize>().ok()).max()?;
            let mut ballot = vec![None; max_index + 1];
            for (key, value) in scores {
                if let Ok(index) = key.parse::<usize>() {
                    ballot[index] = value.as_f64();
                }
            }
            Some(ballot)
        }
        _ => None,
    }
}

fn evaluate_loop(
    max_iterations: usize,
    stop: &LoopStopPolicy,
    dependencies: &[&ExecutionStep],
    attempts: &[ExecutionAttempt],
    controller: &ExecutionStep,
) -> ControlResolution {
    if dependencies.len() != 1 {
        return ControlResolution::Fail {
            summary: "循环控制器必须且只能有一个主体步骤".to_owned(),
            error: "loop requires exactly one body dependency".to_owned(),
            runtime_state: None,
        };
    }
    let body = dependencies[0];
    let output = latest_output(&body.step_id, attempts).unwrap_or_default();
    let mut state = attempts
        .iter()
        .filter(|attempt| attempt.step_id == controller.step_id)
        .max_by_key(|attempt| attempt.attempt_no)
        .and_then(|attempt| attempt.runtime_state.clone())
        .and_then(|value| serde_json::from_value::<LoopRuntimeState>(value).ok())
        .unwrap_or_default();
    state.iteration += 1;
    state.output_hashes.push(output_hash(output));

    let stop_now = state.iteration >= max_iterations.max(1)
        || match stop {
            LoopStopPolicy::MaxIterations => false,
            LoopStopPolicy::Predicate { done_marker } => {
                output.contains(done_marker)
                    || first_json_object(output)
                        .and_then(|object| serde_json::from_str::<Value>(&object).ok())
                        .and_then(|value| value.get("done").and_then(Value::as_bool))
                        == Some(true)
            }
            LoopStopPolicy::Stable { quiet_rounds } => {
                let required = (*quiet_rounds).max(1) + 1;
                state.output_hashes.len() >= required
                    && state.output_hashes[state.output_hashes.len() - required..]
                        .windows(2)
                        .all(|window| window[0] == window[1])
            }
            LoopStopPolicy::Approved => parse_pass(output) == Some(true),
        };
    let runtime_state = serde_json::to_value(&state).unwrap_or_else(|_| serde_json::json!({}));
    if stop_now {
        ControlResolution::Complete {
            summary: format!(
                "循环在第 {} 轮结束。\n\n{}",
                state.iteration,
                output.trim()
            ),
            runtime_state: Some(runtime_state),
        }
    } else {
        ControlResolution::Repeat {
            body_step_id: body.step_id.clone(),
            runtime_state,
        }
    }
}

fn output_hash(output: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    output.trim().hash(&mut hasher);
    hasher.finish()
}

fn first_json_object(raw: &str) -> Option<String> {
    let bytes = raw.as_bytes();
    let start = raw.find('{')?;
    let mut depth = 0_i32;
    let mut in_string = false;
    let mut escaped = false;
    for index in start..bytes.len() {
        let current = bytes[index] as char;
        if in_string {
            if escaped {
                escaped = false;
            } else if current == '\\' {
                escaped = true;
            } else if current == '"' {
                in_string = false;
            }
            continue;
        }
        match current {
            '"' => in_string = true,
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(raw[start..=index].to_owned());
                }
            }
            _ => {}
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use nomifun_common::{
        AgentStepMode, AgentToolPolicy, ExecutionAttemptStatus, ExecutionStepKind, ExecutionStepStatus,
        ParticipantAssignmentSource, StepFailurePolicy,
    };

    fn agent_step(step_id: &str) -> ExecutionStep {
        ExecutionStep {
            step_id: step_id.to_owned(),
            execution_id: "exec".to_owned(),
            title: step_id.to_owned(),
            spec: "test".to_owned(),
            profile: None,
            kind: ExecutionStepKind::Agent,
            agent_mode: Some(AgentStepMode::Normal),
            status: ExecutionStepStatus::Completed,
            tool_policy: AgentToolPolicy::Full,
            role: None,
            fanout_group: None,
            control_policy: None,
            failure_policy: StepFailurePolicy::FailExecution,
            assigned_participant_id: Some("0190f5fe-7c00-7a00-8000-000000000001".to_owned()),
            assignment_source: Some(ParticipantAssignmentSource::Automatic),
            assignment_score: None,
            assignment_rationale: None,
            assignment_locked: false,
            preset_prompt: None,
            graph_x: None,
            graph_y: None,
            dispatch_after: None,
            introduced_in_revision: 0,
            superseded_in_revision: None,
            version: 0,
            created_at: 0,
            updated_at: 0,
        }
    }

    fn completed_attempt(step_id: &str, output: &str) -> ExecutionAttempt {
        ExecutionAttempt {
            attempt_id: format!("{step_id}-attempt"),
            execution_id: "exec".to_owned(),
            step_id: step_id.to_owned(),
            attempt_no: 0,
            participant_id: Some("0190f5fe-7c00-7a00-8000-000000000001".to_owned()),
            conversation_id: None,
            status: ExecutionAttemptStatus::Completed,
            trigger_reason: "test".to_owned(),
            effective_config: serde_json::json!({}),
            question: None,
            error: None,
            output_summary: Some(output.to_owned()),
            output_files: Vec::new(),
            tokens: None,
            retry_after: None,
            runtime_state: None,
            started_at: Some(0),
            finished_at: Some(0),
            version: 0,
            created_at: 0,
            updated_at: 0,
        }
    }

    #[test]
    fn verdict_parser_supports_json_and_markers() {
        assert_eq!(parse_pass("result: {\"pass\":true}"), Some(true));
        assert_eq!(parse_pass("FAIL"), Some(false));
    }

    #[test]
    fn ballot_parser_supports_array_and_index_object() {
        assert_eq!(parse_ballot("{\"scores\":[0.2,0.8]}").unwrap().len(), 2);
        assert_eq!(parse_ballot("{\"scores\":{\"1\":0.8}}").unwrap(), vec![None, Some(0.8)]);
    }

    #[test]
    fn verification_fails_closed_when_any_declared_verdict_is_missing() {
        let one = agent_step("0190f5fe-7c00-7a00-8000-000000000001");
        let two = agent_step("0190f5fe-7c00-7a00-8000-000000000002");
        let three = agent_step("0190f5fe-7c00-7a00-8000-000000000003");
        let dependencies = vec![&one, &two, &three];
        let attempts = vec![completed_attempt(
            "0190f5fe-7c00-7a00-8000-000000000001",
            "PASS",
        )];

        let resolution = evaluate_verify(
            &VerificationPolicy::Unanimous,
            &dependencies,
            &attempts,
        );
        assert!(matches!(resolution, ControlResolution::Fail { .. }));
    }

    #[test]
    fn judge_fails_closed_when_any_declared_ballot_is_invalid() {
        let one = agent_step("0190f5fe-7c00-7a00-8000-000000000001");
        let two = agent_step("0190f5fe-7c00-7a00-8000-000000000002");
        let dependencies = vec![&one, &two];
        let attempts = vec![
            completed_attempt(
                "0190f5fe-7c00-7a00-8000-000000000001",
                r#"{"scores":[0.1,0.9]}"#,
            ),
            completed_attempt(
                "0190f5fe-7c00-7a00-8000-000000000002",
                "not a ballot",
            ),
        ];

        let resolution = evaluate_judge(
            JudgeAggregation::Mean,
            Some(2),
            &dependencies,
            &attempts,
        );
        assert!(matches!(resolution, ControlResolution::Fail { .. }));
    }
}
