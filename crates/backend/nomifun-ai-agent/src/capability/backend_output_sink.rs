use std::collections::HashMap;
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use nomi_agent::output::{
    ArtifactContract, ArtifactExpectation, ArtifactRequirement, OutputSink, ToolMediaDelivery,
    ToolCallExecutionContext, ToolCallRetryContext, artifact_contract,
    artifact_contract_with_input, is_context_only_image_tool,
};
use nomi_types::tool::ToolImage;
use sha2::{Digest, Sha256};
use tokio::sync::broadcast;

use crate::artifact_store::{ArtifactKind, ArtifactStore, PersistedArtifact};
use crate::protocol::events::{
    AgentStatusEventData, AgentStreamEvent, ErrorEventData, FinishEventData, PlanEventData,
    StartEventData, TextEventData, ThinkingEventData, TipType, TipsEventData, ToolCallEventData,
    ToolCallRetryData, ToolCallStatus,
};

pub struct BackendOutputSink {
    event_tx: broadcast::Sender<AgentStreamEvent>,
    /// File-based memory directory for citation reflow. `None` = this session
    /// does not participate (companion sessions, or no base dir).
    distill_dir: Option<PathBuf>,
    /// Workspace-scoped verified store for binary tool outputs. A desktop
    /// session wires this unconditionally; `None` is retained for lightweight
    /// unit/companion sinks and causes media delivery to fail closed.
    artifact_store: Option<ArtifactStore>,
    artifact_workspace: Option<PathBuf>,
    /// Accumulates this turn's assistant text so the `<nomi-mem-citation>`
    /// block can be parsed at stream end. Reset on each stream start.
    turn_text: Mutex<String>,
    /// Schema-valid, committed tool calls announced to the frontend that have
    /// not yet produced a result. Unexpected termination and cancellation drain
    /// this map so no Running lifecycle can leak into a later turn.
    active_tool_calls: Mutex<HashMap<String, ActiveToolCall>>,
    /// Per-result context supplied by the engine. Pre-dispatch validation
    /// failures never enter `active_tool_calls`, so this short-lived map keeps
    /// their original args and retry identity through the legacy artifact
    /// delivery implementation.
    tool_result_contexts: Mutex<HashMap<String, ToolTerminalContext>>,
    /// Accepted-user-turn artifact obligations. Provider sub-streams and
    /// automatic continuations share this state; only the manager's accepted
    /// turn boundary begins/seals it.
    artifact_delivery_turn: Mutex<ArtifactDeliveryTurn>,
}

#[derive(Debug, Clone)]
struct ActiveToolCall {
    call_id: String,
    name: String,
    artifact_identity: String,
    args: serde_json::Value,
    input: Option<serde_json::Value>,
    contract: Option<ArtifactContract>,
    contract_error: Option<String>,
    artifact_path_baselines: ArtifactPathBaselines,
    retry: Option<ToolCallRetryData>,
}

#[derive(Debug, Clone)]
struct ToolTerminalContext {
    args: serde_json::Value,
    input: Option<serde_json::Value>,
    retry: Option<ToolCallRetryData>,
}

const MAX_DECLARED_ARTIFACT_PATHS: usize = 32;
const MAX_DECLARED_PATH_LENGTH: usize = 4096;
const MAX_ARTIFACT_OUTPUT_JSON_NODES: usize = 512;
const MAX_BASELINE_ARTIFACT_BYTES: u64 = 512 * 1024 * 1024;

#[derive(Debug, Clone, Default)]
struct ArtifactPathBaselines {
    entries: Vec<ArtifactPathBaseline>,
    errors: Vec<String>,
}

impl ArtifactPathBaselines {
    fn declares_artifact(&self) -> bool {
        !self.entries.is_empty() || !self.errors.is_empty()
    }
}

#[derive(Debug, Clone)]
struct ArtifactPathBaseline {
    path: PathBuf,
    fingerprint: ArtifactPathFingerprint,
}

#[derive(Debug, Clone)]
enum ArtifactPathFingerprint {
    Absent,
    Present { size_bytes: u64, sha256: String },
}

#[derive(Debug, Clone, Default)]
struct DeclaredArtifactPaths {
    paths: Vec<String>,
    saw_explicit_key: bool,
    errors: Vec<String>,
    resource_limit_errors: Vec<String>,
}

impl DeclaredArtifactPaths {
    fn push_error(&mut self, error: impl Into<String>) {
        let error = error.into();
        if !self.errors.iter().any(|known| known == &error) {
            self.errors.push(error);
        }
    }

    fn push_resource_limit_error(&mut self, error: impl Into<String>) {
        let error = error.into();
        if !self
            .resource_limit_errors
            .iter()
            .any(|known| known == &error)
        {
            self.resource_limit_errors.push(error);
        }
    }

    fn has_artifact_signal(&self) -> bool {
        self.saw_explicit_key || !self.paths.is_empty() || !self.errors.is_empty()
    }

    /// Resource limits protect artifact-contract parsing; they are not, by
    /// themselves, evidence that an ordinary JSON tool result is an artifact.
    /// Promote them to delivery errors only after an artifact signal or an
    /// existing contract makes this scan security-sensitive.
    fn enforce_resource_limits_if_artifact_expected(&mut self, artifact_expected: bool) {
        if !artifact_expected && !self.has_artifact_signal() {
            return;
        }
        for error in std::mem::take(&mut self.resource_limit_errors) {
            self.push_error(error);
        }
    }
}

#[derive(Debug, Default)]
struct ArtifactDeliveryTurn {
    active: bool,
    calls: HashMap<String, ArtifactCallObligation>,
}

#[derive(Debug)]
struct ArtifactCallObligation {
    tool_name: String,
    contract: ArtifactContract,
    status: ArtifactCallDeliveryStatus,
}

#[derive(Debug)]
enum ArtifactCallDeliveryStatus {
    Running,
    CompletedVerified(Vec<PersistedArtifact>),
    Failed(String),
}

fn any_artifact_contract() -> ArtifactContract {
    ArtifactContract {
        expectation: ArtifactExpectation::Any,
        requirement: ArtifactRequirement::Any,
        requested_count: None,
    }
}

fn normalized_path_key(key: &str) -> String {
    key.chars()
        .filter(|character| character.is_ascii_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

fn is_explicit_artifact_path_key(key: &str) -> bool {
    let normalized = normalized_path_key(key);
    const PREFIXES: &[&str] = &[
        "output",
        "outputs",
        "artifact",
        "artifacts",
        "result",
        "results",
        "save",
        "saves",
        "destination",
        "destinations",
    ];
    const SUFFIXES: &[&str] = &[
        "path",
        "paths",
        "file",
        "files",
        "filepath",
        "filepaths",
    ];
    PREFIXES.iter().any(|prefix| {
        normalized
            .strip_prefix(prefix)
            .is_some_and(|suffix| SUFFIXES.contains(&suffix))
    })
}

fn is_unambiguous_artifact_path_key(key: &str) -> bool {
    let normalized = normalized_path_key(key);
    const PREFIXES: &[&str] = &[
        "output",
        "outputs",
        "artifact",
        "artifacts",
        "result",
        "results",
        "save",
        "saves",
        "destination",
        "destinations",
    ];
    // Plural `*files` fields are frequently read-model history (for example
    // execution attempt `output_files`). A singular file or an explicit path
    // suffix is strong enough to recognize inside a root array item.
    const SUFFIXES: &[&str] = &["path", "paths", "file", "filepath", "filepaths"];
    PREFIXES.iter().any(|prefix| {
        normalized
            .strip_prefix(prefix)
            .is_some_and(|suffix| SUFFIXES.contains(&suffix))
    })
}

fn is_plain_path_key(key: &str) -> bool {
    matches!(normalized_path_key(key).as_str(), "path" | "paths")
}

fn is_result_scope(key: &str) -> bool {
    matches!(
        normalized_path_key(key).as_str(),
        "result" | "results" | "output" | "outputs" | "artifact" | "artifacts"
    )
}

fn is_blocked_path_scope(key: &str) -> bool {
    matches!(
        normalized_path_key(key).as_str(),
        "input"
            | "inputs"
            | "source"
            | "sources"
            | "request"
            | "requests"
            | "argument"
            | "arguments"
            | "arg"
            | "args"
            | "parameter"
            | "parameters"
    )
}

fn push_declared_path(declared: &mut DeclaredArtifactPaths, value: &str) {
    let value = value
        .trim()
        .trim_matches(|character| matches!(character, '`' | '"' | '\''));
    if value.is_empty() {
        declared.push_error("declared artifact path is empty");
        return;
    }
    if value.len() > MAX_DECLARED_PATH_LENGTH {
        declared.push_error(format!(
            "declared artifact path exceeds {MAX_DECLARED_PATH_LENGTH} bytes"
        ));
        return;
    }
    if value.chars().any(char::is_control) {
        declared.push_error("declared artifact path contains a control character");
        return;
    }
    if declared.paths.iter().any(|known| known == value) {
        return;
    }
    if declared.paths.len() >= MAX_DECLARED_ARTIFACT_PATHS {
        declared.push_error(format!(
            "artifact contract declares more than {MAX_DECLARED_ARTIFACT_PATHS} distinct paths"
        ));
        return;
    }
    declared.paths.push(value.to_owned());
}

fn collect_path_value(value: &serde_json::Value, declared: &mut DeclaredArtifactPaths) {
    match value {
        serde_json::Value::String(value) => push_declared_path(declared, value),
        serde_json::Value::Array(values) => {
            for value in values {
                if let Some(value) = value.as_str() {
                    push_declared_path(declared, value);
                } else {
                    declared.push_error(
                        "declared artifact path list contains a non-string value",
                    );
                }
            }
        }
        _ => declared.push_error("declared artifact path value is not a string or string list"),
    }
}

fn collect_json_artifact_paths(
    value: &serde_json::Value,
    declared: &mut DeclaredArtifactPaths,
    nodes: &mut usize,
    depth: usize,
    allow_plain_path: bool,
    explicit_paths_require_result_scope: bool,
    at_result_envelope: bool,
    at_root_array_item: bool,
) {
    if depth > 12 {
        declared.push_resource_limit_error("artifact contract JSON nesting exceeds 12 levels");
    }
    if *nodes >= MAX_ARTIFACT_OUTPUT_JSON_NODES {
        declared.push_resource_limit_error(format!(
            "artifact contract JSON exceeds {MAX_ARTIFACT_OUTPUT_JSON_NODES} nodes"
        ));
    } else {
        *nodes += 1;
    }

    // Continue walking object/array structure after the artifact parsing
    // budget is exhausted. This is necessary to distinguish a large ordinary
    // JSON result from a large result that contains a real explicit artifact
    // declaration after the limit. Path collection remains bounded separately.
    match value {
        serde_json::Value::Object(object) => {
            for (key, child) in object {
                if is_blocked_path_scope(key) {
                    continue;
                }
                // Output-shaped field names are common inside ordinary nested
                // domain data (for example execution attempts expose an
                // `output_files` history field). Treat them as an artifact
                // declaration only at the response root or beneath an
                // explicit result/output/artifact envelope. This preserves
                // output-only contracts without reclassifying read-model
                // fields as files produced by the query itself.
                if is_explicit_artifact_path_key(key)
                    && (!explicit_paths_require_result_scope
                        || depth == 0
                        || at_result_envelope
                        || (at_root_array_item
                            && is_unambiguous_artifact_path_key(key)))
                {
                    declared.saw_explicit_key = true;
                    collect_path_value(child, declared);
                } else if allow_plain_path
                    && is_plain_path_key(key)
                    && (depth == 0 || at_result_envelope)
                {
                    collect_path_value(child, declared);
                }
                collect_json_artifact_paths(
                    child,
                    declared,
                    nodes,
                    depth + 1,
                    allow_plain_path,
                    explicit_paths_require_result_scope,
                    is_result_scope(key),
                    false,
                );
            }
        }
        serde_json::Value::Array(values) => {
            for child in values {
                collect_json_artifact_paths(
                    child,
                    declared,
                    nodes,
                    depth + 1,
                    allow_plain_path,
                    explicit_paths_require_result_scope,
                    at_result_envelope,
                    at_root_array_item
                        || (explicit_paths_require_result_scope && depth == 0),
                );
            }
        }
        _ => {}
    }
}

fn input_artifact_paths(value: &serde_json::Value) -> DeclaredArtifactPaths {
    let mut declared = DeclaredArtifactPaths::default();
    let mut nodes = 0;
    collect_json_artifact_paths(
        value,
        &mut declared,
        &mut nodes,
        0,
        false,
        false,
        false,
        false,
    );
    declared.enforce_resource_limits_if_artifact_expected(false);
    declared
}

fn output_artifact_paths(content: &str, allow_plain_path: bool) -> DeclaredArtifactPaths {
    let mut declared = DeclaredArtifactPaths::default();
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(content.trim()) {
        let mut nodes = 0;
        collect_json_artifact_paths(
            &value,
            &mut declared,
            &mut nodes,
            0,
            allow_plain_path,
            true,
            false,
            false,
        );
    }

    // A number of native/export tools return a human-readable locator rather
    // than JSON. Only accept explicit output labels; arbitrary prose and input
    // paths are deliberately ignored.
    const LABELS: &[&str] = &[
        "saved to:",
        "output path:",
        "output file:",
        "artifact path:",
        "artifact file:",
        "result path:",
        "result file:",
        "destination path:",
        "destination file:",
    ];
    for line in content.lines() {
        let trimmed = line.trim();
        let lower = trimmed.to_ascii_lowercase();
        for label in LABELS {
            if lower.starts_with(label) {
                declared.saw_explicit_key = true;
                push_declared_path(&mut declared, &trimmed[label.len()..]);
                break;
            }
        }
    }
    declared
}

fn artifact_candidate_path(value: &str) -> Result<PathBuf, String> {
    if value.get(..5).is_some_and(|prefix| prefix.eq_ignore_ascii_case("file:")) {
        let url = url::Url::parse(value).map_err(|error| format!("invalid artifact file URI: {error}"))?;
        return url
            .to_file_path()
            .map_err(|_| "artifact file URI is not a local filesystem path".to_owned());
    }
    Ok(PathBuf::from(value))
}

fn intended_artifact_path(workspace: &Path, value: &str) -> Result<PathBuf, String> {
    let workspace = std::fs::canonicalize(workspace)
        .map_err(|error| format!("cannot canonicalize artifact workspace: {error}"))?;
    let requested = artifact_candidate_path(value)?;
    if requested
        .components()
        .any(|component| matches!(component, std::path::Component::ParentDir))
    {
        return Err("artifact path contains a parent-directory traversal".to_owned());
    }
    let candidate = if requested.is_absolute() {
        requested
    } else {
        workspace.join(requested)
    };

    // Canonicalize the nearest existing ancestor, then re-attach the missing
    // suffix. This validates both paths that already exist and paths the tool
    // promises to create, including symlinked parents, without a time-of-check
    // assumption about filesystem timestamp precision.
    let mut ancestor = candidate.as_path();
    let mut missing_suffix = Vec::new();
    loop {
        match std::fs::symlink_metadata(ancestor) {
            Ok(_) => break,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let file_name = ancestor
                    .file_name()
                    .ok_or_else(|| "artifact path has no existing ancestor".to_owned())?;
                missing_suffix.push(file_name.to_os_string());
                ancestor = ancestor
                    .parent()
                    .ok_or_else(|| "artifact path has no existing ancestor".to_owned())?;
            }
            Err(error) => return Err(format!("cannot inspect artifact path: {error}")),
        }
    }
    let mut resolved = std::fs::canonicalize(ancestor)
        .map_err(|error| format!("cannot canonicalize artifact path: {error}"))?;
    if !resolved.starts_with(&workspace) {
        return Err("artifact path escapes the workspace boundary".to_owned());
    }
    for component in missing_suffix.into_iter().rev() {
        resolved.push(component);
    }
    if !resolved.starts_with(&workspace) {
        return Err("artifact path escapes the workspace boundary".to_owned());
    }
    Ok(resolved)
}

fn hash_file_for_baseline(path: &Path, expected_size: u64) -> Result<String, String> {
    if expected_size > MAX_BASELINE_ARTIFACT_BYTES {
        return Err(format!(
            "artifact baseline exceeds the {} byte limit",
            MAX_BASELINE_ARTIFACT_BYTES
        ));
    }
    let mut file = File::open(path)
        .map_err(|error| format!("cannot open artifact baseline: {error}"))?;
    let mut digest = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    let mut bytes_read = 0_u64;
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|error| format!("cannot read artifact baseline: {error}"))?;
        if read == 0 {
            break;
        }
        bytes_read = bytes_read.saturating_add(read as u64);
        if bytes_read > MAX_BASELINE_ARTIFACT_BYTES {
            return Err(format!(
                "artifact baseline exceeds the {} byte limit",
                MAX_BASELINE_ARTIFACT_BYTES
            ));
        }
        digest.update(&buffer[..read]);
    }
    let metadata = std::fs::metadata(path)
        .map_err(|error| format!("cannot re-check artifact baseline: {error}"))?;
    if !metadata.is_file() || metadata.len() != expected_size || bytes_read != expected_size {
        return Err("artifact baseline changed while it was being fingerprinted".to_owned());
    }
    Ok(hex::encode(digest.finalize()))
}

