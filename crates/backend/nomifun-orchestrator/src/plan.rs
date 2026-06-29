//! 主管规划 (PlanProducer): turn a goal + a fleet member snapshot into an
//! executable task DAG ([`PlannedDag`]).
//!
//! [`LlmPlanProducer`] does one structured one-shot LLM call against a "lead"
//! model: it builds a planning prompt, asks the model for a strict-JSON
//! `{"tasks":[...]}` object, and parses it via [`parse_plan`].
//!
//! [`parse_plan`] is the heart of testability and is **fail-soft**: it extracts
//! the first JSON object from the raw model text (stripping ```json fences and
//! surrounding prose), parses it into a [`PlannedDag`], and on ANY failure
//! (no JSON, bad shape, empty `tasks`) logs a `warn!` and returns a single-task
//! fallback DAG built from the goal — so the Run engine always has something
//! executable.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use nomifun_ai_agent::{one_shot_completion, resolve_provider_config, user_message};
use nomifun_api_types::{FleetMember, PlannedDag, PlannedTask};
use nomifun_common::{AppError, ProviderWithModel};
use nomifun_db::IProviderRepository;
use nomifun_db::models::Provider;

/// How many tokens the planner may use for its one-shot DAG completion.
const PLAN_MAX_TOKENS: u32 = 4096;

/// Max length of the fallback task title derived from the goal.
const FALLBACK_TITLE_LEN: usize = 60;

/// Per-model user-authored descriptions, keyed by `(provider_id, model)`.
///
/// Built from the providers' `model_descriptions` JSON (Task 1) and threaded
/// into the planning prompt so the lead model can pick the best-matching model
/// per task. A missing key means "no description" (rendered as `-`).
type DescriptionMap = HashMap<(String, String), String>;


/// Produces a task DAG from a goal. The Run engine consumes the result.
#[async_trait]
pub trait PlanProducer: Send + Sync {
    /// 把目标拆成任务 DAG。`members` 是 fleet 成员快照(供按 index 分派)。
    async fn produce(&self, goal: &str, members: &[FleetMember]) -> Result<PlannedDag, AppError>;
}

/// Production planner: a single structured LLM call against a "lead" model
/// yields a [`PlannedDag`] JSON, parsed fail-soft via [`parse_plan`].
///
/// Holds the `provider_repo` plus the `encryption_key` and `workspace` that
/// [`resolve_provider_config`] needs to materialize a `Config` from the
/// `lead` provider row (mirrors `nomifun-idmm`'s `LiveCompleter`).
pub struct LlmPlanProducer {
    provider_repo: Arc<dyn IProviderRepository>,
    encryption_key: [u8; 32],
    workspace: PathBuf,
    lead: ProviderWithModel,
}

impl LlmPlanProducer {
    /// Build a planner against the `lead` model. `encryption_key` / `workspace`
    /// are required to resolve the provider config for the LLM call (the brief
    /// signature is `new(provider_repo, lead)`; these two are adapted in to
    /// satisfy `resolve_provider_config`, matching the IDMM sidecar pattern).
    pub fn new(
        provider_repo: Arc<dyn IProviderRepository>,
        encryption_key: [u8; 32],
        workspace: impl Into<PathBuf>,
        lead: ProviderWithModel,
    ) -> Self {
        Self {
            provider_repo,
            encryption_key,
            workspace: workspace.into(),
            lead,
        }
    }
}

/// Pick the planning "lead" model from the fleet members.
///
/// The app wires `LlmPlanProducer` with an EMPTY placeholder `lead`
/// (`provider_id:""`, `model:""`), which `resolve_provider_config` rejects
/// before `parse_plan`'s fail-soft can ever run — so every real run would stall
/// in `planning`. The real provider+model live on the fleet members, so derive
/// the lead from the FIRST member that carries BOTH a non-empty `provider_id`
/// AND a non-empty `model` (mirroring the Nomi-engine member contract in
/// `worker.rs`). If no member qualifies, fall back to the construction-time
/// `lead` override.
fn pick_lead(members: &[FleetMember], fallback: &ProviderWithModel) -> ProviderWithModel {
    for m in members {
        if let (Some(pid), Some(model)) = (m.provider_id.as_ref(), m.model.as_ref()) {
            if !pid.is_empty() && !model.is_empty() {
                return ProviderWithModel {
                    provider_id: pid.clone(),
                    model: model.clone(),
                    use_model: Some(model.clone()),
                };
            }
        }
    }
    fallback.clone()
}

#[async_trait]
impl PlanProducer for LlmPlanProducer {
    async fn produce(&self, goal: &str, members: &[FleetMember]) -> Result<PlannedDag, AppError> {
        // Derive the lead from the fleet members; `self.lead` is the
        // construction-time override/fallback only (the app wires it empty).
        let lead = pick_lead(members, &self.lead);

        // The model to plan with: prefer the explicit use_model alias, else model.
        let model = lead.use_model.as_deref().unwrap_or(&lead.model);

        let cfg = resolve_provider_config(
            &self.provider_repo,
            &self.encryption_key,
            &lead.provider_id,
            model,
            self.workspace.as_path(),
        )
        .await?;

        // Build the (provider_id, model) → description map so the prompt can
        // surface each member's user-authored model description. Fetch every
        // provider once via `list()` (cheaper than N `find_by_id` calls and the
        // member set is small), then decode each provider's `model_descriptions`
        // JSON fail-soft. A repo error here MUST NOT fail the plan — descriptions
        // are an optimization, so degrade to an empty map (all `desc=-`).
        let descriptions = match self.provider_repo.list().await {
            Ok(providers) => build_description_map(&providers, members),
            Err(err) => {
                tracing::warn!(error = %err, "failed to list providers for plan descriptions; planning without them");
                DescriptionMap::new()
            }
        };

        let user = build_plan_user_prompt(goal, members, &descriptions);
        let raw = one_shot_completion(&cfg, PLAN_SYSTEM, vec![user_message(user)], PLAN_MAX_TOKENS).await?;

        // parse_plan is fail-soft: a bad/empty reply degrades to a single-task DAG
        // rather than erroring out, so the engine always has an executable plan.
        Ok(parse_plan(&raw, goal))
    }
}

