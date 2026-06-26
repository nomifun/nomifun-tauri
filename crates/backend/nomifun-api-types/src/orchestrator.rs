//! Orchestration ("智能编排") request/response DTOs: fleets, fleet members,
//! capability profiles, and orchestration workspaces. Plain serde only (no
//! ts-rs in P0).

use serde::{Deserialize, Serialize};

use crate::webhook::double_option;

/// A fleet (编队) as returned to clients: a named group of agent members with an
/// optional parallelism cap.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Fleet {
    pub id: String,
    pub name: String,
    pub description: Option<String>,
    pub max_parallel: Option<i64>,
    pub members: Vec<FleetMember>,
    pub created_at: i64,
    pub updated_at: i64,
}

/// One member of a fleet: an agent reference plus its routing hints, capability
/// profile, and constraints.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FleetMember {
    pub id: String,
    pub agent_id: String,
    pub provider_id: Option<String>,
    pub model: Option<String>,
    pub role_hint: Option<String>,
    pub capability_profile: Option<CapabilityProfile>,
    pub constraints: Option<MemberConstraints>,
    pub sort_order: i64,
}

/// Declarative capability profile used to route tasks to a member.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CapabilityProfile {
    pub strengths: Vec<String>,
    pub modalities: Vec<String>,
    pub tools: bool,
    pub reasoning: String,
    pub cost_tier: String,
    pub speed_tier: String,
}

/// Per-member runtime constraints applied by the orchestrator.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemberConstraints {
    pub max_concurrency: Option<i64>,
    pub cost_tier: Option<String>,
    pub allowed_task_kinds: Option<Vec<String>>,
}

/// An orchestration workspace: a named scope with an optional default fleet and
/// working directory.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrchWorkspace {
    pub id: String,
    pub name: String,
    pub default_fleet_id: Option<String>,
    pub workspace_dir: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

/// Create a fleet with an initial set of members.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateFleetRequest {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub max_parallel: Option<i64>,
    #[serde(default)]
    pub members: Vec<FleetMemberInput>,
}

/// Partial update of a fleet. The `Option<Option<T>>` patch fields distinguish
/// "absent" (keep current) from explicit `null` (clear) via [`double_option`].
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UpdateFleetRequest {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default, deserialize_with = "double_option")]
    pub description: Option<Option<String>>,
    #[serde(default, deserialize_with = "double_option")]
    pub max_parallel: Option<Option<i64>>,
    #[serde(default)]
    pub members: Option<Vec<FleetMemberInput>>,
}

/// Member payload used when creating or replacing fleet members.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FleetMemberInput {
    pub agent_id: String,
    #[serde(default)]
    pub provider_id: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub role_hint: Option<String>,
    #[serde(default)]
    pub capability_profile: Option<CapabilityProfile>,
    #[serde(default)]
    pub constraints: Option<MemberConstraints>,
    #[serde(default)]
    pub sort_order: Option<i64>,
}

/// Create an orchestration workspace.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateWorkspaceRequest {
    pub name: String,
    #[serde(default)]
    pub default_fleet_id: Option<String>,
    #[serde(default)]
    pub workspace_dir: Option<String>,
}

/// Partial update of an orchestration workspace. `default_fleet_id` uses
/// [`double_option`]: absent keeps the current binding, explicit `null` clears it.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UpdateWorkspaceRequest {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default, deserialize_with = "double_option")]
    pub default_fleet_id: Option<Option<String>>,
}

// ---------------------------------------------------------------------------
// Run / Task / Plan DTOs
//
// `status` (run + task) and `source` stay as plain `String` on the wire to match
// the TEXT columns in the Row structs (`OrchRunRow`, `OrchRunTaskRow`,
// `OrchAssignmentRow`) and the project-wide convention (acp/cron/team/... all use
// `status: String`). The service layer (Task 6) maps Row↔DTO 1:1, decoding the
// JSON-as-TEXT columns (`task_profile`, `output_files`) into the structured
// shapes below.
// ---------------------------------------------------------------------------