fn capture_path_fingerprint(path: &Path) -> Result<ArtifactPathFingerprint, String> {
    let metadata = match std::fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(ArtifactPathFingerprint::Absent);
        }
        Err(error) => return Err(format!("cannot inspect artifact baseline: {error}")),
    };
    if !metadata.is_file() {
        return Err("declared artifact baseline is not a regular file".to_owned());
    }
    let size_bytes = metadata.len();
    let sha256 = hash_file_for_baseline(path, size_bytes)?;
    Ok(ArtifactPathFingerprint::Present { size_bytes, sha256 })
}

/// Parse the `update_plan` tool result content into frontend plan entries.
/// The content may carry a soft-warning prefix, so we start from the first '{'.
fn parse_plan_entries(content: &str) -> Option<Vec<serde_json::Value>> {
    let start = content.find('{')?;
    let v: serde_json::Value = serde_json::from_str(&content[start..]).ok()?;
    if v.get("kind").and_then(|k| k.as_str()) != Some("plan_update") {
        return None;
    }
    let entries = v.get("entries")?.as_array()?.clone();
    Some(entries)
}

impl BackendOutputSink {
    pub fn new(event_tx: broadcast::Sender<AgentStreamEvent>) -> Self {
        Self {
            event_tx,
            distill_dir: None,
            artifact_store: None,
            artifact_workspace: None,
            turn_text: Mutex::new(String::new()),
            active_tool_calls: Mutex::new(HashMap::new()),
            tool_result_contexts: Mutex::new(HashMap::new()),
            artifact_delivery_turn: Mutex::new(ArtifactDeliveryTurn::default()),
        }
    }

    /// Set the file-based memory directory used for citation reflow. `None`
    /// (the default) disables reflow for this session.
    pub fn with_distill_dir(mut self, dir: Option<PathBuf>) -> Self {
        self.distill_dir = dir;
        self
    }

    /// Enable durable, verified binary output delivery under the session's
    /// trusted workspace.
    pub fn with_artifact_workspace(mut self, workspace: impl Into<PathBuf>) -> Self {
        let workspace = workspace.into();
        self.artifact_store = Some(ArtifactStore::new(workspace.clone()));
        self.artifact_workspace = Some(workspace);
        self
    }

    /// Begin one accepted user turn's artifact-delivery ledger. Engine
    /// sub-streams do not call this: steering and truncation continuations are
    /// part of the same accepted turn and must retain earlier failures.
    pub fn begin_artifact_delivery_turn(&self) {
        let mut turn = self
            .artifact_delivery_turn
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        turn.calls.clear();
        turn.active = true;
    }

    /// Seal one accepted user turn. Every artifact-producing call must have a
    /// verified receipt of its own; a later successful call cannot mask an
    /// earlier failure or an unfinished call.
    pub fn finish_artifact_delivery_turn(&self) -> Result<(), String> {
        let mut turn = match self.artifact_delivery_turn.lock() {
            Ok(turn) => turn,
            Err(poisoned) => {
                let mut turn = poisoned.into_inner();
                turn.active = false;
                turn.calls.clear();
                return Err("artifact-delivery ledger lock was poisoned".to_owned());
            }
        };
        if !turn.active {
            return Err("artifact-delivery turn was not active".to_owned());
        }
        turn.active = false;
        let mut failures = Vec::new();
        for obligation in turn.calls.values() {
            match &obligation.status {
                ArtifactCallDeliveryStatus::CompletedVerified(artifacts) => {
                    let Some(store) = self.artifact_store.as_ref() else {
                        failures.push(format!(
                            "{} ({}) lost its workspace artifact store before turn completion",
                            obligation.tool_name,
                            obligation.contract.label()
                        ));
                        continue;
                    };
                    for artifact in artifacts {
                        if let Err(error) = store.reverify_receipt(artifact) {
                            failures.push(format!(
                                "{} ({}) artifact {} failed final verification: {error}",
                                obligation.tool_name,
                                obligation.contract.label(),
                                artifact.path
                            ));
                        }
                    }
                    let mime_types = artifacts
                        .iter()
                        .map(|artifact| artifact.mime_type.as_str())
                        .collect::<Vec<_>>();
                    if let Err(error) = obligation.contract.validate_mimes(&mime_types) {
                        failures.push(format!(
                            "{} ({}) failed final contract verification: {error}",
                            obligation.tool_name,
                            obligation.contract.label()
                        ));
                    }
                }
                ArtifactCallDeliveryStatus::Running => failures.push(format!(
                    "{} ({}) ended without a verified artifact receipt",
                    obligation.tool_name,
                    obligation.contract.label()
                )),
                ArtifactCallDeliveryStatus::Failed(reason) => failures.push(format!(
                    "{} ({}) failed artifact delivery: {reason}",
                    obligation.tool_name,
                    obligation.contract.label()
                )),
            }
        }
        turn.calls.clear();
        if failures.is_empty() {
            Ok(())
        } else {
            Err(failures.join("; "))
        }
    }

    fn record_artifact_obligation(
        &self,
        call_id: &str,
        tool_name: &str,
        contract: Option<ArtifactContract>,
    ) {
        let Some(contract) = contract else {
            return;
        };
        let mut turn = self
            .artifact_delivery_turn
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if !turn.active {
            return;
        }
        turn.calls
            .entry(call_id.to_owned())
            .and_modify(|obligation| {
                if matches!(obligation.status, ArtifactCallDeliveryStatus::Running) {
                    match obligation.contract.merge(contract) {
                        Ok(merged) => obligation.contract = merged,
                        Err(error) => {
                            obligation.status = ArtifactCallDeliveryStatus::Failed(format!(
                                "conflicting artifact contract metadata: {error}"
                            ));
                        }
                    }
                }
            })
            .or_insert_with(|| ArtifactCallObligation {
                tool_name: tool_name.to_owned(),
                contract,
                status: ArtifactCallDeliveryStatus::Running,
            });
    }

    fn register_artifact_obligation(
        &self,
        call_id: &str,
        tool_name: &str,
        contract: Option<ArtifactContract>,
    ) {
        let Some(contract) = contract else {
            return;
        };
        let mut turn = self
            .artifact_delivery_turn
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if !turn.active {
            return;
        }
        match turn.calls.entry(call_id.to_owned()) {
            std::collections::hash_map::Entry::Vacant(entry) => {
                entry.insert(ArtifactCallObligation {
                    tool_name: tool_name.to_owned(),
                    contract,
                    status: ArtifactCallDeliveryStatus::Running,
                });
            }
            std::collections::hash_map::Entry::Occupied(mut entry) => {
                entry.get_mut().status = ArtifactCallDeliveryStatus::Failed(
                    "artifact-producing tool call reused a prior call id".to_owned(),
                );
            }
        }
    }

    fn settle_artifact_obligation(
        &self,
        call_id: &str,
        tool_name: &str,
        is_error: bool,
        artifacts: &[PersistedArtifact],
    ) {
        let mut turn = self
            .artifact_delivery_turn
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if !turn.calls.contains_key(call_id) && !is_error && !artifacts.is_empty() && turn.active {
            // An unknown/brand-named tool may still return a real persisted
            // artifact even when its name carried no pre-call requirement.
            // Every receipt that we publish must enter the turn ledger so a
            // later tool cannot delete it before Finish and escape final
            // re-verification.
            turn.calls.insert(
                call_id.to_owned(),
                ArtifactCallObligation {
                    tool_name: tool_name.to_owned(),
                    contract: any_artifact_contract(),
                    status: ArtifactCallDeliveryStatus::Running,
                },
            );
        }
        let Some(obligation) = turn.calls.get_mut(call_id) else {
            return;
        };
        obligation.status = match &obligation.status {
            ArtifactCallDeliveryStatus::Running => {
                if is_error {
                    ArtifactCallDeliveryStatus::Failed("tool returned an error".to_owned())
                } else if artifacts.is_empty() {
                    ArtifactCallDeliveryStatus::Failed(
                        "tool completed without a verified artifact receipt".to_owned(),
                    )
                } else {
                    let mime_types = artifacts
                        .iter()
                        .map(|artifact| artifact.mime_type.as_str())
                        .collect::<Vec<_>>();
                    match obligation.contract.validate_mimes(&mime_types) {
                        Ok(()) => ArtifactCallDeliveryStatus::CompletedVerified(artifacts.to_vec()),
                        Err(error) => ArtifactCallDeliveryStatus::Failed(format!(
                            "verified receipts do not satisfy the artifact contract: {error}"
                        )),
                    }
                }
            }
            ArtifactCallDeliveryStatus::CompletedVerified(_) => {
                ArtifactCallDeliveryStatus::Failed(
                    "artifact-producing tool call emitted more than one terminal result".to_owned(),
                )
            }
            ArtifactCallDeliveryStatus::Failed(reason) => {
                ArtifactCallDeliveryStatus::Failed(reason.clone())
            }
        };
    }

    fn fail_artifact_obligation(&self, call_id: &str, reason: &str) {
        let mut turn = self
            .artifact_delivery_turn
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(obligation) = turn.calls.get_mut(call_id) {
            obligation.status = ArtifactCallDeliveryStatus::Failed(reason.to_owned());
        }
    }

    fn record_unidentified_artifact_failure(
        &self,
        tool_name: &str,
        contract: Option<ArtifactContract>,
        reason: &str,
    ) {
        let Some(contract) = contract else {
            return;
        };
        let mut turn = self
            .artifact_delivery_turn
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if !turn.active {
            return;
        }
        let mut sequence = turn.calls.len();
        let call_id = loop {
            let candidate = format!("invalid-artifact-call-{sequence}");
            if !turn.calls.contains_key(&candidate) {
                break candidate;
            }
            sequence += 1;
        };
        turn.calls.insert(
            call_id,
            ArtifactCallObligation {
                tool_name: tool_name.to_owned(),
                contract,
                status: ArtifactCallDeliveryStatus::Failed(reason.to_owned()),
            },
        );
    }

    fn capture_artifact_path_baselines(
        &self,
        input: &serde_json::Value,
    ) -> ArtifactPathBaselines {
        let declared = input_artifact_paths(input);
        let mut baselines = ArtifactPathBaselines::default();
        if !declared.saw_explicit_key && declared.errors.is_empty() {
            return baselines;
        }
        baselines.errors.extend(declared.errors);
        if declared.paths.is_empty() {
            if baselines.errors.is_empty() {
                baselines
                    .errors
                    .push("declared artifact output key contains no usable path".to_owned());
            }
            return baselines;
        }
        let Some(workspace) = self.artifact_workspace.as_deref() else {
            baselines
                .errors
                .push("session has no workspace for artifact baselines".to_owned());
            return baselines;
        };
        for raw_path in declared.paths {
            let path = match intended_artifact_path(workspace, &raw_path) {
                Ok(path) => path,
                Err(error) => {
                    baselines
                        .errors
                        .push(format!("invalid declared artifact path {raw_path:?}: {error}"));
                    continue;
                }
            };
            if baselines.entries.iter().any(|known| known.path == path) {
                continue;
            }
            match capture_path_fingerprint(&path) {
                Ok(fingerprint) => baselines
                    .entries
                    .push(ArtifactPathBaseline { path, fingerprint }),
                Err(error) => baselines.errors.push(format!(
                    "cannot fingerprint declared artifact path {raw_path:?}: {error}"
                )),
            }
        }
        baselines
    }

    fn inline_artifact_kind(mime_type: &str) -> ArtifactKind {
        let mime = mime_type
            .split(';')
            .next()
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase();
        if mime.starts_with("image/") {
            ArtifactKind::Image
        } else if mime.starts_with("audio/") {
            ArtifactKind::Audio
        } else if mime.starts_with("video/") {
            ArtifactKind::Video
        } else if mime.starts_with("text/")
            || matches!(mime.as_str(), "application/json" | "application/xml")
        {
            ArtifactKind::Text
        } else {
            ArtifactKind::File
        }
    }