/// System prompt: instruct the model to output ONLY a strict-JSON task DAG.
const PLAN_SYSTEM: &str = "You are a planning supervisor for a multi-agent fleet. \
Decompose the user's GOAL into an executable task DAG and output ONLY a single JSON object — \
no prose, no explanation, no markdown fences. \
The JSON object MUST have exactly this shape:\n\
{\"tasks\":[{\"title\":string,\"spec\":string,\"role\":string,\"kind\":string?,\"pattern_config\":string?,\"task_profile\":{\"kind\":string,\"needs_vision\":bool,\"needs_long_context\":bool,\"needs_high_reasoning\":bool,\"bulk\":bool}?,\"depends_on\":[int],\"member_index\":int?,\"rationale\":string?}]}\n\
Rules:\n\
- \"depends_on\" lists the 0-based indices of EARLIER tasks (smaller index) this task depends on; the graph MUST be acyclic.\n\
- \"member_index\" is the 0-based index into the provided MEMBERS list, if you want to pre-assign the task to a member; omit it to let the engine route automatically.\n\
- Each member row carries a \"desc\" column: the user-authored description of that member's model. PREFER the member whose \"desc\" best matches the task and set \"member_index\" accordingly; \"desc=-\" means no description is available.\n\
- \"role\" is a SHORT Chinese role name naming the kind of work this task is (例如 规划/前端/后端/测试/设计/文档/研究). Give every task a role so the roles a run used can later be distilled into reusable assistants. Keep it to 2–4 字; reuse the same role name across tasks of the same kind.\n\
- \"kind\" is the task's EXECUTION MODE; omit it (or use \"agent\") for a normal single-agent task — this is the DEFAULT and should be the vast majority of tasks. The other values are:\n\
  - \"synthesis\": a task that MERGES/synthesizes its dependency tasks' outputs into one coherent final result. Use it for a closing step like 「综合/合并上述产出，写出最终的 X」: set \"kind\":\"synthesis\" and make \"depends_on\" list every task whose output it should merge. A synthesis task needs no tools of its own — it reasons over the upstream results you give it.\n\
  - \"verify\": a NO-AGENT aggregator that VALIDATES an earlier task's result by majority/quorum vote of independent skeptics, then GATES the work that depends on it. Use it when a result must be checked before downstream work proceeds (correctness-critical output, a plan/spec others will build on). To set up a verify gate emit, for the task T you want to validate:\n\
    1) N independent SKEPTIC tasks (N usually 3) — each is a normal \"kind\":\"agent\" task that \"depends_on\":[T] and whose \"spec\" instructs it to CRITICALLY and INDEPENDENTLY evaluate T's result and OUTPUT ONLY a strict-JSON verdict: {\\\"pass\\\": true|false, \\\"critique\\\": \\\"<one-line reason>\\\"}. Phrase each skeptic's spec a little differently so they don't all make the same mistake.\n\
    2) ONE \"kind\":\"verify\" task that \"depends_on\" ALL N skeptics. Its \"pattern_config\" carries the vote policy as a JSON string: \"{\\\"vote\\\":\\\"majority\\\"}\" (default — pass iff > half pass), \"{\\\"vote\\\":\\\"unanimous\\\"}\" (pass iff every skeptic passes), or \"{\\\"vote\\\":{\\\"threshold\\\":K}}\" (pass iff at least K pass). The verify task runs NO agent — the engine tallies the skeptics' verdicts itself — so give it an empty/short spec.\n\
    3) The downstream work that must only run on a PASS \"depends_on\" the verify task. On a FAIL verdict the engine SKIPS that downstream automatically (it never runs unvalidated).\n\
  - \"judge\": a NO-AGENT aggregator that PICKS THE BEST among M candidate results by averaging/ranking the scores of N independent judges. Use it to choose one winner among alternatives (e.g. several candidate designs/drafts/approaches). To set up a judge contest emit:\n\
    1) M CANDIDATE tasks (usually a fan-out group of \"kind\":\"agent\" siblings) — each produces ONE alternative. Their ORDER matters: candidate i is index i in every judge's ballot.\n\
    2) N independent JUDGE tasks (N usually 3) — each is a normal \"kind\":\"agent\" task that \"depends_on\" ALL M candidates and whose \"spec\" instructs it to SCORE EVERY candidate (0.0–1.0, higher = better) and OUTPUT ONLY a strict-JSON ballot scoring all M, e.g. {\\\"scores\\\":[0.8,0.3,0.6]} (array indexed by candidate order) or {\\\"scores\\\":{\\\"0\\\":0.8,\\\"1\\\":0.3,\\\"2\\\":0.6}} (object keyed by candidate index). Phrase each judge's spec a little differently so they don't all weigh the same way.\n\
    3) ONE \"kind\":\"judge\" task that \"depends_on\" ALL N judges. Its \"pattern_config\" carries the aggregation policy as a JSON string: \"{\\\"aggregate\\\":\\\"mean\\\"}\" (default — average each candidate's scores across judges; winner = highest mean) or \"{\\\"aggregate\\\":\\\"borda\\\"}\" (each judge RANKS the candidates by its scores, award M-1…0 Borda points, sum across judges; winner = highest total). Optionally add \"{\\\"candidates\\\":M}\" to pin the candidate count. The judge task runs NO agent — the engine aggregates the ballots itself — so give it an empty/short spec. It REPORTS the winning candidate index in its output (downstream can build on the winner).\n\
  - \"loop\": a NO-AGENT controller that RE-RUNS one BODY task in place, iterating until a stop condition is met OR a HARD iteration cap is hit. Use it for iterative refinement — keep improving/retrying ONE task until it is good enough (e.g. 「反复打磨这段文案直到没有可改之处」, 「重试直到测试通过」). To set up a loop emit EXACTLY two tasks:\n\
    1) a BODY \"kind\":\"agent\" task that does one round of the work. Its \"spec\" should produce output that can be re-run/refined each round; it sees its own previous round's output as upstream context.\n\
    2) ONE \"kind\":\"loop\" task that \"depends_on\":[BODY] (the body is its ONLY dependency). Its \"pattern_config\" is a JSON string carrying a REQUIRED hard cap and a stop criterion: \"{\\\"max_iter\\\":N,\\\"stop\\\":{...}}\". \"max_iter\" (a small N like 3–5) is the HARD upper bound — the loop ALWAYS stops at the cap even if the criterion never fires (this guarantees termination). \"stop\" is one of: \"{\\\"kind\\\":\\\"max_iter\\\"}\" (stop only at the cap), \"{\\\"kind\\\":\\\"predicate\\\",\\\"done_marker\\\":\\\"DONE\\\"}\" (stop early once the body output contains the marker text, or strict JSON {\\\"done\\\":true}; instruct the body to emit the marker when it judges itself finished), or \"{\\\"kind\\\":\\\"dry\\\",\\\"quiet_rounds\\\":K}\" (stop early once K consecutive rounds produce the SAME body output — no further change). The loop task runs NO agent — the engine re-dispatches the body and evaluates the stop condition itself — so give it an empty/short spec. Downstream work \"depends_on\" the LOOP task (NOT the body), so it waits for the whole iteration to finish.\n\
- FAN-OUT (parallel variants / shards) is expressed by PLANNING, NOT a special kind: when a step benefits from doing the same work in parallel (e.g. N independent drafts, N shards of a corpus, N candidate approaches), emit MULTIPLE sibling tasks that all have \"kind\":\"agent\" and SHARE the same \"pattern_config\" group tag — a JSON string like \"{\\\"group\\\":\\\"<label>\\\"}\" (e.g. \"{\\\"group\\\":\\\"drafts\\\"}\"). Then add ONE downstream task (usually \"kind\":\"synthesis\") that \"depends_on\" ALL of those siblings to combine them. The engine runs the siblings in parallel automatically.\n\
- \"pattern_config\" is a raw JSON STRING (or omit it). It carries the fan-out \"group\" tag, a verify task's \"vote\" policy, a judge task's \"aggregate\" policy, OR a loop task's \"max_iter\"+\"stop\" criterion (see above); leave it out for ordinary tasks.\n\
- \"task_profile\", \"member_index\" and \"rationale\" are optional.\n\
- \"title\" is a short imperative label; \"spec\" is the full instruction the worker agent will execute.\n\
- Keep the plan minimal but complete: one task if the goal is atomic, several with dependencies if it must be staged. Do NOT over-use synthesis/fan-out/verify/judge/loop — reach for them only when the goal genuinely benefits from merging multiple outputs, parallel variants, validating a result before building on it, choosing the best among alternatives, or iteratively refining a single result until it stops improving.\n\
Output the JSON object and nothing else.";