/// An orchestration run as returned to clients: a goal plus its decomposed task
/// DAG. Mirrors [`OrchRunRow`](../../nomifun_db) minus `user_id`/`fleet_snapshot`/
/// `forked_from` (internal columns not surfaced on the wire).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Run {
    pub id: String,
    pub workspace_id: String,
    pub goal: String,
    pub autonomy: String,
    pub max_parallel: Option<i64>,
    pub status: String,
    pub summary: Option<String>,
    /// Lead/coordinator worker conversation — local `conversations.id` INTEGER.
    pub lead_conv_id: Option<i64>,
    pub total_tokens: Option<i64>,
    pub created_at: i64,
    pub updated_at: i64,
}

/// One decomposed task within a run. Mirrors `OrchRunTaskRow` with the
/// JSON-as-TEXT columns decoded: `task_profile` (JSON → [`TaskProfile`]) and
/// `output_files` (JSON → `Vec<String>`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunTask {
    pub id: String,
    pub run_id: String,
    pub title: String,
    pub spec: String,
    pub task_profile: Option<TaskProfile>,
    pub status: String,
    /// Worker conversation — local `conversations.id` INTEGER.
    pub conversation_id: Option<i64>,
    pub output_summary: Option<String>,
    pub output_files: Vec<String>,
    pub attempt: i64,
    pub tokens: Option<i64>,
    pub graph_x: Option<f64>,
    pub graph_y: Option<f64>,
}

/// A blocker→blocked edge in the task DAG. Mirrors `OrchRunTaskDepRow`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunTaskDep {
    pub blocker_task_id: String,
    pub blocked_task_id: String,
}

/// A member assigned to a task (auto-scored or locked). Mirrors
/// `OrchAssignmentRow` with `locked` decoded from the INTEGER column to `bool`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Assignment {
    pub id: String,
    pub task_id: String,
    pub member_id: String,
    pub score: Option<f64>,
    pub rationale: Option<String>,
    pub source: String,
    pub locked: bool,
}

/// Structured capability requirements of a task, used to route it to a member.
/// Stored as JSON in the `orch_run_tasks.task_profile` TEXT column.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskProfile {
    pub kind: String,
    pub needs_vision: bool,
    pub needs_long_context: bool,
    pub needs_high_reasoning: bool,
    pub bulk: bool,
}

/// A run plus its full task DAG: tasks, dependency edges, and assignments.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunDetail {
    pub run: Run,
    pub tasks: Vec<RunTask>,
    pub deps: Vec<RunTaskDep>,
    pub assignments: Vec<Assignment>,
}

/// Create (and kick off) an orchestration run within a workspace against a fleet.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateRunRequest {
    pub workspace_id: String,
    pub goal: String,
    pub fleet_id: String,
    #[serde(default)]
    pub autonomy: Option<String>,
    #[serde(default)]
    pub max_parallel: Option<i64>,
}

/// One planned task in a [`PlannedDag`]. Produced by the PlanProducer (主管规划)
/// and accepted as the `nomi_run_plan` gateway input. Dependencies and member
/// assignment reference other tasks/members by 0-based index because no ids exist
/// yet at planning time.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlannedTask {
    pub title: String,
    pub spec: String,
    #[serde(default)]
    pub task_profile: Option<TaskProfile>,
    /// 0-based indices into [`PlannedDag::tasks`] this task depends on.
    #[serde(default)]
    pub depends_on: Vec<usize>,
    /// 0-based index into the fleet's members, if pre-assigned.
    #[serde(default)]
    pub member_index: Option<usize>,
    #[serde(default)]
    pub rationale: Option<String>,
}