    fn append_delivery_context(content: &str, context: &str) -> String {
        match (content.trim().is_empty(), context.trim().is_empty()) {
            (true, _) => context.to_owned(),
            (_, true) => content.to_owned(),
            (false, false) => format!("{content}\n{context}"),
        }
    }

    fn delivery_context(artifacts: &[PersistedArtifact]) -> String {
        let mut context = String::from("Verified artifacts saved to:");
        for artifact in artifacts {
            context.push_str("\n- ");
            context.push_str(&artifact.path);
        }
        context
    }

    fn preflight_declared_path_artifacts(
        &self,
        active: Option<&ActiveToolCall>,
        contract: ArtifactContract,
        declared_output: &DeclaredArtifactPaths,
    ) -> Result<Vec<PathBuf>, String> {
        if !declared_output.errors.is_empty() {
            return Err(declared_output.errors.join("; "));
        }
        let has_output_declaration =
            declared_output.saw_explicit_key || !declared_output.paths.is_empty();
        let has_input_declaration = active
            .is_some_and(|call| call.artifact_path_baselines.declares_artifact());
        if !has_output_declaration && !has_input_declaration {
            return Ok(Vec::new());
        }
        let active = active.ok_or_else(|| {
            "result-only artifact path has no pre-call baseline; refusing an unproven file"
                .to_owned()
        })?;
        if !active.artifact_path_baselines.errors.is_empty() {
            return Err(active.artifact_path_baselines.errors.join("; "));
        }
        if declared_output.saw_explicit_key && declared_output.paths.is_empty() {
            return Err("artifact result contains an explicit output key but no usable path".to_owned());
        }
        let store = self
            .artifact_store
            .as_ref()
            .ok_or_else(|| "session has no workspace artifact store".to_owned())?;

        let Some(workspace) = self.artifact_workspace.as_deref() else {
            return Err("session has no workspace for artifact verification".to_owned());
        };
        for raw_path in &declared_output.paths {
            let output_path = intended_artifact_path(workspace, raw_path)?;
            if !active
                .artifact_path_baselines
                .entries
                .iter()
                .any(|baseline| baseline.path == output_path)
            {
                return Err(format!(
                    "result-only artifact path {raw_path:?} has no matching pre-call baseline"
                ));
            }
        }

        let mut verified = Vec::with_capacity(active.artifact_path_baselines.entries.len());
        for baseline in &active.artifact_path_baselines.entries {
            let file_type = match std::fs::symlink_metadata(&baseline.path) {
                Ok(metadata) => metadata.file_type(),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    return Err(
                        "declared artifact path is still missing after the tool completed"
                            .to_owned(),
                    );
                }
                Err(error) => {
                    return Err(format!("cannot inspect declared artifact path: {error}"));
                }
            };
            if file_type.is_symlink() {
                return Err(
                    "declared artifact path became a symbolic link after the call began".to_owned(),
                );
            }
            let artifact = store
                .verify_existing_path(&baseline.path)
                .map_err(|error| format!("declared artifact path failed verification: {error}"))?;
            if !contract.accepts_mime(&artifact.mime_type) {
                return Err(format!(
                    "declared artifact path has MIME {}, expected {}",
                    artifact.mime_type,
                    contract.label()
                ));
            }
            if let ArtifactPathFingerprint::Present { size_bytes, sha256 } = &baseline.fingerprint
                && artifact.sha256 == *sha256
            {
                return Err(format!(
                    "declared artifact path is unchanged from its pre-call fingerprint ({} bytes)",
                    size_bytes
                ));
            }
            if !verified.iter().any(|known| known == &baseline.path) {
                verified.push(baseline.path.clone());
            }
        }
        Ok(verified)
    }

    fn emit_terminal_tool_result(
        &self,
        call_id: String,
        name: &str,
        is_error: bool,
        content: &str,
        artifacts: Vec<PersistedArtifact>,
    ) {
        let explicit_context = match self.tool_result_contexts.lock() {
            Ok(mut contexts) => contexts.remove(&call_id),
            Err(poisoned) => {
                tracing::warn!(
                    error = %poisoned,
                    "Tool-result context lock was poisoned while settling a result"
                );
                poisoned.into_inner().remove(&call_id)
            }
        };
        let active_context = self.active_tool_call(&call_id).map(|active| ToolTerminalContext {
            args: active.args,
            input: active.input,
            retry: active.retry,
        });
        let context = explicit_context.or(active_context);
        self.settle_artifact_obligation(&call_id, name, is_error, &artifacts);
        self.forget_active_tool_call(&call_id);
        let status = if is_error {
            ToolCallStatus::Error
        } else {
            ToolCallStatus::Completed
        };

        tracing::info!(
            call_id = %call_id,
            tool = name,
            status = ?status,
            artifact_count = artifacts.len(),
            "Emitting nomi tool_result event"
        );

        let _ = self.event_tx.send(AgentStreamEvent::ToolCall(ToolCallEventData {
            call_id,
            name: name.to_owned(),
            args: context
                .as_ref()
                .map(|context| context.args.clone())
                .unwrap_or(serde_json::Value::Null),
            status,
            input: context.as_ref().and_then(|context| context.input.clone()),
            output: if content.is_empty() {
                None
            } else {
                Some(content.to_owned())
            },
            description: None,
            retry: context.and_then(|context| context.retry),
            artifacts,
        }));
    }

    fn retry_data(retry: &ToolCallRetryContext) -> Option<ToolCallRetryData> {
        let retry_group_id = Self::internal_call_id(&retry.retry_group_id)?;
        let retry_of_call_id = match retry.retry_of_call_id.as_deref() {
            Some(call_id) => Some(Self::internal_call_id(call_id)?),
            None => None,
        };
        Some(ToolCallRetryData {
            retry_group_id,
            attempt_no: retry.attempt_no,
            retry_of_call_id,
        })
    }

    fn internal_call_id(tool_use_id: &str) -> Option<String> {
        let id = tool_use_id.trim();
        if id.is_empty() || id != tool_use_id {
            None
        } else {
            Some(format!("nomi-{id}"))
        }
    }

    fn remember_active_tool_call(
        &self,
        call_id: String,
        name: String,
        artifact_identity: String,
        args: serde_json::Value,
        input: Option<serde_json::Value>,
        retry: Option<ToolCallRetryData>,
    ) {
        let artifact_path_baselines = if is_context_only_image_tool(&artifact_identity) {
            ArtifactPathBaselines::default()
        } else {
            self.capture_artifact_path_baselines(&args)
        };
        let (mut contract, contract_error) =
            match artifact_contract_with_input(&artifact_identity, &args) {
                Ok(contract) => (contract, None),
                Err(error) => (
                    artifact_contract(&artifact_identity),
                    Some(format!("invalid artifact contract input: {error}")),
                ),
            };
        if contract.is_none() && artifact_path_baselines.declares_artifact() {
            contract = Some(any_artifact_contract());
        }
        self.register_artifact_obligation(&call_id, &name, contract);
        if let Some(error) = contract_error.as_deref() {
            self.fail_artifact_obligation(&call_id, error);
        }
        match self.active_tool_calls.lock() {
            Ok(mut active) => {
                active.insert(
                    call_id.clone(),
                    ActiveToolCall {
                        call_id,
                        name,
                        artifact_identity,
                        args,
                        input,
                        contract,
                        contract_error,
                        artifact_path_baselines,
                        retry,
                    },
                );
            }
            Err(err) => {
                tracing::warn!(
                    error = %err,
                    "Failed to record active tool call for continuation cleanup"
                );
            }
        }
    }

    fn forget_active_tool_call(&self, call_id: &str) {
        match self.active_tool_calls.lock() {
            Ok(mut active) => {
                active.remove(call_id);
            }
            Err(poisoned) => {
                tracing::warn!(
                    error = %poisoned,
                    "Active tool-call lock was poisoned while settling a result"
                );
                poisoned.into_inner().remove(call_id);
            }
        }
    }

    fn active_tool_call(&self, call_id: &str) -> Option<ActiveToolCall> {
        match self.active_tool_calls.lock() {
            Ok(active) => active.get(call_id).cloned(),
            Err(poisoned) => {
                tracing::warn!(
                    error = %poisoned,
                    "Active tool-call lock was poisoned while verifying an artifact path"
                );
                poisoned.into_inner().get(call_id).cloned()
            }
        }
    }

    fn terminate_active_tool_calls(
        &self,
        status: ToolCallStatus,
        output: String,
        description: &str,
        lock_failure_context: &str,
    ) {
        // No result from an earlier stream may lend retry/argument metadata to
        // a later call that happens to reuse an id. Active calls carry their
        // own immutable copy for the terminal correction frames below.
        match self.tool_result_contexts.lock() {
            Ok(mut contexts) => contexts.clear(),
            Err(poisoned) => poisoned.into_inner().clear(),
        }
        let interrupted: Vec<ActiveToolCall> = match self.active_tool_calls.lock() {
            Ok(mut active) => active.drain().map(|(_, data)| data).collect(),
            Err(poisoned) => {
                tracing::warn!(
                    error = %poisoned,
                    "{lock_failure_context}"
                );
                poisoned.into_inner().drain().map(|(_, data)| data).collect()
            }
        };

        for active in interrupted {
            self.fail_artifact_obligation(&active.call_id, &output);
            let _ = self.event_tx.send(AgentStreamEvent::ToolCall(ToolCallEventData {
                call_id: active.call_id,
                name: active.name,
                args: active.args,
                status,
                input: active.input,
                output: Some(output.clone()),
                description: Some(description.to_owned()),
                retry: active.retry,
                artifacts: Vec::new(),
            }));
        }
    }

    /// Fail every tool call already announced to the frontend but still lacking
    /// a real result. Provider/engine failures must not leave a permanent
    /// `Running` card that a later continuation can accidentally recover.
    pub(crate) fn fail_active_tool_calls(&self, reason: &str) {
        self.terminate_active_tool_calls(
            ToolCallStatus::Error,
            reason.to_owned(),
            "Tool call failed",
            "Failed to resolve active tool calls after turn failure",
        );
    }

    /// Cancel every tool call already announced to the frontend. The protocol
    /// currently has no `Cancelled` tool status, so cancellation uses the only
    /// non-success terminal status (`Error`) and carries the distinction in the
    /// description/output text.
    pub(crate) fn cancel_active_tool_calls(&self, reason: &str) {
        self.terminate_active_tool_calls(
            ToolCallStatus::Error,
            reason.to_owned(),
            "Tool call cancelled",
            "Failed to resolve active tool calls after turn cancellation",
        );
    }

    /// Fail any active call defensively before a MaxTokens retry. The engine no
    /// longer publishes partial provider deltas, so this is normally a no-op;
    /// retaining the cleanup prevents an already-published committed call from
    /// leaking into a following stream after an unexpected terminal path.
    pub(crate) fn truncate_active_tool_calls_for_auto_continue(&self, reason: &str) {
        let output = format!(
            "The provider response ended at {reason}; this incomplete tool call was not executed. The task is continuing in a new stream."
        );
        self.terminate_active_tool_calls(
            ToolCallStatus::Error,
            output,
            "Tool call truncated",
            "Failed to resolve active tool calls before automatic continuation",
        );
    }

    /// Citation reflow: parse the `<nomi-mem-citation>` block from the turn's
    /// final assistant text and bump each cited memory file's usage stats.
    /// Silent on every failure — a stale citation or unreadable file must
    /// never disrupt the turn.
    fn reflow_citations(&self, full_text: &str) {
        let Some(dir) = self.distill_dir.as_ref() else {
            return;
        };
        let now = chrono::Utc::now();
        for fname in nomi_memory::distill::parse_citation_filenames(full_text) {
            if let Err(e) = nomi_memory::store::bump_memory_usage(dir, &fname, now) {
                tracing::debug!(file = %fname, error = %e, "citation reflow bump failed");
            }
        }
    }
}

impl OutputSink for BackendOutputSink {
    fn emit_text_delta(&self, text: &str, _msg_id: &str) {
        // Accumulate for end-of-turn citation reflow (only when participating).
        if self.distill_dir.is_some()
            && let Ok(mut buf) = self.turn_text.lock()
        {
            buf.push_str(text);
        }
        let _ = self.event_tx.send(AgentStreamEvent::Text(TextEventData {
            content: text.to_owned(),
        }));
    }

    fn emit_thinking(&self, text: &str, _msg_id: &str) {
        let _ = self.event_tx.send(AgentStreamEvent::Thinking(ThinkingEventData {
            content: text.to_owned(),
            subject: None,
            duration: None,
            status: None,
        }));
    }

    fn emit_model_activity(&self, _msg_id: &str, status: &str) {
        let _ = self
            .event_tx
            .send(AgentStreamEvent::AgentStatus(AgentStatusEventData {
                backend: "nomi".to_owned(),
                status: status.to_owned(),
                agent_name: Some("Nomi".to_owned()),
                session_id: None,
            }));
    }

    fn emit_tool_call(&self, tool_use_id: &str, name: &str, input: &str) {
        self.emit_tool_call_with_artifact_identity(tool_use_id, name, name, input);
    }

    fn emit_tool_call_with_artifact_identity(
        &self,
        tool_use_id: &str,
        name: &str,
        artifact_identity: &str,
        input: &str,
    ) {
        let parsed_input = serde_json::from_str(input)
            .unwrap_or(serde_json::Value::String(input.to_owned()));
        let Some(call_id) = Self::internal_call_id(tool_use_id) else {
            let (mut contract, contract_error) =
                match artifact_contract_with_input(artifact_identity, &parsed_input) {
                    Ok(contract) => (contract, None),
                    Err(error) => (
                        artifact_contract(artifact_identity),
                        Some(format!("invalid artifact contract input: {error}")),
                    ),
                };
            if contract.is_none() && input_artifact_paths(&parsed_input).saw_explicit_key {
                contract = Some(any_artifact_contract());
            }
            let reason = contract_error.as_deref().unwrap_or(
                "tool call has an empty or non-canonical call id",
            );
            self.record_unidentified_artifact_failure(
                name,
                contract,
                reason,
            );
            tracing::error!(
                tool = name,
                artifact_identity,
                "Cannot emit tool_call with empty or non-canonical tool_use_id"
            );
            return;
        };
        let retry = match self.tool_result_contexts.lock() {
            Ok(contexts) => contexts
                .get(&call_id)
                .and_then(|context| context.retry.clone()),
            Err(poisoned) => poisoned
                .into_inner()
                .get(&call_id)
                .and_then(|context| context.retry.clone()),
        };

        tracing::debug!(
            tool_use_id = %tool_use_id,
            call_id = %call_id,
            tool = name,
            status = ?ToolCallStatus::Running,
            "Derived internal tool_call id from nomi tool_use_id"
        );
        tracing::info!(
            tool_use_id = %tool_use_id,
            call_id = %call_id,
            tool = name,
            status = ?ToolCallStatus::Running,
            "Emitting nomi tool_call event"
        );

        self.remember_active_tool_call(
            call_id.clone(),
            name.to_owned(),
            artifact_identity.to_owned(),
            parsed_input.clone(),
            Some(parsed_input.clone()),
            retry.clone(),
        );

        let _ = self.event_tx.send(AgentStreamEvent::ToolCall(ToolCallEventData {
            call_id,
            name: name.to_owned(),
            args: parsed_input.clone(),
            status: ToolCallStatus::Running,
            input: Some(parsed_input),
            output: None,
            description: None,
            retry,
            artifacts: Vec::new(),
        }));
    }