/// Build the `(provider_id, model) → description` map for the prompt.
///
/// For each distinct `provider_id` referenced by a member, decode that
/// provider's `model_descriptions` JSON (`{model_id: description}`) and record
/// the description for every `(provider_id, model)` a member actually uses.
///
/// **Fail-soft on every axis** — descriptions are an optimization, never a hard
/// dependency:
/// - a provider with no row, `model_descriptions == None`, or the Task-1 default
///   `"{}"` contributes nothing;
/// - a malformed `model_descriptions` JSON is skipped (no entries) with a warn,
///   not propagated as an error;
/// - a blank/whitespace-only description is dropped (treated as "no description").
fn build_description_map(providers: &[Provider], members: &[FleetMember]) -> DescriptionMap {
    // Index providers by id for O(1) lookup as we walk the members.
    let by_id: HashMap<&str, &Provider> = providers.iter().map(|p| (p.id.as_str(), p)).collect();

    // Decode each referenced provider's model_descriptions once, fail-soft.
    let mut decoded: HashMap<&str, HashMap<String, String>> = HashMap::new();
    let mut out = DescriptionMap::new();

    for m in members {
        let (Some(pid), Some(model)) = (m.provider_id.as_deref(), m.model.as_deref()) else {
            continue;
        };
        if pid.is_empty() || model.is_empty() {
            continue;
        }

        // Lazily decode this provider's descriptions JSON the first time we see it.
        let table = decoded.entry(pid).or_insert_with(|| {
            let Some(provider) = by_id.get(pid) else {
                return HashMap::new();
            };
            let raw = provider.model_descriptions.as_deref().unwrap_or("{}");
            match serde_json::from_str::<HashMap<String, String>>(raw) {
                Ok(map) => map,
                Err(err) => {
                    tracing::warn!(
                        provider_id = pid,
                        error = %err,
                        "provider model_descriptions is not a JSON object; ignoring"
                    );
                    HashMap::new()
                }
            }
        });

        if let Some(desc) = table.get(model) {
            let trimmed = desc.trim();
            if !trimmed.is_empty() {
                out.insert((pid.to_string(), model.to_string()), trimmed.to_string());
            }
        }
    }
    out
}

/// Build the user message: the goal plus a compact member roster.
fn build_plan_user_prompt(
    goal: &str,
    members: &[FleetMember],
    descriptions: &DescriptionMap,
) -> String {
    let mut out = String::new();
    out.push_str("GOAL:\n");
    out.push_str(goal);
    out.push_str("\n\nMEMBERS (index, agent_id, role_hint, strengths, desc):\n");
    if members.is_empty() {
        out.push_str("(none — plan without pre-assigning member_index)\n");
    } else {
        for (i, m) in members.iter().enumerate() {
            let role = m.role_hint.as_deref().unwrap_or("-");
            let strengths = m
                .capability_profile
                .as_ref()
                .map(|p| p.strengths.join("/"))
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| "-".to_string());
            // Description column. PRIMARY source (P4 Task 3, Change 3): the
            // member's own `description` — Task 2 populates it for assistant-backed
            // members (the assistant's description) and decorates bare model-range
            // members that have a provider model description. FALLBACK (P3): the
            // `(provider_id, model)` → provider-`model_descriptions` map, kept for
            // bare members whose `description` was not decorated (no provider desc
            // OR an older snapshot without the field). Missing on both → "-".
            let member_desc = m.description.as_deref().map(str::trim).filter(|s| !s.is_empty());
            let desc = member_desc.unwrap_or_else(|| match (m.provider_id.as_deref(), m.model.as_deref()) {
                (Some(pid), Some(model)) => descriptions
                    .get(&(pid.to_string(), model.to_string()))
                    .map(String::as_str)
                    .unwrap_or("-"),
                _ => "-",
            });
            out.push_str(&format!(
                "{i}. {} | role={role} | strengths={strengths} | desc={desc}\n",
                m.agent_id
            ));
        }
    }
    out.push_str("\nReturn ONLY the JSON task DAG.");
    out
}

/// Parse the raw model text into a [`PlannedDag`], **fail-soft**.
///
/// Strips ```json/``` fences and surrounding prose, locates the first balanced
/// `{...}` JSON object, and deserializes it. On ANY failure — no JSON object,
/// malformed JSON, wrong shape, or an empty `tasks` array — logs a `warn!` and
/// returns a single-task fallback DAG derived from `goal` (so the engine always
/// has an executable plan).
pub fn parse_plan(raw: &str, goal: &str) -> PlannedDag {
    match extract_json_object(raw).and_then(|json| serde_json::from_str::<PlannedDag>(&json).ok()) {
        Some(dag) if !dag.tasks.is_empty() => dag,
        Some(_) => {
            tracing::warn!("planner output parsed but tasks were empty; using fallback DAG");
            fallback_dag(goal)
        }
        None => {
            tracing::warn!(
                raw_len = raw.len(),
                "planner output unparseable (no valid JSON task DAG); using fallback DAG"
            );
            fallback_dag(goal)
        }
    }
}