/// The planned task DAG: the PlanProducer output / `nomi_run_plan` input.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlannedDag {
    pub tasks: Vec<PlannedTask>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_fleet_request_round_trips() {
        let req = CreateFleetRequest {
            name: "research-fleet".to_string(),
            description: Some("multi-agent research".to_string()),
            max_parallel: Some(3),
            members: vec![FleetMemberInput {
                agent_id: "agent_abc".to_string(),
                provider_id: Some("provider_xyz".to_string()),
                model: Some("claude-opus".to_string()),
                role_hint: Some("lead".to_string()),
                capability_profile: Some(CapabilityProfile {
                    strengths: vec!["analysis".to_string(), "writing".to_string()],
                    modalities: vec!["text".to_string()],
                    tools: true,
                    reasoning: "high".to_string(),
                    cost_tier: "premium".to_string(),
                    speed_tier: "medium".to_string(),
                }),
                constraints: Some(MemberConstraints {
                    max_concurrency: Some(2),
                    cost_tier: Some("premium".to_string()),
                    allowed_task_kinds: Some(vec!["research".to_string()]),
                }),
                sort_order: Some(0),
            }],
        };

        let json = serde_json::to_string(&req).expect("serialize");
        let back: CreateFleetRequest = serde_json::from_str(&json).expect("deserialize");

        assert_eq!(back.name, "research-fleet");
        assert_eq!(back.description.as_deref(), Some("multi-agent research"));
        assert_eq!(back.max_parallel, Some(3));
        assert_eq!(back.members.len(), 1);

        let member = &back.members[0];
        assert_eq!(member.agent_id, "agent_abc");
        assert_eq!(member.provider_id.as_deref(), Some("provider_xyz"));
        assert_eq!(member.model.as_deref(), Some("claude-opus"));
        assert_eq!(member.role_hint.as_deref(), Some("lead"));
        assert_eq!(member.sort_order, Some(0));

        let profile = member.capability_profile.as_ref().expect("profile");
        assert_eq!(profile.strengths, vec!["analysis", "writing"]);
        assert_eq!(profile.modalities, vec!["text"]);
        assert!(profile.tools);
        assert_eq!(profile.reasoning, "high");
        assert_eq!(profile.cost_tier, "premium");
        assert_eq!(profile.speed_tier, "medium");

        let constraints = member.constraints.as_ref().expect("constraints");
        assert_eq!(constraints.max_concurrency, Some(2));
        assert_eq!(constraints.cost_tier.as_deref(), Some("premium"));
        assert_eq!(
            constraints.allowed_task_kinds.as_ref().map(|v| v.as_slice()),
            Some(["research".to_string()].as_slice())
        );
    }

    #[test]
    fn update_fleet_request_distinguishes_clear_from_absent() {
        // Explicit null => clear (present-as-null): Some(None).
        let clear: UpdateFleetRequest =
            serde_json::from_str(r#"{"description": null}"#).expect("deserialize clear");
        assert_eq!(clear.description, Some(None), "explicit null must be Some(None)");
        // Other fields absent => None (keep).
        assert_eq!(clear.name, None);
        assert_eq!(clear.max_parallel, None);
        assert!(clear.members.is_none());

        // Key absent => keep current: None.
        let keep: UpdateFleetRequest = serde_json::from_str(r#"{}"#).expect("deserialize keep");
        assert_eq!(keep.description, None, "absent key must be None");

        // Explicit value => set: Some(Some(v)).
        let set: UpdateFleetRequest =
            serde_json::from_str(r#"{"description": "new"}"#).expect("deserialize set");
        assert_eq!(set.description, Some(Some("new".to_string())));

        // max_parallel patch semantics.
        let clear_mp: UpdateFleetRequest =
            serde_json::from_str(r#"{"max_parallel": null}"#).expect("deserialize clear mp");
        assert_eq!(clear_mp.max_parallel, Some(None));
        let set_mp: UpdateFleetRequest =
            serde_json::from_str(r#"{"max_parallel": 5}"#).expect("deserialize set mp");
        assert_eq!(set_mp.max_parallel, Some(Some(5)));
    }

    #[test]
    fn update_workspace_request_clears_default_fleet() {
        let clear: UpdateWorkspaceRequest =
            serde_json::from_str(r#"{"default_fleet_id": null}"#).expect("deserialize clear");
        assert_eq!(clear.default_fleet_id, Some(None));

        let keep: UpdateWorkspaceRequest =
            serde_json::from_str(r#"{}"#).expect("deserialize keep");
        assert_eq!(keep.default_fleet_id, None);
    }

    #[test]
    fn planned_dag_round_trips() {
        let dag = PlannedDag {
            tasks: vec![
                PlannedTask {
                    title: "Gather sources".to_string(),
                    spec: "Search the web for primary sources.".to_string(),
                    task_profile: Some(TaskProfile {
                        kind: "research".to_string(),
                        needs_vision: false,
                        needs_long_context: true,
                        needs_high_reasoning: false,
                        bulk: true,
                    }),
                    depends_on: vec![],
                    member_index: None,
                    rationale: Some("breadth-first collection".to_string()),
                },
                PlannedTask {
                    title: "Synthesize report".to_string(),
                    spec: "Write a cited synthesis from the gathered sources.".to_string(),
                    task_profile: None,
                    depends_on: vec![0],
                    member_index: Some(1),
                    rationale: None,
                },
            ],
        };

        let json = serde_json::to_string(&dag).expect("serialize");
        let back: PlannedDag = serde_json::from_str(&json).expect("deserialize");

        assert_eq!(back.tasks.len(), 2);
        assert_eq!(back.tasks[0].title, "Gather sources");
        assert!(back.tasks[0].depends_on.is_empty());
        assert_eq!(back.tasks[0].member_index, None);
        let profile = back.tasks[0].task_profile.as_ref().expect("profile");
        assert_eq!(profile.kind, "research");
        assert!(profile.needs_long_context);
        assert!(profile.bulk);
        assert!(!profile.needs_vision);

        assert_eq!(back.tasks[1].title, "Synthesize report");
        assert_eq!(back.tasks[1].depends_on, vec![0]);
        assert_eq!(back.tasks[1].member_index, Some(1));
        assert!(back.tasks[1].task_profile.is_none());
        assert_eq!(back.tasks[1].rationale, None);
    }

    #[test]
    fn create_run_request_round_trips() {
        let req = CreateRunRequest {
            workspace_id: "ws_abc".to_string(),
            goal: "Research the orchestration market.".to_string(),
            fleet_id: "fleet_xyz".to_string(),
            autonomy: Some("supervised".to_string()),
            max_parallel: Some(4),
        };

        let json = serde_json::to_string(&req).expect("serialize");
        let back: CreateRunRequest = serde_json::from_str(&json).expect("deserialize");

        assert_eq!(back.workspace_id, "ws_abc");
        assert_eq!(back.goal, "Research the orchestration market.");
        assert_eq!(back.fleet_id, "fleet_xyz");
        assert_eq!(back.autonomy.as_deref(), Some("supervised"));
        assert_eq!(back.max_parallel, Some(4));

        // Optional fields default when absent.
        let minimal: CreateRunRequest = serde_json::from_str(
            r#"{"workspace_id":"ws_1","goal":"g","fleet_id":"f_1"}"#,
        )
        .expect("deserialize minimal");
        assert_eq!(minimal.autonomy, None);
        assert_eq!(minimal.max_parallel, None);
    }

    #[test]
    fn run_detail_round_trips() {
        let detail = RunDetail {
            run: Run {
                id: "run_1".to_string(),
                workspace_id: "ws_abc".to_string(),
                goal: "Research and synthesize.".to_string(),
                autonomy: "supervised".to_string(),
                max_parallel: Some(2),
                status: "running".to_string(),
                summary: Some("in progress".to_string()),
                lead_conv_id: Some(101),
                total_tokens: Some(4242),
                created_at: 1_700_000_000_000,
                updated_at: 1_700_000_500_000,
            },
            tasks: vec![
                RunTask {
                    id: "task_1".to_string(),
                    run_id: "run_1".to_string(),
                    title: "Gather".to_string(),
                    spec: "collect".to_string(),
                    task_profile: Some(TaskProfile {
                        kind: "research".to_string(),
                        needs_vision: false,
                        needs_long_context: true,
                        needs_high_reasoning: true,
                        bulk: false,
                    }),
                    status: "done".to_string(),
                    conversation_id: Some(201),
                    output_summary: Some("found 12 sources".to_string()),
                    output_files: vec!["sources.md".to_string(), "notes.txt".to_string()],
                    attempt: 1,
                    tokens: Some(1234),
                    graph_x: Some(10.5),
                    graph_y: Some(20.0),
                },
                RunTask {
                    id: "task_2".to_string(),
                    run_id: "run_1".to_string(),
                    title: "Synthesize".to_string(),
                    spec: "write".to_string(),
                    task_profile: None,
                    status: "pending".to_string(),
                    conversation_id: None,
                    output_summary: None,
                    output_files: vec![],
                    attempt: 0,
                    tokens: None,
                    graph_x: None,
                    graph_y: None,
                },
            ],
            deps: vec![RunTaskDep {
                blocker_task_id: "task_1".to_string(),
                blocked_task_id: "task_2".to_string(),
            }],
            assignments: vec![Assignment {
                id: "asg_1".to_string(),
                task_id: "task_1".to_string(),
                member_id: "fm_1".to_string(),
                score: Some(0.87),
                rationale: Some("best at research".to_string()),
                source: "auto".to_string(),
                locked: false,
            }],
        };

        let json = serde_json::to_string(&detail).expect("serialize");
        let back: RunDetail = serde_json::from_str(&json).expect("deserialize");

        // Run.
        assert_eq!(back.run.id, "run_1");
        assert_eq!(back.run.workspace_id, "ws_abc");
        assert_eq!(back.run.autonomy, "supervised");
        assert_eq!(back.run.max_parallel, Some(2));
        assert_eq!(back.run.status, "running");
        assert_eq!(back.run.summary.as_deref(), Some("in progress"));
        assert_eq!(back.run.lead_conv_id, Some(101));
        assert_eq!(back.run.total_tokens, Some(4242));
        assert_eq!(back.run.created_at, 1_700_000_000_000);
        assert_eq!(back.run.updated_at, 1_700_000_500_000);

        // Tasks.
        assert_eq!(back.tasks.len(), 2);
        let t1 = &back.tasks[0];
        assert_eq!(t1.id, "task_1");
        assert_eq!(t1.run_id, "run_1");
        assert_eq!(t1.status, "done");
        assert_eq!(t1.conversation_id, Some(201));
        assert_eq!(t1.output_files, vec!["sources.md", "notes.txt"]);
        assert_eq!(t1.attempt, 1);
        assert_eq!(t1.tokens, Some(1234));
        assert_eq!(t1.graph_x, Some(10.5));
        assert_eq!(t1.graph_y, Some(20.0));
        let t1_profile = t1.task_profile.as_ref().expect("t1 profile");
        assert!(t1_profile.needs_high_reasoning);
        assert!(!t1_profile.bulk);

        let t2 = &back.tasks[1];
        assert_eq!(t2.id, "task_2");
        assert_eq!(t2.status, "pending");
        assert_eq!(t2.conversation_id, None);
        assert!(t2.output_files.is_empty());
        assert!(t2.task_profile.is_none());
        assert_eq!(t2.attempt, 0);

        // Deps.
        assert_eq!(back.deps.len(), 1);
        assert_eq!(back.deps[0].blocker_task_id, "task_1");
        assert_eq!(back.deps[0].blocked_task_id, "task_2");

        // Assignments.
        assert_eq!(back.assignments.len(), 1);
        let asg = &back.assignments[0];
        assert_eq!(asg.id, "asg_1");
        assert_eq!(asg.task_id, "task_1");
        assert_eq!(asg.member_id, "fm_1");
        assert_eq!(asg.score, Some(0.87));
        assert_eq!(asg.rationale.as_deref(), Some("best at research"));
        assert_eq!(asg.source, "auto");
        assert!(!asg.locked);
    }
}