    fn emit_tool_call_with_context(
        &self,
        tool_use_id: &str,
        name: &str,
        artifact_identity: &str,
        input: &str,
        context: &ToolCallExecutionContext,
    ) {
        if let Some(call_id) = Self::internal_call_id(tool_use_id) {
            let terminal_context = ToolTerminalContext {
                args: context.input.clone(),
                input: Some(context.input.clone()),
                retry: Self::retry_data(&context.retry),
            };
            match self.tool_result_contexts.lock() {
                Ok(mut contexts) => {
                    contexts.insert(call_id, terminal_context);
                }
                Err(poisoned) => {
                    poisoned.into_inner().insert(call_id, terminal_context);
                }
            }
        }
        self.emit_tool_call_with_artifact_identity(
            tool_use_id,
            name,
            artifact_identity,
            input,
        );
    }

    fn emit_tool_result(&self, tool_use_id: &str, name: &str, is_error: bool, content: &str) {
        // update_plan special case: emit a Plan event so the frontend renders
        // the checklist (MessagePlan) instead of a raw JSON tool card.
        if name == "update_plan"
            && !is_error
            && let Some(entries) = parse_plan_entries(content)
        {
            let Some(call_id) = Self::internal_call_id(tool_use_id) else {
                tracing::error!(
                    tool = name,
                    "Cannot emit update_plan result with empty or non-canonical tool_use_id"
                );
                return;
            };
            self.forget_active_tool_call(&call_id);
            let _ = self.event_tx.send(AgentStreamEvent::Plan(PlanEventData {
                session_id: Some("update_plan".to_string()),
                source_call_id: Some(call_id),
                entries,
            }));
            return;
        }
        // Unparsable update_plan output falls through to a normal tool result.

        let _ = self.emit_tool_result_with_images_and_artifact_identity(
            tool_use_id,
            name,
            name,
            is_error,
            content,
            &[],
        );
    }

    fn emit_tool_result_with_images(
        &self,
        tool_use_id: &str,
        name: &str,
        is_error: bool,
        content: &str,
        images: &[ToolImage],
    ) -> ToolMediaDelivery {
        self.emit_tool_result_with_images_and_artifact_identity(
            tool_use_id,
            name,
            name,
            is_error,
            content,
            images,
        )
    }

    fn emit_tool_result_with_images_and_artifact_identity(
        &self,
        tool_use_id: &str,
        name: &str,
        artifact_identity: &str,
        is_error: bool,
        content: &str,
        images: &[ToolImage],
    ) -> ToolMediaDelivery {
        let Some(call_id) = Self::internal_call_id(tool_use_id) else {
            let mut explicit_output = output_artifact_paths(content, false);
            let mut contract = artifact_contract(artifact_identity);
            explicit_output.enforce_resource_limits_if_artifact_expected(contract.is_some());
            if contract.is_none()
                && (!images.is_empty()
                    || explicit_output.saw_explicit_key
                    || !explicit_output.paths.is_empty()
                    || !explicit_output.errors.is_empty())
            {
                contract = Some(any_artifact_contract());
            }
            self.record_unidentified_artifact_failure(
                name,
                contract,
                "tool result has an empty or non-canonical call id",
            );
            return ToolMediaDelivery::Failed {
                error: "tool result has no canonical call id; artifact was not written".to_owned(),
            };
        };

        // Failed tools may return diagnostic images. They remain transient
        // model context: never persist or publish them as successful artifacts.
        if is_error {
            self.emit_terminal_tool_result(call_id, name, true, content, Vec::new());
            return ToolMediaDelivery::Unmanaged;
        }

        let active = self.active_tool_call(&call_id);
        let effective_identity = active
            .as_ref()
            .map(|call| call.artifact_identity.as_str())
            .unwrap_or(artifact_identity);

        // Browser/computer screenshots are observational context, not durable
        // user-requested output. Do not create files or artifact receipts.
        if is_context_only_image_tool(effective_identity) {
            self.emit_terminal_tool_result(call_id, name, false, content, Vec::new());
            return ToolMediaDelivery::Unmanaged;
        }

        if let Some(error) = active
            .as_ref()
            .and_then(|call| call.contract_error.as_deref())
        {
            let error = error.to_owned();
            let output = Self::append_delivery_context(
                content,
                &format!("Artifact delivery failed: {error}"),
            );
            self.emit_terminal_tool_result(call_id, name, true, &output, Vec::new());
            return ToolMediaDelivery::Failed { error };
        }

        let mut explicit_output = output_artifact_paths(content, false);
        let observed_contract = artifact_contract(artifact_identity);
        let mut contract = match (
            active.as_ref().and_then(|call| call.contract),
            observed_contract,
        ) {
            (Some(existing), Some(observed)) => match existing.merge(observed) {
                Ok(contract) => Some(contract),
                Err(error) => {
                    let error = format!("conflicting tool artifact identities: {error}");
                    self.fail_artifact_obligation(&call_id, &error);
                    let output = Self::append_delivery_context(
                        content,
                        &format!("Artifact delivery failed: {error}"),
                    );
                    self.emit_terminal_tool_result(call_id, name, true, &output, Vec::new());
                    return ToolMediaDelivery::Failed { error };
                }
            },
            (Some(contract), None) | (None, Some(contract)) => Some(contract),
            (None, None) => None,
        };
        explicit_output.enforce_resource_limits_if_artifact_expected(contract.is_some());
        if contract.is_none()
            && (!images.is_empty()
                || explicit_output.saw_explicit_key
                || !explicit_output.paths.is_empty()
                || !explicit_output.errors.is_empty())
        {
            contract = Some(any_artifact_contract());
        }
        self.record_artifact_obligation(&call_id, name, contract);

        let mut declared_output = if contract.is_some() {
            output_artifact_paths(content, true)
        } else {
            explicit_output
        };
        declared_output.enforce_resource_limits_if_artifact_expected(contract.is_some());

        if images.is_empty()
            && contract.is_none()
            && !declared_output.saw_explicit_key
            && declared_output.paths.is_empty()
            && declared_output.errors.is_empty()
        {
            self.emit_terminal_tool_result(call_id, name, false, content, Vec::new());
            return ToolMediaDelivery::Unmanaged;
        }

        let contract = contract.unwrap_or_else(any_artifact_contract);

        // Some native and third-party generators write directly into the
        // workspace and return a structured/human-readable output path instead
        // of inline bytes. Accept only paths captured before the call and only
        // when an absent path appeared or an existing file's content hash
        // changed. Preflight these paths before writing inline bytes so mixed
        // output cannot leave a partial artifact batch.
        let path_sources = match self.preflight_declared_path_artifacts(
            active.as_ref(),
            contract,
            &declared_output,
        ) {
            Ok(artifacts) => artifacts,
            Err(error) => {
                let output = Self::append_delivery_context(
                    content,
                    &format!("Artifact delivery failed: {error}"),
                );
                self.emit_terminal_tool_result(call_id, name, true, &output, Vec::new());
                return ToolMediaDelivery::Failed { error };
            }
        };

        if let Some((index, artifact)) = images
            .iter()
            .enumerate()
            .find(|(_, artifact)| !contract.accepts_mime(&artifact.media_type))
        {
            let error = format!(
                "artifact-producing tool returned no {} satisfying the contract; inline artifact {index} has MIME {}",
                contract.label(),
                artifact.media_type,
            );
            let output = Self::append_delivery_context(
                content,
                &format!("Artifact delivery failed: {error}"),
            );
            self.emit_terminal_tool_result(call_id, name, true, &output, Vec::new());
            return ToolMediaDelivery::Failed { error };
        }

        let actual_count = path_sources.len().saturating_add(images.len());
        if actual_count < contract.expected_count() {
            let error = if actual_count == 0 && contract.expected_count() == 1 {
                format!("artifact-producing tool returned no {}", contract.label())
            } else {
                format!(
                    "artifact-producing tool returned {actual_count} verified candidate(s), expected at least {} {}(s)",
                    contract.expected_count(),
                    contract.label()
                )
            };
            let output = Self::append_delivery_context(
                content,
                &format!("Artifact delivery failed: {error}"),
            );
            self.emit_terminal_tool_result(call_id, name, true, &output, Vec::new());
            return ToolMediaDelivery::Failed { error };
        }

        let Some(store) = self.artifact_store.as_ref() else {
            let error = "session has no workspace artifact store".to_owned();
            let output = Self::append_delivery_context(content, &format!("Artifact delivery failed: {error}"));
            self.emit_terminal_tool_result(call_id, name, true, &output, Vec::new());
            return ToolMediaDelivery::Failed { error };
        };

        match store.persist_inline_and_existing_batch(
            images.iter().map(|artifact| {
                (
                    Self::inline_artifact_kind(&artifact.media_type),
                    &artifact.media_type,
                    &artifact.data,
                )
            }),
            &path_sources,
        ) {
            Ok(artifacts) => {
                let context = Self::delivery_context(&artifacts);
                let output = Self::append_delivery_context(content, &context);
                self.emit_terminal_tool_result(call_id, name, false, &output, artifacts);
                ToolMediaDelivery::Delivered { context }
            }
            Err(error) => {
                let error = error.to_string();
                let output = Self::append_delivery_context(content, &format!("Artifact delivery failed: {error}"));
                self.emit_terminal_tool_result(call_id, name, true, &output, Vec::new());
                ToolMediaDelivery::Failed { error }
            }
        }
    }

    fn emit_tool_result_with_images_and_context(
        &self,
        tool_use_id: &str,
        name: &str,
        artifact_identity: &str,
        is_error: bool,
        content: &str,
        images: &[ToolImage],
        context: &ToolCallExecutionContext,
    ) -> ToolMediaDelivery {
        if let Some(call_id) = Self::internal_call_id(tool_use_id) {
            let terminal_context = ToolTerminalContext {
                args: context.input.clone(),
                input: Some(context.input.clone()),
                retry: Self::retry_data(&context.retry),
            };
            match self.tool_result_contexts.lock() {
                Ok(mut contexts) => {
                    contexts.insert(call_id, terminal_context);
                }
                Err(poisoned) => {
                    poisoned.into_inner().insert(call_id, terminal_context);
                }
            }
        }
        self.emit_tool_result_with_images_and_artifact_identity(
            tool_use_id,
            name,
            artifact_identity,
            is_error,
            content,
            images,
        )
    }

    fn emit_stream_start(&self, _msg_id: &str) {
        // A fresh stream is a lifecycle boundary. Normally the manager has
        // already resolved the prior pass (including MaxTokens auto-continue),
        // but fail any survivor defensively so it cannot be resurrected by a
        // later continuation.
        self.fail_active_tool_calls(
            "A new model stream started before the previous tool call reached a terminal state.",
        );
        // Reset the per-turn text buffer used for citation reflow.
        if let Ok(mut buf) = self.turn_text.lock() {
            buf.clear();
        }
        let _ = self
            .event_tx
            .send(AgentStreamEvent::Start(StartEventData { session_id: None }));
    }

    fn emit_stream_end(
        &self,
        _msg_id: &str,
        _turns: usize,
        _input_tokens: u64,
        _output_tokens: u64,
        _cache_creation_tokens: u64,
        _cache_read_tokens: u64,
    ) {
        // Citation reflow: parse the accumulated assistant text and bump the
        // cited memory files. Take the buffer so it doesn't linger.
        if self.distill_dir.is_some() {
            let full = self
                .turn_text
                .lock()
                .map(|mut b| std::mem::take(&mut *b))
                .unwrap_or_default();
            if !full.is_empty() {
                self.reflow_citations(&full);
            }
        }
        let _ = self
            .event_tx
            .send(AgentStreamEvent::Finish(FinishEventData {
                session_id: None,
                stop_reason: None,
            }));
    }

    fn emit_error(&self, msg: &str) {
        let _ = self
            .event_tx
            .send(AgentStreamEvent::Error(ErrorEventData::legacy(msg, None)));
    }

    fn emit_info(&self, msg: &str) {
        let _ = self.event_tx.send(AgentStreamEvent::Tips(TipsEventData {
            content: msg.to_owned(),
            tip_type: TipType::Success,
        }));
    }