/// Extract the first balanced top-level `{...}` substring from `raw`,
/// after stripping any markdown code fences. Returns `None` if no balanced
/// object is found. Quote/escape aware so braces inside strings don't confuse
/// the brace counter.
fn extract_json_object(raw: &str) -> Option<String> {
    // Strip code fences first; the model is told not to use them, but be robust.
    let cleaned = raw.replace("```json", "").replace("```JSON", "").replace("```", "");

    let bytes = cleaned.as_bytes();
    let start = cleaned.find('{')?;

    let mut depth: i32 = 0;
    let mut in_string = false;
    let mut escaped = false;
    for i in start..bytes.len() {
        let c = bytes[i] as char;
        if in_string {
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                in_string = false;
            }
            continue;
        }
        match c {
            '"' => in_string = true,
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(cleaned[start..=i].to_string());
                }
            }
            _ => {}
        }
    }
    None
}

/// Single-task fallback DAG: the whole goal as one task assigned to member 0.
fn fallback_dag(goal: &str) -> PlannedDag {
    PlannedDag {
        tasks: vec![PlannedTask {
            title: truncate_title(goal),
            spec: goal.to_string(),
            task_profile: None,
            depends_on: vec![],
            member_index: Some(0),
            rationale: Some("fallback: planner output unparseable".to_string()),
            role: None,
            kind: "agent".to_string(),
            pattern_config: None,
        }],
    }
}

/// Truncate the goal into a short title (~`FALLBACK_TITLE_LEN` chars),
/// respecting char boundaries (CJK-safe).
fn truncate_title(goal: &str) -> String {
    let trimmed = goal.trim();
    if trimmed.chars().count() <= FALLBACK_TITLE_LEN {
        return trimmed.to_string();
    }
    let truncated: String = trimmed.chars().take(FALLBACK_TITLE_LEN).collect();
    format!("{truncated}…")
}

#[cfg(test)]
mod tests {
    use super::*;
    use nomifun_api_types::TaskProfile;

    /// Fixed 2-task DAG mock proving the [`PlanProducer`] trait shape. Reused by
    /// the Run engine (Task 6) to drive the scheduler without a live LLM.
    struct MockPlanProducer;

    #[async_trait]
    impl PlanProducer for MockPlanProducer {
        async fn produce(&self, _goal: &str, _members: &[FleetMember]) -> Result<PlannedDag, AppError> {
            Ok(PlannedDag {
                tasks: vec![
                    PlannedTask {
                        title: "Gather".to_string(),
                        spec: "collect sources".to_string(),
                        task_profile: None,
                        depends_on: vec![],
                        member_index: Some(0),
                        rationale: None,
                        role: None,
                        kind: "agent".to_string(),
                        pattern_config: None,
                    },
                    PlannedTask {
                        title: "Synthesize".to_string(),
                        spec: "write the report".to_string(),
                        task_profile: None,
                        depends_on: vec![0],
                        member_index: Some(1),
                        rationale: None,
                        role: None,
                        kind: "agent".to_string(),
                        pattern_config: None,
                    },
                ],
            })
        }
    }

    #[tokio::test]
    async fn mock_plan_producer_returns_fixed_two_task_dag() {
        let planner: Arc<dyn PlanProducer> = Arc::new(MockPlanProducer);
        let dag = planner.produce("anything", &[]).await.expect("mock never errors");

        assert_eq!(dag.tasks.len(), 2);
        assert_eq!(dag.tasks[0].title, "Gather");
        assert!(dag.tasks[0].depends_on.is_empty());
        assert_eq!(dag.tasks[1].title, "Synthesize");
        assert_eq!(dag.tasks[1].depends_on, vec![0]);
        assert_eq!(dag.tasks[1].member_index, Some(1));
    }

    #[test]
    fn parse_plan_accepts_bare_valid_json() {
        let raw = r#"{"tasks":[
            {"title":"Research","spec":"find sources","depends_on":[],"member_index":0},
            {"title":"Write","spec":"synthesize","depends_on":[0],"member_index":1,
             "task_profile":{"kind":"writing","needs_vision":false,"needs_long_context":true,"needs_high_reasoning":true,"bulk":false}}
        ]}"#;
        let dag = parse_plan(raw, "Research and write a report");

        assert_eq!(dag.tasks.len(), 2);
        assert_eq!(dag.tasks[0].title, "Research");
        assert_eq!(dag.tasks[0].member_index, Some(0));
        assert_eq!(dag.tasks[1].depends_on, vec![0]);
        let profile: &TaskProfile = dag.tasks[1].task_profile.as_ref().expect("profile decoded");
        assert_eq!(profile.kind, "writing");
        assert!(profile.needs_long_context);
        assert!(profile.needs_high_reasoning);
        assert!(!profile.bulk);
    }

    #[test]
    fn parse_plan_strips_json_fences() {
        let raw = "```json\n{\"tasks\":[{\"title\":\"One\",\"spec\":\"do it\",\"depends_on\":[]}]}\n```";
        let dag = parse_plan(raw, "goal");
        assert_eq!(dag.tasks.len(), 1);
        assert_eq!(dag.tasks[0].title, "One");
    }

    // 迁移 023: a plan whose closing task is kind="synthesis" parses, and its
    // pattern_config (fan-out group tag on a sibling) round-trips. Sibling agent
    // tasks sharing a "group" tag + a synthesis task depending on all of them is
    // the 1a fan-out → synthesis shape.
    #[test]
    fn parse_plan_accepts_kind_synthesis_and_fanout_group() {
        let raw = r#"{"tasks":[
            {"title":"Draft A","spec":"write variant A","depends_on":[],"kind":"agent","pattern_config":"{\"group\":\"drafts\"}"},
            {"title":"Draft B","spec":"write variant B","depends_on":[],"kind":"agent","pattern_config":"{\"group\":\"drafts\"}"},
            {"title":"Merge","spec":"combine the two drafts into the final","depends_on":[0,1],"kind":"synthesis"}
        ]}"#;
        let dag = parse_plan(raw, "produce a doc via parallel drafts");
        assert_eq!(dag.tasks.len(), 3);
        // The two fan-out siblings are agent tasks sharing the same group tag.
        assert_eq!(dag.tasks[0].kind, "agent");
        assert_eq!(dag.tasks[1].kind, "agent");
        assert_eq!(dag.tasks[0].pattern_config.as_deref(), Some("{\"group\":\"drafts\"}"));
        assert_eq!(dag.tasks[1].pattern_config.as_deref(), Some("{\"group\":\"drafts\"}"));
        // The closing task is synthesis and depends on BOTH siblings.
        assert_eq!(dag.tasks[2].kind, "synthesis");
        assert_eq!(dag.tasks[2].depends_on, vec![0, 1]);
        assert!(dag.tasks[2].pattern_config.is_none());
    }

    // UC-1b: a verify plan — a task to validate, N skeptic agent tasks each
    // depending on it, and a `verify` aggregator depending on all skeptics with a
    // vote policy in pattern_config — parses, and the verify kind + vote policy
    // round-trip. parse_plan stays fail-soft for the kind (it is kept as-is; the
    // engine recognizes "verify").
    #[test]
    fn parse_plan_accepts_verify_skeptics_and_aggregator() {
        let raw = r#"{"tasks":[
            {"title":"Build","spec":"build the feature","depends_on":[],"kind":"agent"},
            {"title":"Skeptic 1","spec":"evaluate; output {\"pass\":bool}","depends_on":[0],"kind":"agent"},
            {"title":"Skeptic 2","spec":"evaluate; output {\"pass\":bool}","depends_on":[0],"kind":"agent"},
            {"title":"Skeptic 3","spec":"evaluate; output {\"pass\":bool}","depends_on":[0],"kind":"agent"},
            {"title":"Gate","spec":"tally","depends_on":[1,2,3],"kind":"verify","pattern_config":"{\"vote\":\"majority\"}"},
            {"title":"Deploy","spec":"ship it","depends_on":[4],"kind":"agent"}
        ]}"#;
        let dag = parse_plan(raw, "build, verify, deploy");
        assert_eq!(dag.tasks.len(), 6);
        // The three skeptics are plain agent tasks depending on Build.
        for i in 1..=3 {
            assert_eq!(dag.tasks[i].kind, "agent", "skeptic {i} is an agent task");
            assert_eq!(dag.tasks[i].depends_on, vec![0]);
        }
        // The aggregator is `verify`, depends on all three skeptics, carries the
        // vote policy in pattern_config.
        let gate = &dag.tasks[4];
        assert_eq!(gate.kind, "verify");
        assert_eq!(gate.depends_on, vec![1, 2, 3]);
        assert_eq!(gate.pattern_config.as_deref(), Some("{\"vote\":\"majority\"}"));
        // Downstream gates on the verify task.
        assert_eq!(dag.tasks[5].depends_on, vec![4]);
    }

    // An unknown kind stays as-is here (parse_plan does not normalize kinds); the
    // engine is what treats anything other than the known kinds as agent. This
    // pins the fail-soft contract (parse keeps the string, no error).
    #[test]
    fn parse_plan_keeps_unknown_kind_verbatim_fail_soft() {
        let raw = r#"{"tasks":[{"title":"X","spec":"do X","depends_on":[],"kind":"totally-unknown"}]}"#;
        let dag = parse_plan(raw, "goal");
        assert_eq!(dag.tasks.len(), 1);
        assert_eq!(dag.tasks[0].kind, "totally-unknown", "parse keeps the kind verbatim (fail-soft)");
    }

    // ZERO-REGRESSION: a legacy plan WITHOUT any `kind` field parses with every
    // task defaulting to "agent" — the current single-agent behavior is unchanged
    // for any pre-023 plan.
    #[test]
    fn parse_plan_legacy_without_kind_defaults_all_agent() {
        let raw = r#"{"tasks":[
            {"title":"Research","spec":"find sources","depends_on":[],"member_index":0},
            {"title":"Write","spec":"synthesize","depends_on":[0],"member_index":1}
        ]}"#;
        let dag = parse_plan(raw, "Research and write a report");
        assert_eq!(dag.tasks.len(), 2);
        for t in &dag.tasks {
            assert_eq!(t.kind, "agent", "missing kind must default to agent (zero regression)");
            assert!(t.pattern_config.is_none());
        }
    }

    // The fallback DAG (planner output unparseable) is an `agent` task — patterns
    // never appear on the safety fallback.
    #[test]
    fn fallback_dag_task_is_agent_kind() {
        let dag = parse_plan("not json at all", "Build a thing");
        assert_eq!(dag.tasks.len(), 1);
        assert_eq!(dag.tasks[0].kind, "agent", "fallback task must be a plain agent task");
        assert!(dag.tasks[0].pattern_config.is_none());
    }

    // The system prompt must TEACH the synthesis kind + the fan-out grouping
    // convention, otherwise the lead model never emits them. Assert both the
    // schema mentions `kind`/`pattern_config` and the rules name synthesis + group.
    #[test]
    fn plan_system_teaches_synthesis_and_fanout() {
        assert!(PLAN_SYSTEM.contains("\"kind\""), "schema must mention kind: {PLAN_SYSTEM}");
        assert!(
            PLAN_SYSTEM.contains("\"pattern_config\""),
            "schema must mention pattern_config: {PLAN_SYSTEM}"
        );
        assert!(PLAN_SYSTEM.contains("synthesis"), "rules must teach synthesis: {PLAN_SYSTEM}");
        assert!(
            PLAN_SYSTEM.contains("FAN-OUT") || PLAN_SYSTEM.contains("fan-out"),
            "rules must teach fan-out: {PLAN_SYSTEM}"
        );
        assert!(PLAN_SYSTEM.contains("group"), "rules must teach the group tag: {PLAN_SYSTEM}");
    }

    // UC-1b: the system prompt must TEACH the verify pattern — the `verify` kind,
    // the skeptic JSON verdict shape, and the vote policies — otherwise the lead
    // model never emits a verify gate.
    #[test]
    fn plan_system_teaches_verify_pattern() {
        assert!(PLAN_SYSTEM.contains("verify"), "rules must teach the verify kind: {PLAN_SYSTEM}");
        assert!(
            PLAN_SYSTEM.contains("SKEPTIC") || PLAN_SYSTEM.contains("skeptic"),
            "rules must mention skeptic tasks: {PLAN_SYSTEM}"
        );
        assert!(PLAN_SYSTEM.contains("\\\"pass\\\""), "rules must teach the pass verdict shape: {PLAN_SYSTEM}");
        assert!(PLAN_SYSTEM.contains("vote"), "rules must teach the vote policy: {PLAN_SYSTEM}");
        for kw in ["majority", "unanimous", "threshold"] {
            assert!(PLAN_SYSTEM.contains(kw), "rules must teach vote policy '{kw}': {PLAN_SYSTEM}");
        }
    }

    // UC-1c: the system prompt must TEACH the judge pattern — the `judge` kind,
    // the per-candidate `scores` ballot shape, and the mean/borda aggregate
    // policies — otherwise the lead model never emits a judge contest.
    #[test]
    fn plan_system_teaches_judge_pattern() {
        assert!(PLAN_SYSTEM.contains("judge"), "rules must teach the judge kind: {PLAN_SYSTEM}");
        assert!(
            PLAN_SYSTEM.contains("CANDIDATE") || PLAN_SYSTEM.contains("candidate"),
            "rules must mention candidate tasks: {PLAN_SYSTEM}"
        );
        assert!(PLAN_SYSTEM.contains("\\\"scores\\\""), "rules must teach the scores ballot shape: {PLAN_SYSTEM}");
        assert!(PLAN_SYSTEM.contains("aggregate"), "rules must teach the aggregate policy: {PLAN_SYSTEM}");
        for kw in ["mean", "borda"] {
            assert!(PLAN_SYSTEM.contains(kw), "rules must teach aggregate policy '{kw}': {PLAN_SYSTEM}");
        }
    }

    // UC-1d: the system prompt must TEACH the loop pattern — the `loop` kind, the
    // REQUIRED `max_iter` hard cap, and the three stop kinds (max_iter/predicate/
    // dry) — otherwise the lead model never emits a bounded loop.
    #[test]
    fn plan_system_teaches_loop_pattern() {
        assert!(PLAN_SYSTEM.contains("loop"), "rules must teach the loop kind: {PLAN_SYSTEM}");
        assert!(PLAN_SYSTEM.contains("max_iter"), "rules must teach the hard cap: {PLAN_SYSTEM}");
        assert!(
            PLAN_SYSTEM.contains("BODY") || PLAN_SYSTEM.contains("body"),
            "rules must mention the body task: {PLAN_SYSTEM}"
        );
        assert!(PLAN_SYSTEM.contains("\\\"stop\\\""), "rules must teach the stop criterion: {PLAN_SYSTEM}");
        for kw in ["predicate", "dry", "quiet_rounds", "done_marker"] {
            assert!(PLAN_SYSTEM.contains(kw), "rules must teach loop stop kw '{kw}': {PLAN_SYSTEM}");
        }
    }

    // UC-1d: a loop plan — a BODY agent task + a `loop` controller depending only
    // on the body, carrying max_iter + a stop criterion in pattern_config, plus a
    // downstream task gated on the LOOP (not the body) — parses, and the loop kind
    // + config round-trip. parse_plan stays fail-soft (kept verbatim; the engine
    // recognizes "loop").
    #[test]
    fn parse_plan_accepts_loop_body_controller_and_downstream() {
        let raw = r#"{"tasks":[
            {"title":"Refine","spec":"improve the draft one round; emit DONE when finished","depends_on":[],"kind":"agent"},
            {"title":"Loop","spec":"iterate","depends_on":[0],"kind":"loop","pattern_config":"{\"max_iter\":5,\"stop\":{\"kind\":\"predicate\",\"done_marker\":\"DONE\"}}"},
            {"title":"Publish","spec":"publish the refined draft","depends_on":[1],"kind":"agent"}
        ]}"#;
        let dag = parse_plan(raw, "iteratively refine then publish");
        assert_eq!(dag.tasks.len(), 3);
        // The body is a plain agent task with no deps.
        assert_eq!(dag.tasks[0].kind, "agent");
        assert!(dag.tasks[0].depends_on.is_empty());
        // The controller is `loop`, depends ONLY on the body, carries the config.
        let ctrl = &dag.tasks[1];
        assert_eq!(ctrl.kind, "loop");
        assert_eq!(ctrl.depends_on, vec![0], "loop depends only on the body");
        assert_eq!(
            ctrl.pattern_config.as_deref(),
            Some("{\"max_iter\":5,\"stop\":{\"kind\":\"predicate\",\"done_marker\":\"DONE\"}}")
        );
        // Downstream gates on the LOOP controller, not the body.
        assert_eq!(dag.tasks[2].depends_on, vec![1], "downstream waits for the loop, not the body");
    }

    // UC-1c: a judge plan — M candidate agent tasks (a fan-out group), N judge
    // agent tasks each depending on ALL M candidates, and one `judge` aggregator
    // depending on all N judges with an aggregate policy in pattern_config —
    // parses, and the judge kind + aggregate policy round-trip. parse_plan stays
    // fail-soft for the kind (kept as-is; the engine recognizes "judge").
    #[test]
    fn parse_plan_accepts_judge_candidates_judges_and_aggregator() {
        let raw = r#"{"tasks":[
            {"title":"Candidate A","spec":"design approach A","depends_on":[],"kind":"agent","pattern_config":"{\"group\":\"candidates\"}"},
            {"title":"Candidate B","spec":"design approach B","depends_on":[],"kind":"agent","pattern_config":"{\"group\":\"candidates\"}"},
            {"title":"Candidate C","spec":"design approach C","depends_on":[],"kind":"agent","pattern_config":"{\"group\":\"candidates\"}"},
            {"title":"Judge 1","spec":"score every candidate; output {\"scores\":[..]}","depends_on":[0,1,2],"kind":"agent"},
            {"title":"Judge 2","spec":"score every candidate; output {\"scores\":[..]}","depends_on":[0,1,2],"kind":"agent"},
            {"title":"Judge 3","spec":"score every candidate; output {\"scores\":[..]}","depends_on":[0,1,2],"kind":"agent"},
            {"title":"Pick","spec":"aggregate ballots","depends_on":[3,4,5],"kind":"judge","pattern_config":"{\"aggregate\":\"borda\"}"}
        ]}"#;
        let dag = parse_plan(raw, "pick the best design");
        assert_eq!(dag.tasks.len(), 7);
        // The three candidates are plain agent tasks sharing the fan-out group.
        for i in 0..=2 {
            assert_eq!(dag.tasks[i].kind, "agent", "candidate {i} is an agent task");
            assert!(dag.tasks[i].depends_on.is_empty(), "candidates are independent");
        }
        // The three judges are agent tasks depending on ALL candidates.
        for i in 3..=5 {
            assert_eq!(dag.tasks[i].kind, "agent", "judge {i} is an agent task");
            assert_eq!(dag.tasks[i].depends_on, vec![0, 1, 2], "judge {i} scores all candidates");
        }
        // The aggregator is `judge`, depends on all three judges, carries the
        // aggregate policy in pattern_config.
        let pick = &dag.tasks[6];
        assert_eq!(pick.kind, "judge");
        assert_eq!(pick.depends_on, vec![3, 4, 5]);
        assert_eq!(pick.pattern_config.as_deref(), Some("{\"aggregate\":\"borda\"}"));
    }

    #[test]
    fn parse_plan_extracts_json_wrapped_in_prose() {
        let raw = "Sure! Here is the plan you asked for:\n\n\
            {\"tasks\":[{\"title\":\"Alpha\",\"spec\":\"step\",\"depends_on\":[]}]}\n\n\
            Let me know if you'd like changes.";
        let dag = parse_plan(raw, "goal");
        assert_eq!(dag.tasks.len(), 1);
        assert_eq!(dag.tasks[0].title, "Alpha");
    }

    #[test]
    fn parse_plan_handles_braces_inside_strings() {
        // A literal "}" inside a string value must not prematurely close the object.
        let raw = r#"{"tasks":[{"title":"Use {braces}","spec":"emit a } char","depends_on":[]}]}"#;
        let dag = parse_plan(raw, "goal");
        assert_eq!(dag.tasks.len(), 1);
        assert_eq!(dag.tasks[0].title, "Use {braces}");
        assert_eq!(dag.tasks[0].spec, "emit a } char");
    }

    #[test]
    fn parse_plan_falls_back_on_garbage() {
        let dag = parse_plan("I'm sorry, I cannot help with that.", "Build a rocket");
        assert_eq!(dag.tasks.len(), 1);
        assert_eq!(dag.tasks[0].title, "Build a rocket");
        assert_eq!(dag.tasks[0].spec, "Build a rocket");
        assert_eq!(dag.tasks[0].member_index, Some(0));
        assert!(dag.tasks[0].depends_on.is_empty());
        assert_eq!(
            dag.tasks[0].rationale.as_deref(),
            Some("fallback: planner output unparseable")
        );
    }

    #[test]
    fn parse_plan_falls_back_on_empty_tasks() {
        let dag = parse_plan(r#"{"tasks":[]}"#, "Some goal");
        assert_eq!(dag.tasks.len(), 1, "empty tasks must degrade to fallback");
        assert_eq!(dag.tasks[0].title, "Some goal");
    }

    #[test]
    fn parse_plan_falls_back_on_malformed_json() {
        // Unterminated object → no balanced match → fallback.
        let dag = parse_plan(r#"{"tasks":[{"title":"x" "#, "Goal text");
        assert_eq!(dag.tasks.len(), 1);
        assert_eq!(dag.tasks[0].spec, "Goal text");
    }

    #[test]
    fn parse_plan_truncates_long_goal_title() {
        let goal = "x".repeat(200);
        let dag = parse_plan("not json", &goal);
        // 60 chars + ellipsis.
        assert_eq!(dag.tasks[0].title.chars().count(), FALLBACK_TITLE_LEN + 1);
        assert!(dag.tasks[0].title.ends_with('…'));
        // spec keeps the full goal.
        assert_eq!(dag.tasks[0].spec, goal);
    }

    #[test]
    fn truncate_title_is_cjk_safe() {
        let goal = "目标".repeat(50); // 100 CJK chars
        let title = truncate_title(&goal);
        assert_eq!(title.chars().count(), FALLBACK_TITLE_LEN + 1);
        assert!(title.ends_with('…'));
    }

    #[test]
    fn build_plan_user_prompt_includes_goal_and_members() {
        let member = FleetMember {
            id: "fm_1".to_string(),
            agent_id: "agent_research".to_string(),
            provider_id: None,
            model: None,
            role_hint: Some("researcher".to_string()),
            capability_profile: Some(nomifun_api_types::CapabilityProfile {
                strengths: vec!["search".to_string(), "synthesis".to_string()],
                modalities: vec!["text".to_string()],
                tools: true,
                reasoning: "high".to_string(),
                cost_tier: "premium".to_string(),
                speed_tier: "medium".to_string(),
            }),
            constraints: None,
            sort_order: 0,
            description: None,
            system_prompt: None,
            enabled_skills: Vec::new(),
            disabled_builtin_skills: Vec::new(),
        };
        let prompt = build_plan_user_prompt("Research X", &[member], &DescriptionMap::new());
        assert!(prompt.contains("Research X"));
        assert!(prompt.contains("0. agent_research"));
        assert!(prompt.contains("role=researcher"));
        assert!(prompt.contains("search/synthesis"));
        // No description available for this member → desc column is the "-" sentinel.
        assert!(prompt.contains("desc=-"), "missing-description members get desc=-: {prompt}");
    }

    #[test]
    fn build_plan_user_prompt_handles_no_members() {
        let prompt = build_plan_user_prompt("Solo goal", &[], &DescriptionMap::new());
        assert!(prompt.contains("Solo goal"));
        assert!(prompt.contains("none"));
    }

    // (P5 Task 1, d) The planner must be INSTRUCTED to emit a short Chinese role
    // per task. The instruction lives in the system prompt's JSON schema + rules;
    // assert both the `role` key in the schema and a rule naming example roles, so
    // the LLM actually produces it (otherwise nothing precipitates downstream).
    #[test]
    fn plan_system_instructs_role_per_task() {
        // The JSON shape the model is told to return includes "role".
        assert!(
            PLAN_SYSTEM.contains("\"role\""),
            "PLAN_SYSTEM JSON schema must include the role field: {PLAN_SYSTEM}"
        );
        // A rule names short Chinese example roles so the model emits sensible ones.
        for kw in ["规划", "前端", "后端", "测试", "设计"] {
            assert!(
                PLAN_SYSTEM.contains(kw),
                "PLAN_SYSTEM should mention example role '{kw}': {PLAN_SYSTEM}"
            );
        }
    }

    // (a) build_plan_user_prompt surfaces a member's model description in the
    // desc= column when the (provider_id, model) → description map carries one,
    // so the planner can read it and pick the best-matching model.
    #[test]
    fn build_plan_user_prompt_includes_model_description() {
        let member = member_with(Some("prov_x"), Some("model-x"));
        let mut descriptions = DescriptionMap::new();
        descriptions.insert(
            ("prov_x".to_string(), "model-x".to_string()),
            "擅长前端与可视化".to_string(),
        );
        let prompt = build_plan_user_prompt("Build a UI", &[member], &descriptions);
        assert!(
            prompt.contains("desc=擅长前端与可视化"),
            "description must surface in the desc= column: {prompt}"
        );
    }

    // (Change 3) `member.description` is the PRIMARY desc source (Task 2 fills it
    // for assistant-backed and decorated bare members). It is shown even when the
    // P3 provider-description map has no entry for the member.
    #[test]
    fn build_plan_user_prompt_prefers_member_description() {
        let mut member = member_with(Some("prov_x"), Some("model-x"));
        member.description = Some("研究型助手，擅长检索与综述".to_string());
        // Empty P3 map — member.description alone must drive the desc= column.
        let prompt = build_plan_user_prompt("Research X", &[member], &DescriptionMap::new());
        assert!(
            prompt.contains("desc=研究型助手，擅长检索与综述"),
            "member.description must surface as the desc= column: {prompt}"
        );
    }

    // member.description WINS over the P3 provider-description map when both are
    // present (the member snapshot is the authoritative source now).
    #[test]
    fn build_plan_user_prompt_member_description_overrides_provider_map() {
        let mut member = member_with(Some("prov_x"), Some("model-x"));
        member.description = Some("助手自述描述".to_string());
        let mut descriptions = DescriptionMap::new();
        descriptions.insert(
            ("prov_x".to_string(), "model-x".to_string()),
            "模型卡描述（应被覆盖）".to_string(),
        );
        let prompt = build_plan_user_prompt("goal", &[member], &descriptions);
        assert!(prompt.contains("desc=助手自述描述"), "member.description wins: {prompt}");
        assert!(
            !prompt.contains("模型卡描述"),
            "provider-map desc must not appear when member.description is set: {prompt}"
        );
    }

    // A blank member.description falls back to the P3 provider-description map
    // (so bare members still get the model-card description via the fallback).
    #[test]
    fn build_plan_user_prompt_blank_member_description_falls_back_to_provider_map() {
        let mut member = member_with(Some("prov_x"), Some("model-x"));
        member.description = Some("   ".to_string()); // whitespace-only → ignored
        let mut descriptions = DescriptionMap::new();
        descriptions.insert(
            ("prov_x".to_string(), "model-x".to_string()),
            "模型卡描述".to_string(),
        );
        let prompt = build_plan_user_prompt("goal", &[member], &descriptions);
        assert!(
            prompt.contains("desc=模型卡描述"),
            "blank member.description must fall back to the provider map: {prompt}"
        );
    }

    /// Build a minimal `FleetMember` carrying the given provider/model.
    fn member_with(provider_id: Option<&str>, model: Option<&str>) -> FleetMember {
        FleetMember {
            id: "fm".to_string(),
            agent_id: "agent".to_string(),
            provider_id: provider_id.map(str::to_string),
            model: model.map(str::to_string),
            role_hint: None,
            capability_profile: None,
            constraints: None,
            sort_order: 0,
            description: None,
            system_prompt: None,
            enabled_skills: Vec::new(),
            disabled_builtin_skills: Vec::new(),
        }
    }

    #[test]
    fn pick_lead_picks_first_member_with_provider_and_model() {
        let fallback = ProviderWithModel {
            provider_id: String::new(),
            model: String::new(),
            use_model: None,
        };
        // members[0] lacks a model; members[1] is fully specified → pick [1].
        let members = vec![
            member_with(Some("prov_a"), None),
            member_with(Some("prov_b"), Some("model_b")),
        ];
        let lead = pick_lead(&members, &fallback);
        assert_eq!(lead.provider_id, "prov_b");
        assert_eq!(lead.model, "model_b");
        assert_eq!(lead.use_model.as_deref(), Some("model_b"));
    }

    #[test]
    fn pick_lead_skips_empty_string_provider() {
        let fallback = ProviderWithModel {
            provider_id: String::new(),
            model: String::new(),
            use_model: None,
        };
        // members[0] has an EMPTY provider_id → skipped; members[1] qualifies.
        let members = vec![
            member_with(Some(""), Some("model_x")),
            member_with(Some("prov_real"), Some("model_real")),
        ];
        let lead = pick_lead(&members, &fallback);
        assert_eq!(lead.provider_id, "prov_real");
        assert_eq!(lead.model, "model_real");
        assert_eq!(lead.use_model.as_deref(), Some("model_real"));
    }

    #[test]
    fn pick_lead_falls_back_when_no_member_qualifies() {
        let fallback = ProviderWithModel {
            provider_id: "fallback_prov".to_string(),
            model: "fallback_model".to_string(),
            use_model: Some("fallback_use".to_string()),
        };
        // No member carries both provider+model → return the fallback as-is.
        let members = vec![member_with(None, Some("m")), member_with(Some("p"), None), member_with(Some(""), Some(""))];
        let lead = pick_lead(&members, &fallback);
        assert_eq!(lead.provider_id, "fallback_prov");
        assert_eq!(lead.model, "fallback_model");
        assert_eq!(lead.use_model.as_deref(), Some("fallback_use"));
    }

    /// Build a minimal `Provider` row carrying the given `model_descriptions`
    /// JSON (the only field `build_description_map` reads, besides `id`).
    fn provider_with_descriptions(id: &str, model_descriptions: Option<&str>) -> Provider {
        Provider {
            id: id.to_string(),
            platform: "openai".to_string(),
            name: "p".to_string(),
            base_url: String::new(),
            api_key_encrypted: String::new(),
            models: "[]".to_string(),
            enabled: true,
            capabilities: "[]".to_string(),
            context_limit: None,
            model_protocols: None,
            model_descriptions: model_descriptions.map(str::to_string),
            model_enabled: None,
            model_health: None,
            bedrock_config: None,
            is_full_url: false,
            created_at: 0,
            updated_at: 0,
        }
    }

    // build_description_map decodes each provider's model_descriptions JSON and
    // keys the result by (provider_id, model) for the members that reference it.
    #[test]
    fn build_description_map_keys_by_provider_and_model() {
        let providers = vec![provider_with_descriptions(
            "prov_a",
            Some(r#"{"model-a":"擅长前端","model-b":"擅长后端"}"#),
        )];
        let members = vec![
            member_with(Some("prov_a"), Some("model-a")),
            member_with(Some("prov_a"), Some("model-b")),
        ];
        let map = build_description_map(&providers, &members);
        assert_eq!(
            map.get(&("prov_a".to_string(), "model-a".to_string())).map(String::as_str),
            Some("擅长前端")
        );
        assert_eq!(
            map.get(&("prov_a".to_string(), "model-b".to_string())).map(String::as_str),
            Some("擅长后端")
        );
    }

    // An unset model_descriptions (Task 1 stores the default as `Some("{}")`) and
    // an absent model entry both yield "no description" (no map entry) — not an error.
    #[test]
    fn build_description_map_treats_empty_object_as_no_description() {
        let providers = vec![
            provider_with_descriptions("prov_empty", Some("{}")),
            provider_with_descriptions("prov_partial", Some(r#"{"other-model":"x"}"#)),
        ];
        let members = vec![
            member_with(Some("prov_empty"), Some("model-a")),
            member_with(Some("prov_partial"), Some("model-a")),
        ];
        let map = build_description_map(&providers, &members);
        assert!(map.is_empty(), "no member matched a description entry: {map:?}");
    }

    // A blank description string is dropped (treated as "no description"), and a
    // malformed model_descriptions JSON is fail-soft (no entries, no panic/error).
    #[test]
    fn build_description_map_is_fail_soft_on_bad_json_and_blank() {
        let providers = vec![
            provider_with_descriptions("prov_bad", Some("not json at all")),
            provider_with_descriptions("prov_blank", Some(r#"{"model-a":"   "}"#)),
            provider_with_descriptions("prov_none", None),
        ];
        let members = vec![
            member_with(Some("prov_bad"), Some("model-a")),
            member_with(Some("prov_blank"), Some("model-a")),
            member_with(Some("prov_none"), Some("model-a")),
        ];
        let map = build_description_map(&providers, &members);
        assert!(map.is_empty(), "bad/blank/absent descriptions yield no entries: {map:?}");
    }
}