    fn emit_warning(&self, msg: &str) {
        // Benign, non-fatal diagnostic: emit as Tips{Warning} on the broadcast —
        // NOT an Error — so the AutoWork runner does not read
        // an otherwise-successful turn as failed. See OutputSink::emit_warning.
        let _ = self.event_tx.send(AgentStreamEvent::Tips(TipsEventData {
            content: msg.to_owned(),
            tip_type: TipType::Warning,
        }));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_sink() -> (BackendOutputSink, broadcast::Receiver<AgentStreamEvent>) {
        let (tx, rx) = broadcast::channel(16);
        (BackendOutputSink::new(tx), rx)
    }

    #[test]
    fn emit_text_delta_sends_text_event() {
        let (sink, mut rx) = make_sink();
        sink.emit_text_delta("hello", "msg-1");
        let event = rx.try_recv().unwrap();
        match event {
            AgentStreamEvent::Text(data) => assert_eq!(data.content, "hello"),
            other => panic!("Expected Text, got {:?}", other),
        }
    }

    #[test]
    fn emit_thinking_sends_thinking_event() {
        let (sink, mut rx) = make_sink();
        sink.emit_thinking("analyzing...", "msg-1");
        let event = rx.try_recv().unwrap();
        match event {
            AgentStreamEvent::Thinking(data) => assert_eq!(data.content, "analyzing..."),
            other => panic!("Expected Thinking, got {:?}", other),
        }
    }

    #[test]
    fn emit_tool_call_sends_running_tool_call() {
        let (sink, mut rx) = make_sink();
        sink.emit_tool_call("call_read_1", "Read", r#"{"path":"/tmp/a.txt"}"#);
        let event = rx.try_recv().unwrap();
        match event {
            AgentStreamEvent::ToolCall(data) => {
                assert_eq!(data.name, "Read");
                assert_eq!(data.status, ToolCallStatus::Running);
            }
            other => panic!("Expected ToolCall, got {:?}", other),
        }
    }

    #[test]
    fn retry_identity_uses_the_same_internal_call_id_domain_as_events() {
        let (sink, mut rx) = make_sink();
        let first = ToolCallExecutionContext {
            input: serde_json::json!({ "tasks": ["invalid"] }),
            retry: ToolCallRetryContext {
                retry_group_id: "call-1".to_owned(),
                attempt_no: 1,
                retry_of_call_id: None,
            },
        };
        sink.emit_tool_call_with_context(
            "call-1",
            "nomi_delegate",
            "nomi_delegate",
            r#"{"tasks":["invalid"]}"#,
            &first,
        );
        let first_running = match rx.try_recv().unwrap() {
            AgentStreamEvent::ToolCall(data) => data,
            other => panic!("Expected ToolCall, got {other:?}"),
        };
        assert_eq!(first_running.call_id, "nomi-call-1");
        assert_eq!(
            first_running.retry.as_ref().unwrap().retry_group_id,
            first_running.call_id
        );

        sink.emit_tool_result_with_images_and_context(
            "call-1",
            "nomi_delegate",
            "nomi_delegate",
            true,
            "invalid arguments",
            &[],
            &first,
        );
        let first_terminal = match rx.try_recv().unwrap() {
            AgentStreamEvent::ToolCall(data) => data,
            other => panic!("Expected ToolCall, got {other:?}"),
        };
        assert_eq!(first_terminal.args, first.input);
        assert_eq!(first_terminal.retry, first_running.retry);

        let second = ToolCallExecutionContext {
            input: serde_json::json!({ "tasks": [{ "title": "valid" }] }),
            retry: ToolCallRetryContext {
                retry_group_id: "call-1".to_owned(),
                attempt_no: 2,
                retry_of_call_id: Some("call-1".to_owned()),
            },
        };
        sink.emit_tool_call_with_context(
            "call-2",
            "nomi_delegate",
            "nomi_delegate",
            r#"{"tasks":[{"title":"valid"}]}"#,
            &second,
        );
        let second_running = match rx.try_recv().unwrap() {
            AgentStreamEvent::ToolCall(data) => data,
            other => panic!("Expected ToolCall, got {other:?}"),
        };
        let retry = second_running.retry.unwrap();
        assert_eq!(retry.retry_group_id, "nomi-call-1");
        assert_eq!(retry.retry_of_call_id.as_deref(), Some("nomi-call-1"));
        assert_eq!(retry.attempt_no, 2);
    }

    #[test]
    fn preflight_failure_preserves_rejected_args_and_retry_identity() {
        let (sink, mut rx) = make_sink();
        let context = ToolCallExecutionContext {
            input: serde_json::json!({ "tasks": ["invalid"] }),
            retry: ToolCallRetryContext {
                retry_group_id: "invalid-1".to_owned(),
                attempt_no: 1,
                retry_of_call_id: None,
            },
        };

        sink.emit_tool_result_with_images_and_context(
            "invalid-1",
            "nomi_delegate",
            "nomi_delegate",
            true,
            "invalid arguments",
            &[],
            &context,
        );

        match rx.try_recv().unwrap() {
            AgentStreamEvent::ToolCall(data) => {
                assert_eq!(data.args, context.input);
                assert_eq!(data.input, Some(context.input));
                assert_eq!(data.retry.unwrap().retry_group_id, data.call_id);
            }
            other => panic!("Expected ToolCall, got {other:?}"),
        }
    }

    #[test]
    fn stream_termination_clears_short_lived_result_contexts() {
        let (sink, mut rx) = make_sink();
        let context = ToolCallExecutionContext {
            input: serde_json::json!({ "path": "a" }),
            retry: ToolCallRetryContext {
                retry_group_id: "reused".to_owned(),
                attempt_no: 1,
                retry_of_call_id: None,
            },
        };
        sink.emit_tool_call_with_context(
            "reused",
            "Read",
            "Read",
            r#"{"path":"a"}"#,
            &context,
        );
        let _running = rx.try_recv().unwrap();
        sink.fail_active_tool_calls("interrupted");
        let interrupted = match rx.try_recv().unwrap() {
            AgentStreamEvent::ToolCall(data) => data,
            other => panic!("Expected ToolCall, got {other:?}"),
        };
        assert!(interrupted.retry.is_some());

        sink.emit_tool_result("reused", "Read", true, "late legacy result");
        let late = match rx.try_recv().unwrap() {
            AgentStreamEvent::ToolCall(data) => data,
            other => panic!("Expected ToolCall, got {other:?}"),
        };
        assert!(late.retry.is_none());
        assert_eq!(late.args, serde_json::Value::Null);
    }

    #[test]
    fn auto_continue_marks_active_tool_as_truncated_not_completed() {
        let (sink, mut rx) = make_sink();
        sink.emit_tool_call(
            "call_write_1",
            "Write",
            r#"{"file_path":"/tmp/index.html"}"#,
        );
        let _running = rx.try_recv().unwrap();

        sink.truncate_active_tool_calls_for_auto_continue("output token limit");

        match rx.try_recv().unwrap() {
            AgentStreamEvent::ToolCall(data) => {
                assert_eq!(data.call_id, "nomi-call_write_1");
                assert_eq!(data.name, "Write");
                assert_eq!(data.status, ToolCallStatus::Error);
                assert_eq!(data.input.as_ref().unwrap()["file_path"], "/tmp/index.html");
                assert!(
                    data.output
                        .as_deref()
                        .unwrap()
                        .contains("incomplete tool call was not executed")
                );
            }
            other => panic!("Expected ToolCall, got {:?}", other),
        }
    }

    #[test]
    fn auto_continue_ignores_finished_tool() {
        let (sink, mut rx) = make_sink();
        sink.emit_tool_call("call_read_1", "Read", r#"{"path":"/tmp/a.txt"}"#);
        let _running = rx.try_recv().unwrap();
        sink.emit_tool_result("call_read_1", "Read", false, "ok");
        let _completed = rx.try_recv().unwrap();

        sink.truncate_active_tool_calls_for_auto_continue("output token limit");

        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn fail_active_tool_calls_marks_pending_tool_error_and_drains_it() {
        let (sink, mut rx) = make_sink();
        sink.emit_tool_call(
            "call_write_1",
            "Write",
            r#"{"file_path":"/tmp/index.html"}"#,
        );
        let _running = rx.try_recv().unwrap();

        sink.fail_active_tool_calls("provider rejected the structured arguments");

        match rx.try_recv().unwrap() {
            AgentStreamEvent::ToolCall(data) => {
                assert_eq!(data.call_id, "nomi-call_write_1");
                assert_eq!(data.status, ToolCallStatus::Error);
                assert_eq!(data.description.as_deref(), Some("Tool call failed"));
                assert_eq!(
                    data.output.as_deref(),
                    Some("provider rejected the structured arguments")
                );
            }
            other => panic!("Expected ToolCall, got {:?}", other),
        }

        sink.truncate_active_tool_calls_for_auto_continue("output token limit");
        assert!(rx.try_recv().is_err(), "a failed call must not be recovered twice");
    }

    #[test]
    fn stream_start_fails_stale_tool_before_emitting_start() {
        let (sink, mut rx) = make_sink();
        sink.emit_tool_call("stale", "Write", "{}");
        let _running = rx.try_recv().unwrap();

        sink.emit_stream_start("next-msg");

        match rx.try_recv().unwrap() {
            AgentStreamEvent::ToolCall(data) => {
                assert_eq!(data.call_id, "nomi-stale");
                assert_eq!(data.status, ToolCallStatus::Error);
            }
            other => panic!("Expected stale ToolCall cleanup, got {:?}", other),
        }
        assert!(matches!(rx.try_recv().unwrap(), AgentStreamEvent::Start(_)));
    }

    #[test]
    fn emit_model_activity_sends_agent_status() {
        let (sink, mut rx) = make_sink();
        sink.emit_model_activity("msg-1", "preparing");
        let event = rx.try_recv().unwrap();
        match event {
            AgentStreamEvent::AgentStatus(data) => {
                assert_eq!(data.backend, "nomi");
                assert_eq!(data.status, "preparing");
                assert_eq!(data.agent_name.as_deref(), Some("Nomi"));
            }
            other => panic!("Expected AgentStatus, got {:?}", other),
        }
    }

    #[test]
    fn emit_tool_result_success_sends_completed() {
        let (sink, mut rx) = make_sink();
        sink.emit_tool_result("call_read_1", "Read", false, "file content here");
        let event = rx.try_recv().unwrap();
        match event {
            AgentStreamEvent::ToolCall(data) => {
                assert_eq!(data.name, "Read");
                assert_eq!(data.status, ToolCallStatus::Completed);
            }
            other => panic!("Expected ToolCall, got {:?}", other),
        }
    }

    #[test]
    fn emit_tool_result_error_sends_error_status() {
        let (sink, mut rx) = make_sink();
        sink.emit_tool_result("call_bash_1", "Bash", true, "command failed");
        let event = rx.try_recv().unwrap();
        match event {
            AgentStreamEvent::ToolCall(data) => {
                assert_eq!(data.name, "Bash");
                assert_eq!(data.status, ToolCallStatus::Error);
            }
            other => panic!("Expected ToolCall, got {:?}", other),
        }
    }

    #[test]
    fn emit_warning_is_a_non_failing_tip_not_an_error_event() {
        // Benign, non-fatal diagnostics (autocompact failure, session save/index
        // hiccup, MCP-init failure, /compact failure) must reach the stream as a
        // non-failing Tips{Warning} — NOT an Error. The AutoWork / requirement
        // AutoWork runner classifies any non-retryable Error stream event as a FAILED
        // turn, so routing a benign warning through emit_error would re-pend the
        // requirement / burn an attempt / pause the tag on an otherwise-successful
        // turn (the regression this guards against).
        let (sink, mut rx) = make_sink();
        sink.emit_warning("Failed to save session: disk full");
        match rx.try_recv().expect("a warning event should be emitted") {
            AgentStreamEvent::Tips(data) => {
                assert_eq!(data.tip_type, TipType::Warning);
                assert!(data.content.contains("Failed to save session"));
            }
            other => panic!("emit_warning must be a non-failing Tips(Warning), got {:?}", other),
        }
    }

    #[test]
    fn duplicate_tool_names_use_distinct_internal_call_ids() {
        let (sink, mut rx) = make_sink();

        sink.emit_tool_call("call_a", "Glob", r#"{"pattern":"*.rs"}"#);
        sink.emit_tool_call("call_b", "Glob", r#"{"pattern":"*.toml"}"#);
        sink.emit_tool_result("call_a", "Glob", false, "first");
        sink.emit_tool_result("call_b", "Glob", false, "second");

        let events = (0..4).map(|_| rx.try_recv().unwrap()).collect::<Vec<_>>();

        let mut call_ids = vec![];
        for event in events {
            match event {
                AgentStreamEvent::ToolCall(data) => call_ids.push((data.call_id, data.status)),
                other => panic!("Expected ToolCall, got {:?}", other),
            }
        }

        assert_eq!(call_ids[0].0, "nomi-call_a");
        assert_eq!(call_ids[1].0, "nomi-call_b");
        assert_eq!(call_ids[2].0, "nomi-call_a");
        assert_eq!(call_ids[3].0, "nomi-call_b");
        assert_eq!(call_ids[2].1, ToolCallStatus::Completed);
        assert_eq!(call_ids[3].1, ToolCallStatus::Completed);
    }

    #[test]
    fn whitespace_variant_tool_ids_cannot_alias_a_canonical_active_call() {
        let (sink, mut rx) = make_sink();

        sink.emit_tool_call("x", "Read", r#"{"path":"a"}"#);
        let running = rx.try_recv().unwrap();
        assert!(matches!(
            running,
            AgentStreamEvent::ToolCall(ref data) if data.call_id == "nomi-x"
        ));

        sink.emit_tool_call(" x ", "Read", r#"{"path":"b"}"#);
        sink.emit_tool_call("\tx", "Read", "{}");
        sink.emit_tool_result("x ", "Read", false, "wrong call");
        assert!(
            rx.try_recv().is_err(),
            "non-canonical IDs must not emit or settle a colliding lifecycle"
        );

        sink.emit_tool_result("x", "Read", false, "ok");
        match rx.try_recv().unwrap() {
            AgentStreamEvent::ToolCall(data) => {
                assert_eq!(data.call_id, "nomi-x");
                assert_eq!(data.status, ToolCallStatus::Completed);
            }
            other => panic!("Expected ToolCall, got {other:?}"),
        }
    }

    #[test]
    fn emit_stream_start_sends_start_event() {
        let (sink, mut rx) = make_sink();
        sink.emit_stream_start("msg-1");
        let event = rx.try_recv().unwrap();
        match event {
            AgentStreamEvent::Start(_) => {}
            other => panic!("Expected Start, got {:?}", other),
        }
    }

    #[test]
    fn emit_stream_end_sends_finish_event() {
        let (sink, mut rx) = make_sink();
        sink.emit_stream_end("msg-1", 3, 1000, 500, 100, 200);
        let event = rx.try_recv().unwrap();
        match event {
            AgentStreamEvent::Finish(_) => {}
            other => panic!("Expected Finish, got {:?}", other),
        }
    }

    #[test]
    fn emit_error_sends_error_event() {
        let (sink, mut rx) = make_sink();
        sink.emit_error("something went wrong");
        let event = rx.try_recv().unwrap();
        match event {
            AgentStreamEvent::Error(data) => assert_eq!(data.message, "something went wrong"),
            other => panic!("Expected Error, got {:?}", other),
        }
    }

    #[test]
    fn emit_info_sends_tips_event() {
        let (sink, mut rx) = make_sink();
        sink.emit_info("operation completed");
        let event = rx.try_recv().unwrap();
        match event {
            AgentStreamEvent::Tips(data) => {
                assert_eq!(data.content, "operation completed");
                assert_eq!(data.tip_type, TipType::Success);
            }
            other => panic!("Expected Tips, got {:?}", other),
        }
    }

    #[test]
    fn emit_tool_call_carries_input() {
        let (sink, mut rx) = make_sink();
        sink.emit_tool_call("call_glob_1", "Glob", r#"{"pattern":"**/*.rs"}"#);
        let event = rx.try_recv().unwrap();
        match event {
            AgentStreamEvent::ToolCall(data) => {
                assert_eq!(data.name, "Glob");
                assert_eq!(data.status, ToolCallStatus::Running);
                assert!(data.input.is_some());
                assert_eq!(data.input.unwrap()["pattern"], "**/*.rs");
            }
            other => panic!("Expected ToolCall, got {:?}", other),
        }
    }

    #[test]
    fn emit_tool_result_carries_output_and_matching_call_id() {
        let (sink, mut rx) = make_sink();
        sink.emit_tool_call("call_glob_1", "Glob", r#"{"pattern":"**/*.rs"}"#);
        let start_event = rx.try_recv().unwrap();
        let start_call_id = match &start_event {
            AgentStreamEvent::ToolCall(data) => data.call_id.clone(),
            _ => panic!("Expected ToolCall"),
        };

        sink.emit_tool_result("call_glob_1", "Glob", false, "src/main.rs\nsrc/lib.rs");
        let event = rx.try_recv().unwrap();
        match event {
            AgentStreamEvent::ToolCall(data) => {
                assert_eq!(data.name, "Glob");
                assert_eq!(data.status, ToolCallStatus::Completed);
                assert_eq!(data.call_id, start_call_id);
                assert_eq!(data.output.as_deref(), Some("src/main.rs\nsrc/lib.rs"));
            }
            other => panic!("Expected ToolCall, got {:?}", other),
        }
    }

    #[test]
    fn emit_tool_result_empty_content_omits_output() {
        let (sink, mut rx) = make_sink();
        sink.emit_tool_result("call_glob_1", "Glob", false, "");
        let event = rx.try_recv().unwrap();
        match event {
            AgentStreamEvent::ToolCall(data) => {
                assert!(data.output.is_none());
            }
            other => panic!("Expected ToolCall, got {:?}", other),
        }
    }

    #[test]
    fn valid_image_is_verified_persisted_and_attached_before_completed() {
        const PNG: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII=";
        let workspace = tempfile::tempdir().unwrap();
        let (tx, mut rx) = broadcast::channel(16);
        let sink = BackendOutputSink::new(tx).with_artifact_workspace(workspace.path());
        sink.emit_tool_call("image-1", "image_gen", "{}");
        let _running = rx.try_recv().unwrap();

        let delivery = sink.emit_tool_result_with_images(
            "image-1",
            "image_gen",
            false,
            "",
            &[ToolImage {
                media_type: "image/png".into(),
                data: PNG.into(),
            }],
        );

        assert!(matches!(delivery, ToolMediaDelivery::Delivered { .. }));
        match rx.try_recv().unwrap() {
            AgentStreamEvent::ToolCall(data) => {
                assert_eq!(data.status, ToolCallStatus::Completed);
                assert_eq!(data.artifacts.len(), 1);
                assert!(std::path::Path::new(&data.artifacts[0].path).is_file());
                assert_eq!(data.artifacts[0].mime_type, "image/png");
                assert!(data.output.unwrap().contains("Verified artifacts saved to:"));
            }
            other => panic!("Expected ToolCall, got {other:?}"),
        }
    }

    #[test]
    fn generic_audio_and_resource_descriptor_are_verified_before_completed() {
        use base64::Engine as _;

        let workspace = tempfile::tempdir().unwrap();
        let (tx, mut rx) = broadcast::channel(16);
        let sink = BackendOutputSink::new(tx).with_artifact_workspace(workspace.path());
        sink.emit_tool_call("export-1", "mcp__reports__export", "{}");
        let _running = rx.try_recv().unwrap();
        let mut wav = b"RIFF".to_vec();
        wav.extend_from_slice(&38_u32.to_le_bytes());
        wav.extend_from_slice(b"WAVEfmt ");
        wav.extend_from_slice(&16_u32.to_le_bytes());
        wav.extend_from_slice(&[1, 0, 1, 0]);
        wav.extend_from_slice(&8_000_u32.to_le_bytes());
        wav.extend_from_slice(&8_000_u32.to_le_bytes());
        wav.extend_from_slice(&[1, 0, 8, 0]);
        wav.extend_from_slice(b"data");
        wav.extend_from_slice(&1_u32.to_le_bytes());
        wav.extend_from_slice(&[128, 0]);

        let delivery = sink.emit_tool_result_with_images(
            "export-1",
            "mcp__reports__export",
            false,
            "Resource link: report — https://example.test/report.pdf",
            &[
                ToolImage {
                    media_type: "audio/wav".into(),
                    data: base64::engine::general_purpose::STANDARD.encode(wav),
                },
                ToolImage {
                    media_type: "application/json".into(),
                    data: "eyJ1cmkiOiJodHRwczovL2UifQ==".into(),
                },
            ],
        );

        assert!(matches!(delivery, ToolMediaDelivery::Delivered { .. }));
        match rx.try_recv().unwrap() {
            AgentStreamEvent::ToolCall(data) => {
                assert_eq!(data.status, ToolCallStatus::Completed);
                assert_eq!(data.artifacts.len(), 2);
                assert_eq!(data.artifacts[0].kind, ArtifactKind::Audio);
                assert_eq!(data.artifacts[1].kind, ArtifactKind::Text);
                assert!(data.artifacts.iter().all(|artifact| {
                    std::path::Path::new(&artifact.path).is_file()
                        && artifact.size_bytes > 0
                        && !artifact.sha256.is_empty()
                }));
                let output = data.output.unwrap();
                assert!(output.contains("https://example.test/report.pdf"));
                assert!(output.contains("Verified artifacts saved to:"));
            }
            other => panic!("Expected ToolCall, got {other:?}"),
        }
    }

    #[test]
    fn image_generator_cannot_complete_with_only_a_file_artifact() {
        let workspace = tempfile::tempdir().unwrap();
        let (tx, mut rx) = broadcast::channel(16);
        let sink = BackendOutputSink::new(tx).with_artifact_workspace(workspace.path());

        let delivery = sink.emit_tool_result_with_images(
            "image-1",
            "image_gen",
            false,
            "generated",
            &[ToolImage {
                media_type: "text/plain".into(),
                data: "bm90IGFuIGltYWdl".into(),
            }],
        );

        assert!(matches!(delivery, ToolMediaDelivery::Failed { .. }));
        match rx.try_recv().unwrap() {
            AgentStreamEvent::ToolCall(data) => {
                assert_eq!(data.status, ToolCallStatus::Error);
                assert!(data.artifacts.is_empty());
                assert!(data.output.unwrap().contains("no image artifact"));
            }
            other => panic!("Expected ToolCall, got {other:?}"),
        }
        assert!(!workspace.path().join("nomifun-artifacts").exists());
    }

    #[test]
    fn image_generator_without_an_image_is_failed_not_completed() {
        let workspace = tempfile::tempdir().unwrap();
        let (tx, mut rx) = broadcast::channel(16);
        let sink = BackendOutputSink::new(tx).with_artifact_workspace(workspace.path());

        let delivery = sink.emit_tool_result_with_images("image-1", "image_gen", false, "done", &[]);

        assert!(matches!(delivery, ToolMediaDelivery::Failed { .. }));
        match rx.try_recv().unwrap() {
            AgentStreamEvent::ToolCall(data) => {
                assert_eq!(data.status, ToolCallStatus::Error);
                assert!(data.artifacts.is_empty());
                assert!(data.output.unwrap().contains("returned no image artifact"));
            }
            other => panic!("Expected ToolCall, got {other:?}"),
        }
        assert!(!workspace.path().join("nomifun-artifacts").exists());
    }

    #[test]
    fn report_export_without_a_file_is_failed_not_completed() {
        let workspace = tempfile::tempdir().unwrap();
        let (tx, mut rx) = broadcast::channel(16);
        let sink = BackendOutputSink::new(tx).with_artifact_workspace(workspace.path());

        let delivery = sink.emit_tool_result_with_images(
            "report-1",
            "mcp__reports__export_report",
            false,
            "Report generated successfully",
            &[],
        );

        assert!(matches!(delivery, ToolMediaDelivery::Failed { .. }));
        match rx.try_recv().unwrap() {
            AgentStreamEvent::ToolCall(data) => {
                assert_eq!(data.status, ToolCallStatus::Error);
                assert!(data.artifacts.is_empty());
                assert!(data.output.unwrap().contains("returned no file artifact"));
            }
            other => panic!("Expected ToolCall, got {other:?}"),
        }
    }

    #[test]
    fn exact_format_tools_reject_wrong_format_receipts_before_persistence() {
        const SAMPLE_BASE64: &str =
            "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII=";
        let cases = [
            ("renderPng", "image/jpeg", "PNG image artifact"),
            ("generateMp3", "audio/wav", "MP3 audio artifact"),
            ("exportMp4", "video/webm", "MP4 video artifact"),
            ("exportPdf", "text/plain", "PDF artifact"),
        ];

        for (index, (tool_name, wrong_mime, expected_label)) in cases.into_iter().enumerate() {
            let workspace = tempfile::tempdir().unwrap();
            let (tx, mut rx) = broadcast::channel(8);
            let sink = BackendOutputSink::new(tx).with_artifact_workspace(workspace.path());
            let call_id = format!("wrong-format-{index}");
            sink.begin_artifact_delivery_turn();
            sink.emit_tool_call(&call_id, tool_name, "{}");
            let _running = rx.try_recv().unwrap();

            let delivery = sink.emit_tool_result_with_images(
                &call_id,
                tool_name,
                false,
                "claimed success",
                &[ToolImage {
                    media_type: wrong_mime.to_owned(),
                    data: SAMPLE_BASE64.to_owned(),
                }],
            );

            let ToolMediaDelivery::Failed { error } = delivery else {
                panic!("{tool_name} accepted {wrong_mime}");
            };
            assert!(error.contains(expected_label), "{tool_name}: {error}");
            match rx.try_recv().unwrap() {
                AgentStreamEvent::ToolCall(data) => {
                    assert_eq!(data.status, ToolCallStatus::Error);
                    assert!(data.artifacts.is_empty());
                }
                other => panic!("Expected ToolCall, got {other:?}"),
            }
            assert!(sink.finish_artifact_delivery_turn().is_err());
            assert!(!workspace.path().join("nomifun-artifacts").exists());
        }
    }

    #[test]
    fn requested_image_count_is_a_minimum_verified_receipt_contract() {
        const PNG: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII=";
        let workspace = tempfile::tempdir().unwrap();
        let (tx, mut rx) = broadcast::channel(16);
        let sink = BackendOutputSink::new(tx).with_artifact_workspace(workspace.path());

        sink.emit_tool_call("count-short", "image_gen", r#"{"n":4}"#);
        let _running = rx.try_recv().unwrap();
        let delivery = sink.emit_tool_result_with_images(
            "count-short",
            "image_gen",
            false,
            "generated",
            &[ToolImage {
                media_type: "image/png".into(),
                data: PNG.into(),
            }],
        );
        let ToolMediaDelivery::Failed { error } = delivery else {
            panic!("one receipt incorrectly satisfied n=4");
        };
        assert!(error.contains("expected at least 4"));
        match rx.try_recv().unwrap() {
            AgentStreamEvent::ToolCall(data) => {
                assert_eq!(data.status, ToolCallStatus::Error);
                assert!(data.artifacts.is_empty());
            }
            other => panic!("Expected ToolCall, got {other:?}"),
        }
        assert!(!workspace.path().join("nomifun-artifacts").exists());

        sink.emit_tool_call("count-good", "image_gen", r#"{"num_images":4}"#);
        let _running = rx.try_recv().unwrap();
        let images = (0..4)
            .map(|_| ToolImage {
                media_type: "image/png".into(),
                data: PNG.into(),
            })
            .collect::<Vec<_>>();
        assert!(matches!(
            sink.emit_tool_result_with_images(
                "count-good",
                "image_gen",
                false,
                "generated",
                &images,
            ),
            ToolMediaDelivery::Delivered { .. }
        ));
        match rx.try_recv().unwrap() {
            AgentStreamEvent::ToolCall(data) => {
                assert_eq!(data.status, ToolCallStatus::Completed);
                assert_eq!(data.artifacts.len(), 4);
                assert!(data.artifacts.iter().all(|artifact| {
                    artifact.mime_type == "image/png"
                        && std::path::Path::new(&artifact.path).is_file()
                }));
            }
            other => panic!("Expected ToolCall, got {other:?}"),
        }
    }

    #[test]
    fn long_mcp_identity_cannot_lose_export_pdf_obligation_to_display_hashing() {
        let workspace = tempfile::tempdir().unwrap();
        let (tx, mut rx) = broadcast::channel(8);
        let sink = BackendOutputSink::new(tx).with_artifact_workspace(workspace.path());
        let display_name = "mcp__very_long_server__7f6e5d4c";
        let artifact_identity = format!("{}__export_pdf", "very_long_server_segment_".repeat(20));
        assert!(artifact_contract(display_name).is_none());
        assert_eq!(
            artifact_contract(&artifact_identity).unwrap().requirement,
            ArtifactRequirement::Pdf
        );

        sink.begin_artifact_delivery_turn();
        sink.emit_tool_call_with_artifact_identity(
            "long-pdf",
            display_name,
            &artifact_identity,
            "{}",
        );
        let _running = rx.try_recv().unwrap();
        let delivery = sink.emit_tool_result_with_images_and_artifact_identity(
            "long-pdf",
            display_name,
            &artifact_identity,
            false,
            "PDF exported successfully",
            &[],
        );

        let ToolMediaDelivery::Failed { error } = delivery else {
            panic!("hashed display name bypassed its PDF contract");
        };
        assert!(error.contains("PDF artifact"));
        match rx.try_recv().unwrap() {
            AgentStreamEvent::ToolCall(data) => {
                assert_eq!(data.status, ToolCallStatus::Error);
                assert!(data.artifacts.is_empty());
            }
            other => panic!("Expected ToolCall, got {other:?}"),
        }
        assert!(sink.finish_artifact_delivery_turn().is_err());
    }

    #[test]
    fn freshly_written_declared_output_path_is_verified_and_attached() {
        let workspace = tempfile::tempdir().unwrap();
        let (tx, mut rx) = broadcast::channel(16);
        let sink = BackendOutputSink::new(tx).with_artifact_workspace(workspace.path());
        sink.emit_tool_call(
            "report-path",
            "mcp__reports__export_report",
            r#"{"output_path":"report.md"}"#,
        );
        let _running = rx.try_recv().unwrap();
        std::fs::write(workspace.path().join("report.md"), "# Generated report\n").unwrap();

        let delivery = sink.emit_tool_result_with_images(
            "report-path",
            "mcp__reports__export_report",
            false,
            r#"{"path":"report.md"}"#,
            &[],
        );

        assert!(matches!(delivery, ToolMediaDelivery::Delivered { .. }));
        let artifact = match rx.try_recv().unwrap() {
            AgentStreamEvent::ToolCall(data) => {
                assert_eq!(data.status, ToolCallStatus::Completed);
                assert_eq!(data.artifacts.len(), 1);
                assert!(
                    data.artifacts[0]
                        .relative_path
                        .starts_with("nomifun-artifacts/artifact-")
                );
                assert_eq!(data.artifacts[0].mime_type, "text/markdown");
                data.artifacts[0].clone()
            }
            other => panic!("Expected ToolCall, got {other:?}"),
        };

        // The published receipt is an immutable snapshot. A later tool in the
        // same accepted turn may overwrite or delete the caller-owned path,
        // but that must not invalidate a green terminal delivery.
        std::fs::write(workspace.path().join("report.md"), "# Replaced later\n").unwrap();
        assert_eq!(
            std::fs::read_to_string(&artifact.path).unwrap(),
            "# Generated report\n"
        );
        assert_ne!(artifact.path, workspace.path().join("report.md").to_string_lossy());
    }

    #[test]
    fn artifact_path_contract_over_limit_fails_instead_of_truncating() {
        let workspace = tempfile::tempdir().unwrap();
        let (tx, mut rx) = broadcast::channel(16);
        let sink = BackendOutputSink::new(tx).with_artifact_workspace(workspace.path());
        let paths = (0..=MAX_DECLARED_ARTIFACT_PATHS)
            .map(|index| format!("result-{index}.md"))
            .collect::<Vec<_>>();
        let contract = serde_json::json!({ "outputPaths": paths }).to_string();

        sink.emit_tool_call("too-many-paths", "exportReport", &contract);
        let _running = rx.try_recv().unwrap();
        let delivery = sink.emit_tool_result_with_images(
            "too-many-paths",
            "exportReport",
            false,
            &contract,
            &[],
        );

        assert!(matches!(delivery, ToolMediaDelivery::Failed { .. }));
        match rx.try_recv().unwrap() {
            AgentStreamEvent::ToolCall(data) => {
                assert_eq!(data.status, ToolCallStatus::Error);
                assert!(data.artifacts.is_empty());
                assert!(data.output.unwrap().contains("more than 32 distinct paths"));
            }
            other => panic!("Expected ToolCall, got {other:?}"),
        }
        assert!(!workspace.path().join("nomifun-artifacts").exists());
    }

    #[test]
    fn ordinary_large_execution_json_is_not_misclassified_as_an_artifact() {
        let workspace = tempfile::tempdir().unwrap();
        let (tx, mut rx) = broadcast::channel(16);
        let sink = BackendOutputSink::new(tx).with_artifact_workspace(workspace.path());
        let steps = (0..600)
            .map(|index| {
                serde_json::json!({
                    "id": format!("step-{index}"),
                    "status": "completed",
                    "attempts": [{
                        "sequence": 1,
                        "status": "completed",
                        "output_files": if index == 0 {
                            serde_json::json!(["reports/from-prior-agent.md"])
                        } else {
                            serde_json::json!([])
                        },
                    }],
                    "dependencies": [],
                })
            })
            .collect::<Vec<_>>();
        let content = serde_json::json!({
            "execution": {
                "id": "019f8fcb-1e47-7893-82db-c03aab79a2c4",
                "status": "running",
                "steps": steps,
            }
        })
        .to_string();

        assert!(artifact_contract("nomi_execution_get").is_none());
        sink.emit_tool_call(
            "execution-get",
            "nomi_execution_get",
            r#"{"execution_id":"019f8fcb-1e47-7893-82db-c03aab79a2c4"}"#,
        );
        let _running = rx.try_recv().unwrap();
        let delivery = sink.emit_tool_result_with_images(
            "execution-get",
            "nomi_execution_get",
            false,
            &content,
            &[],
        );

        assert!(matches!(delivery, ToolMediaDelivery::Unmanaged));
        match rx.try_recv().unwrap() {
            AgentStreamEvent::ToolCall(data) => {
                assert_eq!(data.status, ToolCallStatus::Completed);
                assert!(data.artifacts.is_empty());
                assert_eq!(data.output.as_deref(), Some(content.as_str()));
                assert!(!data.output.unwrap().contains("Artifact delivery failed"));
            }
            other => panic!("Expected ToolCall, got {other:?}"),
        }
        assert!(!workspace.path().join("nomifun-artifacts").exists());
    }

    #[test]
    fn nested_input_output_path_still_creates_a_pre_call_declaration() {
        let declared = input_artifact_paths(&serde_json::json!({
            "options": {
                "outputPath": "reports/generated.md"
            }
        }));

        assert!(declared.saw_explicit_key);
        assert_eq!(declared.paths, vec!["reports/generated.md"]);
        assert!(declared.errors.is_empty());
    }

    #[test]
    fn output_path_inference_requires_root_or_result_scope() {
        let ordinary = output_artifact_paths(
            r#"{"execution":{"attempts":[{"output_files":["prior.md"]}]}}"#,
            false,
        );
        assert!(!ordinary.saw_explicit_key);
        assert!(ordinary.paths.is_empty());

        let nested_read_model = output_artifact_paths(
            r#"{"result":{"attempts":[{"output_files":["prior.md"]}]}}"#,
            false,
        );
        assert!(!nested_read_model.saw_explicit_key);
        assert!(nested_read_model.paths.is_empty());

        let root_array_history = output_artifact_paths(
            r#"[{"output_files":["prior.md"]}]"#,
            false,
        );
        assert!(!root_array_history.saw_explicit_key);
        assert!(root_array_history.paths.is_empty());

        let root_array_declaration = output_artifact_paths(
            r#"[{"outputPath":"generated.md"}]"#,
            false,
        );
        assert!(root_array_declaration.saw_explicit_key);
        assert_eq!(root_array_declaration.paths, vec!["generated.md"]);

        let declared = output_artifact_paths(
            r#"{"result":{"output_files":["generated.md"]}}"#,
            false,
        );
        assert!(declared.saw_explicit_key);
        assert_eq!(declared.paths, vec!["generated.md"]);
    }

    #[test]
    fn explicit_artifact_declaration_after_json_limit_still_fails_closed() {
        let workspace = tempfile::tempdir().unwrap();
        let (tx, mut rx) = broadcast::channel(16);
        let sink = BackendOutputSink::new(tx).with_artifact_workspace(workspace.path());
        sink.emit_tool_call(
            "large-explicit-artifact",
            "mcp__vendor__worker",
            "{}",
        );
        let _running = rx.try_recv().unwrap();
        std::fs::write(workspace.path().join("report.md"), "# Generated\n").unwrap();

        let mut records = (0..600)
            .map(|index| serde_json::json!({ "index": index, "status": "completed" }))
            .collect::<Vec<_>>();
        records.push(serde_json::json!({ "outputPath": "report.md" }));
        let content = serde_json::Value::Array(records).to_string();
        let delivery = sink.emit_tool_result_with_images(
            "large-explicit-artifact",
            "mcp__vendor__worker",
            false,
            &content,
            &[],
        );

        assert!(matches!(delivery, ToolMediaDelivery::Failed { .. }));
        match rx.try_recv().unwrap() {
            AgentStreamEvent::ToolCall(data) => {
                assert_eq!(data.status, ToolCallStatus::Error);
                assert!(data.artifacts.is_empty());
                assert!(
                    data.output
                        .unwrap()
                        .contains("artifact contract JSON exceeds 512 nodes")
                );
            }
            other => panic!("Expected ToolCall, got {other:?}"),
        }
        assert!(!workspace.path().join("nomifun-artifacts").exists());
    }

    #[test]
    fn existing_artifact_contract_keeps_large_json_limit_fail_closed() {
        let workspace = tempfile::tempdir().unwrap();
        let (tx, mut rx) = broadcast::channel(16);
        let sink = BackendOutputSink::new(tx).with_artifact_workspace(workspace.path());
        assert!(artifact_contract("exportPdf").is_some());
        sink.emit_tool_call("large-pdf-result", "exportPdf", "{}");
        let _running = rx.try_recv().unwrap();
        let content = serde_json::Value::Array(
            (0..600)
                .map(|index| serde_json::json!({ "index": index, "status": "completed" }))
                .collect(),
        )
        .to_string();
        let delivery = sink.emit_tool_result_with_images(
            "large-pdf-result",
            "exportPdf",
            false,
            &content,
            &[],
        );

        assert!(matches!(delivery, ToolMediaDelivery::Failed { .. }));
        match rx.try_recv().unwrap() {
            AgentStreamEvent::ToolCall(data) => {
                assert_eq!(data.status, ToolCallStatus::Error);
                assert!(data.artifacts.is_empty());
                assert!(
                    data.output
                        .unwrap()
                        .contains("artifact contract JSON exceeds 512 nodes")
                );
            }
            other => panic!("Expected ToolCall, got {other:?}"),
        }
        assert!(!workspace.path().join("nomifun-artifacts").exists());
    }

    #[test]
    fn result_only_declared_path_without_pre_call_baseline_fails_closed() {
        let workspace = tempfile::tempdir().unwrap();
        std::fs::write(workspace.path().join("old-report.md"), "# Old report\n").unwrap();
        let (tx, mut rx) = broadcast::channel(16);
        let sink = BackendOutputSink::new(tx).with_artifact_workspace(workspace.path());

        let delivery = sink.emit_tool_result_with_images(
            "report-path",
            "mcp__reports__export_report",
            false,
            r#"{"outputPath":"old-report.md"}"#,
            &[],
        );

        assert!(matches!(delivery, ToolMediaDelivery::Failed { .. }));
        match rx.try_recv().unwrap() {
            AgentStreamEvent::ToolCall(data) => {
                assert_eq!(data.status, ToolCallStatus::Error);
                assert!(data.artifacts.is_empty());
                assert!(data.output.unwrap().contains("no pre-call baseline"));
            }
            other => panic!("Expected ToolCall, got {other:?}"),
        }
    }

    #[test]
    fn unchanged_preexisting_artifact_and_missing_artifact_both_fail_closed() {
        let workspace = tempfile::tempdir().unwrap();
        std::fs::write(workspace.path().join("old-report.md"), "# Old report\n").unwrap();
        let (tx, mut rx) = broadcast::channel(16);
        let sink = BackendOutputSink::new(tx).with_artifact_workspace(workspace.path());

        sink.emit_tool_call(
            "old",
            "exportReport",
            r#"{"outputPath":"old-report.md"}"#,
        );
        let _running = rx.try_recv().unwrap();
        let old_delivery = sink.emit_tool_result_with_images(
            "old",
            "exportReport",
            false,
            r#"{"outputPath":"old-report.md"}"#,
            &[],
        );
        assert!(matches!(old_delivery, ToolMediaDelivery::Failed { .. }));
        match rx.try_recv().unwrap() {
            AgentStreamEvent::ToolCall(data) => {
                assert_eq!(data.status, ToolCallStatus::Error);
                assert!(data.output.unwrap().contains("unchanged from its pre-call fingerprint"));
            }
            other => panic!("Expected ToolCall, got {other:?}"),
        }

        sink.emit_tool_call(
            "missing",
            "exportReport",
            r#"{"output_path":"never-created.md"}"#,
        );
        let _running = rx.try_recv().unwrap();
        let missing_delivery = sink.emit_tool_result_with_images(
            "missing",
            "exportReport",
            false,
            r#"{"result":{"path":"never-created.md"}}"#,
            &[],
        );
        assert!(matches!(missing_delivery, ToolMediaDelivery::Failed { .. }));
        match rx.try_recv().unwrap() {
            AgentStreamEvent::ToolCall(data) => {
                assert_eq!(data.status, ToolCallStatus::Error);
                assert!(data.output.unwrap().contains("still missing"));
            }
            other => panic!("Expected ToolCall, got {other:?}"),
        }
    }

    #[test]
    fn unknown_tool_with_explicit_output_path_becomes_any_artifact_contract() {
        let workspace = tempfile::tempdir().unwrap();
        let (tx, mut rx) = broadcast::channel(16);
        let sink = BackendOutputSink::new(tx).with_artifact_workspace(workspace.path());
        sink.emit_tool_call(
            "custom",
            "mcp__vendor__worker",
            r#"{"artifacts_paths":["custom.bin"]}"#,
        );
        let _running = rx.try_recv().unwrap();
        std::fs::write(workspace.path().join("custom.bin"), b"generated bytes").unwrap();

        let delivery = sink.emit_tool_result_with_images(
            "custom",
            "mcp__vendor__worker",
            false,
            r#"{"resultsFiles":["custom.bin"]}"#,
            &[],
        );
        assert!(matches!(delivery, ToolMediaDelivery::Delivered { .. }));
        match rx.try_recv().unwrap() {
            AgentStreamEvent::ToolCall(data) => {
                assert_eq!(data.status, ToolCallStatus::Completed);
                assert_eq!(data.artifacts.len(), 1);
                assert!(
                    data.artifacts[0]
                        .relative_path
                        .starts_with("nomifun-artifacts/artifact-")
                );
            }
            other => panic!("Expected ToolCall, got {other:?}"),
        }

        std::fs::write(workspace.path().join("untracked.bin"), b"old bytes").unwrap();
        let result_only = sink.emit_tool_result_with_images(
            "untracked",
            "mcp__vendor__worker",
            false,
            r#"{"artifactPath":"untracked.bin"}"#,
            &[],
        );
        assert!(matches!(result_only, ToolMediaDelivery::Failed { .. }));
        match rx.try_recv().unwrap() {
            AgentStreamEvent::ToolCall(data) => {
                assert_eq!(data.status, ToolCallStatus::Error);
                assert!(data.output.unwrap().contains("no pre-call baseline"));
            }
            other => panic!("Expected ToolCall, got {other:?}"),
        }
    }

    #[test]
    fn nested_source_paths_are_never_published_as_generated_outputs() {
        let workspace = tempfile::tempdir().unwrap();
        std::fs::write(workspace.path().join("source.md"), "# Source\n").unwrap();
        let (tx, mut rx) = broadcast::channel(16);
        let sink = BackendOutputSink::new(tx).with_artifact_workspace(workspace.path());
        sink.emit_tool_call(
            "source-output",
            "exportReport",
            r#"{"input":{"path":"source.md"},"output_path":"report.md"}"#,
        );
        let _running = rx.try_recv().unwrap();
        std::fs::write(workspace.path().join("report.md"), "# Generated\n").unwrap();

        let delivery = sink.emit_tool_result_with_images(
            "source-output",
            "exportReport",
            false,
            r#"{"source":{"path":"source.md"},"output":{"path":"report.md"}}"#,
            &[],
        );
        assert!(matches!(delivery, ToolMediaDelivery::Delivered { .. }));
        match rx.try_recv().unwrap() {
            AgentStreamEvent::ToolCall(data) => {
                assert_eq!(data.status, ToolCallStatus::Completed);
                assert_eq!(data.artifacts.len(), 1);
                assert!(
                    data.artifacts[0]
                        .relative_path
                        .starts_with("nomifun-artifacts/artifact-")
                );
                assert_eq!(
                    std::fs::read_to_string(&data.artifacts[0].path).unwrap(),
                    "# Generated\n"
                );
            }
            other => panic!("Expected ToolCall, got {other:?}"),
        }
    }

    #[test]
    fn failed_images_and_context_screenshots_are_never_persisted() {
        const PNG: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII=";
        let workspace = tempfile::tempdir().unwrap();
        let (tx, mut rx) = broadcast::channel(16);
        let sink = BackendOutputSink::new(tx).with_artifact_workspace(workspace.path());

        sink.emit_tool_call("failed-image", "image_gen", "{}");
        let _running = rx.try_recv().unwrap();
        let failed = sink.emit_tool_result_with_images(
            "failed-image",
            "image_gen",
            true,
            "provider failed",
            &[ToolImage {
                media_type: "image/png".into(),
                data: PNG.into(),
            }],
        );
        assert_eq!(failed, ToolMediaDelivery::Unmanaged);
        match rx.try_recv().unwrap() {
            AgentStreamEvent::ToolCall(data) => {
                assert_eq!(data.status, ToolCallStatus::Error);
                assert!(data.artifacts.is_empty());
            }
            other => panic!("Expected ToolCall, got {other:?}"),
        }

        sink.emit_tool_call("screenshot", "browserScreenshot", "{}");
        let _running = rx.try_recv().unwrap();
        let screenshot = sink.emit_tool_result_with_images(
            "screenshot",
            "browserScreenshot",
            false,
            "captured",
            &[ToolImage {
                media_type: "image/png".into(),
                data: PNG.into(),
            }],
        );
        assert_eq!(screenshot, ToolMediaDelivery::Unmanaged);
        match rx.try_recv().unwrap() {
            AgentStreamEvent::ToolCall(data) => {
                assert_eq!(data.status, ToolCallStatus::Completed);
                assert!(data.artifacts.is_empty());
            }
            other => panic!("Expected ToolCall, got {other:?}"),
        }
        assert!(!workspace.path().join("nomifun-artifacts").exists());
    }

    #[test]
    fn accepted_turn_requires_each_artifact_call_to_complete_with_its_own_receipt() {
        const PNG: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII=";
        let workspace = tempfile::tempdir().unwrap();
        let (tx, mut rx) = broadcast::channel(32);
        let sink = BackendOutputSink::new(tx).with_artifact_workspace(workspace.path());

        sink.begin_artifact_delivery_turn();
        sink.emit_tool_call("good", "image_gen", "{}");
        let _running = rx.try_recv().unwrap();
        assert!(matches!(
            sink.emit_tool_result_with_images(
                "good",
                "image_gen",
                false,
                "done",
                &[ToolImage {
                    media_type: "image/png".into(),
                    data: PNG.into(),
                }],
            ),
            ToolMediaDelivery::Delivered { .. }
        ));
        let _completed = rx.try_recv().unwrap();
        assert!(sink.finish_artifact_delivery_turn().is_ok());

        sink.begin_artifact_delivery_turn();
        sink.emit_tool_call("first-failed", "image_gen", "{}");
        let _running = rx.try_recv().unwrap();
        let _ = sink.emit_tool_result_with_images(
            "first-failed",
            "image_gen",
            false,
            "claimed success",
            &[],
        );
        let _failed = rx.try_recv().unwrap();
        sink.emit_tool_call("later-good", "image_gen", "{}");
        let _running = rx.try_recv().unwrap();
        let _ = sink.emit_tool_result_with_images(
            "later-good",
            "image_gen",
            false,
            "done",
            &[ToolImage {
                media_type: "image/png".into(),
                data: PNG.into(),
            }],
        );
        let _completed = rx.try_recv().unwrap();
        let error = sink.finish_artifact_delivery_turn().unwrap_err();
        assert!(error.contains("first-failed") || error.contains("image_gen"));

        sink.begin_artifact_delivery_turn();
        sink.emit_tool_call("still-running", "exportReport", r#"{"output_path":"x.md"}"#);
        let _running = rx.try_recv().unwrap();
        sink.emit_stream_start("continuation");
        let _failed = rx.try_recv().unwrap();
        let _start = rx.try_recv().unwrap();
        assert!(sink.finish_artifact_delivery_turn().is_err());
    }

    #[test]
    fn accepted_turn_reverifies_receipts_after_all_later_tools_finish() {
        const PNG: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII=";
        let workspace = tempfile::tempdir().unwrap();
        let (tx, mut rx) = broadcast::channel(16);
        let sink = BackendOutputSink::new(tx).with_artifact_workspace(workspace.path());

        sink.begin_artifact_delivery_turn();
        // `flux` deliberately has no classifier-derived pre-call expectation;
        // the actual published receipt must still be enrolled in the ledger.
        sink.emit_tool_call("image-delete", "flux", "{}");
        let _running = rx.try_recv().unwrap();
        let delivery = sink.emit_tool_result_with_images(
            "image-delete",
            "flux",
            false,
            "done",
            &[ToolImage {
                media_type: "image/png".into(),
                data: PNG.into(),
            }],
        );
        assert!(matches!(delivery, ToolMediaDelivery::Delivered { .. }));
        let receipt_path = match rx.try_recv().unwrap() {
            AgentStreamEvent::ToolCall(data) => data.artifacts[0].path.clone(),
            other => panic!("Expected ToolCall, got {other:?}"),
        };

        // Simulate a later shell tool deleting the locator after the image
        // call completed but before the accepted user turn reached Finish.
        std::fs::remove_file(receipt_path).unwrap();
        let error = sink.finish_artifact_delivery_turn().unwrap_err();
        assert!(error.contains("failed final verification"));
    }

    #[test]
    fn invalid_declared_path_prevents_partial_inline_persistence() {
        const PNG: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII=";
        let workspace = tempfile::tempdir().unwrap();
        let (tx, mut rx) = broadcast::channel(16);
        let sink = BackendOutputSink::new(tx).with_artifact_workspace(workspace.path());
        sink.emit_tool_call("image-mixed", "mcp__openai__image_gen", "{}");
        let _running = rx.try_recv().unwrap();

        let delivery = sink.emit_tool_result_with_images(
            "image-mixed",
            "mcp__openai__image_gen",
            false,
            r#"{"artifactPath":"missing.png"}"#,
            &[ToolImage {
                media_type: "image/png".into(),
                data: PNG.into(),
            }],
        );

        assert!(matches!(delivery, ToolMediaDelivery::Failed { .. }));
        assert!(!workspace.path().join("nomifun-artifacts").exists());
        match rx.try_recv().unwrap() {
            AgentStreamEvent::ToolCall(data) => {
                assert_eq!(data.status, ToolCallStatus::Error);
                assert!(data.artifacts.is_empty());
            }
            other => panic!("Expected ToolCall, got {other:?}"),
        }
    }

    #[test]
    fn invalid_inline_member_rolls_back_valid_path_snapshot_batch() {
        let workspace = tempfile::tempdir().unwrap();
        let (tx, mut rx) = broadcast::channel(16);
        let sink = BackendOutputSink::new(tx).with_artifact_workspace(workspace.path());
        sink.emit_tool_call(
            "mixed-invalid-inline",
            "exportReport",
            r#"{"outputPath":"report.md"}"#,
        );
        let _running = rx.try_recv().unwrap();
        std::fs::write(workspace.path().join("report.md"), "# Valid report\n").unwrap();

        let delivery = sink.emit_tool_result_with_images(
            "mixed-invalid-inline",
            "exportReport",
            false,
            r#"{"outputPath":"report.md"}"#,
            &[ToolImage {
                media_type: "image/png".into(),
                data: "bm90IGFuIGltYWdl".into(),
            }],
        );

        assert!(matches!(delivery, ToolMediaDelivery::Failed { .. }));
        assert!(!workspace.path().join("nomifun-artifacts").exists());
        match rx.try_recv().unwrap() {
            AgentStreamEvent::ToolCall(data) => {
                assert_eq!(data.status, ToolCallStatus::Error);
                assert!(data.artifacts.is_empty());
            }
            other => panic!("Expected ToolCall, got {other:?}"),
        }
    }

    #[test]
    fn invalid_image_bytes_fail_delivery_without_creating_a_receipt() {
        let workspace = tempfile::tempdir().unwrap();
        let (tx, mut rx) = broadcast::channel(16);
        let sink = BackendOutputSink::new(tx).with_artifact_workspace(workspace.path());

        let delivery = sink.emit_tool_result_with_images(
            "image-1",
            "image_gen",
            false,
            "provider said success",
            &[ToolImage {
                media_type: "image/png".into(),
                data: "bm90IGFuIGltYWdl".into(),
            }],
        );

        assert!(matches!(delivery, ToolMediaDelivery::Failed { .. }));
        match rx.try_recv().unwrap() {
            AgentStreamEvent::ToolCall(data) => {
                assert_eq!(data.status, ToolCallStatus::Error);
                assert!(data.artifacts.is_empty());
                assert!(data.output.unwrap().contains("Artifact delivery failed"));
            }
            other => panic!("Expected ToolCall, got {other:?}"),
        }
        assert!(!workspace.path().join("nomifun-artifacts").exists());
    }

    #[test]
    fn no_panic_when_no_receivers() {
        let (tx, _) = broadcast::channel(16);
        let sink = BackendOutputSink::new(tx);
        sink.emit_text_delta("hello", "msg-1");
        sink.emit_thinking("thought", "msg-1");
        sink.emit_tool_call("call_read_1", "Read", "{}");
        sink.emit_tool_result("call_read_1", "Read", false, "ok");
        sink.emit_stream_start("msg-1");
        sink.emit_stream_end("msg-1", 1, 100, 50, 0, 0);
        sink.emit_error("err");
        sink.emit_info("info");
    }

    #[test]
    fn update_plan_result_emits_plan_event() {
        let (sink, mut rx) = make_sink();
        let content = r#"{"kind":"plan_update","explanation":null,"entries":[{"content":"a","status":"completed"},{"content":"b","status":"in_progress"}]}"#;
        sink.emit_tool_call("call_1", "update_plan", r#"{"plan":[]}"#);
        assert!(matches!(rx.try_recv().unwrap(), AgentStreamEvent::ToolCall(_)));
        sink.emit_tool_result("call_1", "update_plan", false, content);
        match rx.try_recv().unwrap() {
            AgentStreamEvent::Plan(data) => {
                assert_eq!(data.session_id.as_deref(), Some("update_plan"));
                assert_eq!(data.source_call_id.as_deref(), Some("nomi-call_1"));
                assert_eq!(data.entries.len(), 2);
                assert_eq!(data.entries[1]["status"], "in_progress");
            }
            other => panic!("expected Plan, got {other:?}"),
        }
        sink.truncate_active_tool_calls_for_auto_continue("max_tokens");
        // The successful plan result must settle the source tool without
        // emitting a synthetic continuation recovery later.
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn update_plan_with_warning_prefix_still_parses() {
        let (sink, mut rx) = make_sink();
        let content = "[note] 2 steps are in_progress; convention is exactly one. Plan rendered as submitted.\n{\"kind\":\"plan_update\",\"explanation\":null,\"entries\":[{\"content\":\"a\",\"status\":\"in_progress\"}]}";
        sink.emit_tool_result("call_1", "update_plan", false, content);
        match rx.try_recv().unwrap() {
            AgentStreamEvent::Plan(data) => assert_eq!(data.entries.len(), 1),
            other => panic!("expected Plan, got {other:?}"),
        }
    }

    #[test]
    fn update_plan_unparsable_falls_through_to_toolcall() {
        let (sink, mut rx) = make_sink();
        sink.emit_tool_result("call_1", "update_plan", false, "not json");
        assert!(matches!(rx.try_recv().unwrap(), AgentStreamEvent::ToolCall(_)));
    }

    // -- citation reflow ------------------------------------------------------

    #[test]
    fn citation_reflow_bumps_cited_file_on_stream_end() {
        use nomi_memory::store::{read_memory, write_memory};
        use nomi_memory::types::{MemoryEntry, MemoryType};

        let tmp = tempfile::tempdir().unwrap();
        let entry = MemoryEntry::build("role", "user role", MemoryType::User, "senior dev");
        let path = write_memory(tmp.path(), &entry).unwrap();
        let filename = path.file_name().unwrap().to_str().unwrap().to_owned();

        let (tx, _rx) = broadcast::channel(16);
        let sink = BackendOutputSink::new(tx).with_distill_dir(Some(tmp.path().to_path_buf()));

        sink.emit_stream_start("m1");
        sink.emit_text_delta("Here is the answer.\n\n<nomi-mem-citation>\n", "m1");
        sink.emit_text_delta(&format!("{filename}|note=[used role]\n"), "m1");
        sink.emit_text_delta("</nomi-mem-citation>", "m1");
        sink.emit_stream_end("m1", 1, 10, 5, 0, 0);

        let read_back = read_memory(&path).unwrap();
        assert_eq!(read_back.frontmatter.usage_count, Some(1));
        assert!(read_back.frontmatter.last_used.is_some());
    }

    #[test]
    fn no_distill_dir_means_no_reflow_and_no_accumulation() {
        // Without a distill dir, the sink must not touch any file (and the
        // text buffer is never used).
        let (tx, _rx) = broadcast::channel(16);
        let sink = BackendOutputSink::new(tx); // distill_dir = None
        sink.emit_stream_start("m1");
        sink.emit_text_delta("<nomi-mem-citation>\nuser_role.md|note=[x]\n</nomi-mem-citation>", "m1");
        sink.emit_stream_end("m1", 1, 10, 5, 0, 0);
        // Nothing to assert beyond "did not panic / did not write" — the
        // turn_text buffer stays empty because distill_dir is None.
        assert!(sink.turn_text.lock().unwrap().is_empty());
    }
}
