use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, RwLock};

use jsonschema::{PatternOptions, Validator};
use nomi_types::tool::ToolDef;
use serde_json::Value;

use crate::Tool;

pub(crate) const MAX_DEFERRED_SEARCH_MATCHES: usize = 5;
const RESERVED_PROVIDER_NAME_PREFIXES: &[&str] = &["mcp__"];
const MAX_TOOL_SCHEMA_BYTES: usize = 512 * 1024;
const MAX_TOOL_SCHEMA_NODES: usize = 16_384;
const MAX_TOOL_SCHEMA_DEPTH: usize = 64;
const MAX_INPUT_VALIDATION_ERRORS: usize = 6;
const MAX_SINGLE_VALIDATION_ERROR_BYTES: usize = 512;
const MAX_INPUT_VALIDATION_MESSAGE_BYTES: usize = 4 * 1024;

/// Session-scoped state for deferred tools whose full schemas have been
/// activated by [`crate::tool_search::ToolSearchTool`].
///
/// The registry and ToolSearch share this handle. ToolSearch mutates it after a
/// successful match, and the registry consults it every time it builds the next
/// provider request. Persisted activation identities may wait here for dynamic
/// registration; ordered sets keep session snapshots stable even when a
/// tool's provider-visible display name changes between runs.
#[derive(Clone, Default)]
pub struct DeferredToolState {
    inner: Arc<RwLock<DeferredToolStateInner>>,
}

#[derive(Default)]
struct DeferredToolStateInner {
    /// Search catalog keyed by the current provider-visible display name.
    catalog: BTreeMap<String, DeferredCatalogEntry>,
    /// Stable activation identities, never provider-visible display aliases.
    activated: BTreeSet<String>,
    /// Restored session activations whose dynamic tools are not registered yet.
    pending_restored: BTreeSet<String>,
}

#[derive(Clone)]
struct DeferredCatalogEntry {
    definition: ToolDef,
    activation_identity: String,
    /// Informational lookup terms only. They must never be used for dispatch,
    /// allowlist policy, or approval because aliases need not be globally unique.
    search_aliases: Vec<String>,
}

impl DeferredToolState {
    /// Whether this tool's full schema should be sent to the provider.
    pub fn is_activated(&self, identity: &str) -> bool {
        self.inner
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .activated
            .contains(identity)
    }

    /// Stable activated identities in deterministic order for session storage.
    pub fn activated_identities(&self) -> Vec<String> {
        self.inner
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .activated
            .iter()
            .cloned()
            .collect()
    }

    /// Restore a session activation. If the tool is registered already it is
    /// activated immediately; otherwise the identity remains pending until a
    /// later dynamic registration (for example pre-message AddMcpServer).
    fn restore_activation(&self, identity: impl Into<String>) {
        let identity = identity.into();
        let mut inner = self
            .inner
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if inner
            .catalog
            .values()
            .any(|entry| entry.activation_identity == identity)
        {
            inner.activated.insert(identity);
        } else {
            inner.pending_restored.insert(identity);
        }
    }

    /// Union of active and not-yet-registered identities for persistence.
    fn session_identities(&self) -> Vec<String> {
        let inner = self
            .inner
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        inner
            .activated
            .union(&inner.pending_restored)
            .cloned()
            .collect()
    }

    pub(crate) fn has_exact_search_term(&self, query: &str) -> bool {
        let query = query.trim();
        self.inner
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .catalog
            .values()
            .any(|entry| {
                entry.definition.name.eq_ignore_ascii_case(query)
                    || entry
                        .search_aliases
                        .iter()
                        .any(|alias| alias.eq_ignore_ascii_case(query))
            })
    }

    /// Register or refresh one deferred definition in the live search catalog.
    fn register_definition(
        &self,
        definition: ToolDef,
        activation_identity: String,
        search_aliases: Vec<String>,
    ) {
        let mut inner = self
            .inner
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if definition.deferred {
            let mut search_aliases: Vec<String> = search_aliases
                .into_iter()
                .map(|alias| alias.trim().to_lowercase())
                .filter(|alias| !alias.is_empty())
                .collect();
            search_aliases.sort();
            search_aliases.dedup();
            let display_name = definition.name.clone();
            inner.catalog.insert(
                display_name,
                DeferredCatalogEntry {
                    definition,
                    activation_identity: activation_identity.clone(),
                    search_aliases,
                },
            );
            if inner.pending_restored.remove(&activation_identity) {
                inner.activated.insert(activation_identity);
            }
        } else {
            if let Some(previous) = inner.catalog.remove(&definition.name) {
                inner.activated.remove(&previous.activation_identity);
            }
            inner.activated.remove(&activation_identity);
            inner.pending_restored.remove(&activation_identity);
        }
    }

    /// Keep the live catalog and activation set aligned with registry filtering.
    fn retain_definitions(&self, names: &BTreeSet<String>) {
        let mut inner = self
            .inner
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let removed_identities: Vec<String> = inner
            .catalog
            .iter()
            .filter(|(display_name, _)| !names.contains(*display_name))
            .map(|(_, entry)| entry.activation_identity.clone())
            .collect();
        inner.catalog.retain(|name, _| names.contains(name));
        for identity in removed_identities {
            inner.activated.remove(&identity);
        }
    }

    /// Remove every searchable definition and activation.
    fn clear(&self) {
        let mut inner = self
            .inner
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        inner.catalog.clear();
        inner.activated.clear();
        inner.pending_restored.clear();
    }

    /// Search the current deferred catalog and atomically activate a bounded,
    /// deterministic set of best matches. A provider-name exact match activates
    /// only that route; an informational-alias exact match activates all tools
    /// sharing that alias (bounded by the cap). Prefix/substring/description
    /// matches follow. Aliases are lookup-only and never authorize execution.
    pub(crate) fn search_and_activate(&self, query: &str) -> Vec<ToolDef> {
        let query = query.trim().to_lowercase();
        if query.is_empty() {
            return Vec::new();
        }
        let mut inner = self
            .inner
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut ranked: Vec<(u8, DeferredCatalogEntry)> = inner
            .catalog
            .values()
            .filter_map(|entry| {
                let definition = &entry.definition;
                let name = definition.name.to_lowercase();
                let description = definition.description.to_lowercase();
                let rank = if name == query {
                    0
                } else if entry.search_aliases.iter().any(|alias| alias == &query) {
                    1
                } else if name.starts_with(&query) {
                    2
                } else if entry
                    .search_aliases
                    .iter()
                    .any(|alias| alias.starts_with(&query))
                {
                    3
                } else if name.contains(&query) {
                    4
                } else if entry
                    .search_aliases
                    .iter()
                    .any(|alias| alias.contains(&query))
                {
                    5
                } else if description.contains(&query) {
                    6
                } else {
                    return None;
                };
                Some((rank, entry.clone()))
            })
            .collect();
        ranked.sort_by(|(left_rank, left), (right_rank, right)| {
            left_rank
                .cmp(right_rank)
                .then_with(|| left.definition.name.cmp(&right.definition.name))
        });
        let exact_rank = ranked
            .first()
            .map(|(rank, _)| *rank)
            .filter(|rank| *rank <= 1);
        let matches: Vec<DeferredCatalogEntry> = ranked
            .into_iter()
            .take_while(|(rank, _)| exact_rank.is_none_or(|exact| *rank == exact))
            .take(MAX_DEFERRED_SEARCH_MATCHES)
            .map(|(_, entry)| entry)
            .collect();
        for entry in &matches {
            inner.pending_restored.remove(&entry.activation_identity);
            inner.activated.insert(entry.activation_identity.clone());
        }
        matches
            .into_iter()
            .map(|entry| entry.definition)
            .collect()
    }
}

enum RegistrationPolicy {
    Unrestricted,
    Allow(BTreeSet<String>),
    DenyAll,
}

impl RegistrationPolicy {
    fn allows(&self, display_name: &str) -> bool {
        match self {
            Self::Unrestricted => true,
            Self::Allow(names) => names.contains(display_name),
            Self::DenyAll => false,
        }
    }

    /// Registration authority is monotonic for the lifetime of a registry:
    /// later filters may narrow an existing allowlist but never widen it.
    fn retain(&mut self, requested: BTreeSet<String>) {
        match self {
            Self::Unrestricted => *self = Self::Allow(requested),
            Self::Allow(existing) => existing.retain(|name| requested.contains(name)),
            Self::DenyAll => {}
        }
    }
}

pub struct ToolRegistry {
    tools: Vec<Box<dyn Tool>>,
    input_contracts: BTreeMap<String, ToolInputContract>,
    deferred_state: DeferredToolState,
    registration_policy: RegistrationPolicy,
}

/// The exact schema advertised for a registered route and its compiled
/// validator must remain one unit. Calling `Tool::input_schema` again during a
/// turn could otherwise let a stateful/dynamic implementation make provider
/// advertisement and validation observe different contracts.
struct ToolInputContract {
    schema: Value,
    validator: Validator,
}

impl Default for ToolRegistry {
    fn default() -> Self {
        Self::new()
    }
}
impl ToolRegistry {
    pub fn new() -> Self {
        Self {
            tools: Vec::new(),
            input_contracts: BTreeMap::new(),
            deferred_state: DeferredToolState::default(),
            registration_policy: RegistrationPolicy::Unrestricted,
        }
    }

    /// Register a tool, returning `true` only when this exact call inserted it.
    /// Rejected namespace claims, policy denials, and duplicate names/identities
    /// return `false`; callers that publish readiness metadata must use this
    /// result instead of inferring success from a later registry lookup.
    pub fn register(&mut self, tool: Box<dyn Tool>) -> bool {
        if !self.registration_policy_allows(tool.name())
            || !self.can_register_route(tool.as_ref(), &BTreeSet::new(), &BTreeSet::new())
        {
            return false;
        }
        let schema = tool.input_schema();
        let validator = match compile_input_validator(tool.name(), &schema) {
            Ok(validator) => validator,
            Err(error) => {
                tracing::warn!(
                    target: "nomi_tools",
                    tool = %tool.name(),
                    error = %error,
                    "rejecting tool with an unsafe or invalid input schema"
                );
                return false;
            }
        };
        self.insert_registered(tool, schema, validator);
        true
    }

    /// Register the policy-allowed subset of a related tool set atomically.
    /// Persistent policy is applied first, using only each tool's unique
    /// provider-visible name; informational aliases never grant authority.
    /// Every remaining name, activation identity, and namespace claim is then
    /// preflighted against both the live registry and the rest of the allowed
    /// subset. If any allowed member conflicts, none are inserted, preventing
    /// one MCP server from mixing old and new manager routes.
    ///
    /// Returns the exact provider names inserted. An empty result means either
    /// the policy allowed no member or an allowed member conflicted.
    pub fn register_batch(&mut self, tools: Vec<Box<dyn Tool>>) -> Vec<String> {
        let tools: Vec<Box<dyn Tool>> = tools
            .into_iter()
            .filter(|tool| self.registration_policy_allows(tool.name()))
            .collect();
        if tools.is_empty() {
            return Vec::new();
        }

        let mut pending_names = BTreeSet::new();
        let mut pending_identities = BTreeSet::new();
        for tool in &tools {
            if !self.can_register_route(tool.as_ref(), &pending_names, &pending_identities) {
                return Vec::new();
            }
            pending_names.insert(tool.name().to_owned());
            pending_identities.insert(tool.activation_identity().to_owned());
        }
        let mut prepared = Vec::with_capacity(tools.len());
        for tool in tools {
            let schema = tool.input_schema();
            let validator = match compile_input_validator(tool.name(), &schema) {
                Ok(validator) => validator,
                Err(error) => {
                    tracing::warn!(
                        target: "nomi_tools",
                        tool = %tool.name(),
                        error = %error,
                        "rejecting tool batch with an unsafe or invalid input schema"
                    );
                    return Vec::new();
                }
            };
            prepared.push((tool, schema, validator));
        }
        let inserted_names = prepared
            .iter()
            .map(|(tool, _, _)| tool.name().to_owned())
            .collect();
        for (tool, schema, validator) in prepared {
            self.insert_registered(tool, schema, validator);
        }
        inserted_names
    }

    fn registration_policy_allows(&self, name: &str) -> bool {
        if self.registration_policy.allows(name) {
            return true;
        }
        tracing::warn!(
            target: "nomi_tools",
            tool = %name,
            "rejecting tool registration outside the registry's persistent allow policy"
        );
        false
    }

    fn can_register_route(
        &self,
        tool: &dyn Tool,
        pending_names: &BTreeSet<String>,
        pending_identities: &BTreeSet<String>,
    ) -> bool {
        let name = tool.name().to_owned();
        let claimed_prefix = tool.reserved_provider_name_prefix();
        let reserved_prefix = RESERVED_PROVIDER_NAME_PREFIXES
            .iter()
            .copied()
            .find(|prefix| name.starts_with(prefix));
        if reserved_prefix != claimed_prefix
            && (reserved_prefix.is_some() || claimed_prefix.is_some())
        {
            tracing::warn!(
                target: "nomi_tools",
                tool = %name,
                ?claimed_prefix,
                ?reserved_prefix,
                "rejecting invalid claim on a reserved provider tool-name namespace"
            );
            return false;
        }
        if self.tools.iter().any(|existing| existing.name() == name)
            || pending_names.contains(&name)
        {
            tracing::warn!(
                target: "nomi_tools",
                tool = %name,
                "rejecting duplicate tool registration to preserve the existing tool and unique provider names"
            );
            return false;
        }
        let activation_identity = tool.activation_identity().to_owned();
        if self
            .tools
            .iter()
            .any(|existing| existing.activation_identity() == activation_identity)
            || pending_identities.contains(&activation_identity)
        {
            tracing::warn!(
                target: "nomi_tools",
                tool = %name,
                identity = %activation_identity,
                "rejecting duplicate tool activation identity"
            );
            return false;
        }
        true
    }

    fn insert_registered(
        &mut self,
        tool: Box<dyn Tool>,
        input_schema: Value,
        validator: Validator,
    ) {
        let name = tool.name().to_owned();
        let activation_identity = tool.activation_identity().to_owned();
        let search_aliases = tool.deferred_search_aliases();
        let definition = ToolDef {
            name: name.clone(),
            description: tool.description().to_string(),
            input_schema: input_schema.clone(),
            deferred: tool.is_deferred(),
        };
        self.tools.push(tool);
        self.input_contracts.insert(
            name,
            ToolInputContract {
                schema: input_schema,
                validator,
            },
        );
        self.deferred_state
            .register_definition(definition, activation_identity, search_aliases);
    }

    /// Remove every registered tool.
    /// Unlike an empty allowlist, this is an explicit deny-all operation.
    pub fn clear(&mut self) {
        self.tools.clear();
        self.input_contracts.clear();
        self.deferred_state.clear();
        self.registration_policy = RegistrationPolicy::DenyAll;
    }

    /// Find a tool by name
    pub fn get(&self, name: &str) -> Option<&dyn Tool> {
        self.tools
            .iter()
            .find(|t| t.name() == name)
            .map(|t| t.as_ref())
    }

    /// Validate the provider-supplied input for a registered tool.
    ///
    /// Callers must invoke this before approval/UI/hook evaluation and before
    /// dispatch. The returned text is intentionally model-facing: it includes
    /// instance paths and enough schema detail for the model to correct its
    /// next call without ever running the invalid invocation.
    pub fn validate_input(&self, name: &str, input: &Value) -> Result<(), String> {
        let Some(contract) = self.input_contracts.get(name) else {
            return Err(format!(
                "Unknown or unauthorized tool '{name}'; the tool was not executed."
            ));
        };
        validate_input_contract(name, contract, input)
    }

    /// Canonicalize a provider payload and validate the exact value that
    /// approval, hooks, lifecycle output, and dispatch will observe.
    ///
    /// Some OpenAI-compatible providers stringify nested JSON values even when
    /// the advertised schema requires an object, array, number, or boolean.
    /// Recover only those schema-directed fields, and accept the recovery only
    /// when the complete cached validator succeeds. Unknown fields, invalid
    /// enum values, mixed union branches, and whole-object strings stay invalid;
    /// no field is removed or guessed.
    pub fn prepare_input(&self, name: &str, input: Value) -> Result<Value, String> {
        let Some(contract) = self.input_contracts.get(name) else {
            return Err(format!(
                "Unknown or unauthorized tool '{name}'; the tool was not executed."
            ));
        };

        let original_error = match validate_input_contract(name, contract, &input) {
            Ok(()) => return Ok(input),
            Err(error) => error,
        };

        let candidate = coerce_object_fields_to_schema(&contract.schema, input.clone());
        if candidate != input && validate_input_contract(name, contract, &candidate).is_ok() {
            return Ok(candidate);
        }

        Err(original_error)
    }

    /// Get all registered tool names
    pub fn tool_names(&self) -> Vec<String> {
        self.tools.iter().map(|t| t.name().to_string()).collect()
    }

    /// Shared activation state used by ToolSearch and this registry.
    pub fn deferred_state(&self) -> DeferredToolState {
        self.deferred_state.clone()
    }

    /// Restore a persisted activation without requiring the dynamic tool to be
    /// registered yet. Registration later in this session consumes the pending
    /// identity atomically and exposes the full schema immediately.
    pub fn restore_deferred_tool_activation(&self, identity: &str) {
        if !identity.trim().is_empty() {
            self.deferred_state
                .restore_activation(identity.to_owned());
        }
    }

    /// Activated deferred identities in stable order for session persistence.
    pub fn activated_deferred_tool_identities(&self) -> Vec<String> {
        self.deferred_state.activated_identities()
    }

    /// Deferred activation identities to persist, including restored identities
    /// waiting for a dynamic tool to be registered.
    pub fn session_deferred_tool_identities(&self) -> Vec<String> {
        self.deferred_state.session_identities()
    }

    /// Snapshot all tools that are deferred in the current provider turn.
    /// Tool execution captures this once before dispatch so ToolSearch and a
    /// target emitted in the same model response cannot race the gate.
    pub fn provider_deferred_tool_names(&self) -> BTreeSet<String> {
        self.tools
            .iter()
            .filter(|tool| {
                tool.is_deferred()
                    && !self
                        .deferred_state
                        .is_activated(tool.activation_identity())
            })
            .map(|tool| tool.name().to_owned())
            .collect()
    }

    /// Generate API tool definitions for all registered tools
    pub fn to_tool_defs(&self) -> Vec<ToolDef> {
        self.tools
            .iter()
            .map(|t| self.tool_definition(t.as_ref()))
            .collect()
    }

    /// Generate API tool definitions for tools matching a predicate.
    ///
    /// Used by plan mode to restrict the tool set sent to the LLM.
    pub fn to_tool_defs_filtered<F>(&self, filter: F) -> Vec<ToolDef>
    where
        F: Fn(&dyn Tool) -> bool,
    {
        self.tools
            .iter()
            .filter(|t| filter(t.as_ref()))
            .map(|t| self.tool_definition(t.as_ref()))
            .collect()
    }

    fn tool_definition(&self, tool: &dyn Tool) -> ToolDef {
        let contract = self
            .input_contracts
            .get(tool.name())
            .expect("registered tool must have a compiled input contract");
        ToolDef {
            name: tool.name().to_string(),
            description: tool.description().to_string(),
            input_schema: contract.schema.clone(),
            deferred: tool.is_deferred()
                && !self
                    .deferred_state
                    .is_activated(tool.activation_identity()),
        }
    }

    /// Keep only tools named in `allowed` and persist that policy for every
    /// later registration. An empty slice is a no-op, so an absent config does
    /// not change the current policy. Repeated non-empty calls only narrow the
    /// existing authority; they cannot reopen names removed earlier.
    pub fn retain_named(&mut self, allowed: &[String]) {
        if allowed.is_empty() {
            return;
        }
        self.registration_policy
            .retain(allowed.iter().cloned().collect());
        let policy = &self.registration_policy;
        self.tools.retain(|tool| policy.allows(tool.name()));
        let retained_names: BTreeSet<String> =
            self.tools.iter().map(|tool| tool.name().to_owned()).collect();
        self.input_contracts
            .retain(|name, _| retained_names.contains(name));
        self.deferred_state.retain_definitions(&retained_names);
    }
}

fn validate_input_contract(
    name: &str,
    contract: &ToolInputContract,
    input: &Value,
) -> Result<(), String> {
    let mut validation_errors = contract.validator.iter_errors(input);
    let Some(first) = validation_errors.next() else {
        return Ok(());
    };

    let mut messages = Vec::with_capacity(MAX_INPUT_VALIDATION_ERRORS);
    messages.push(format_validation_error(&first));
    for error in validation_errors
        .by_ref()
        .take(MAX_INPUT_VALIDATION_ERRORS - 1)
    {
        messages.push(format_validation_error(&error));
    }
    let more = validation_errors.next().is_some();
    let suffix = if more {
        format!(
            "; additional validation errors omitted after {MAX_INPUT_VALIDATION_ERRORS} issues"
        )
    } else {
        String::new()
    };
    let message = format!(
        "Invalid arguments for tool '{name}': JSON Schema validation failed: {}{suffix}. Correct the arguments and retry; the tool was not executed.",
        messages.join("; ")
    );
    Err(truncate_error_text(
        message,
        MAX_INPUT_VALIDATION_MESSAGE_BYTES,
    ))
}

fn coerce_object_fields_to_schema(schema: &Value, mut input: Value) -> Value {
    let Some(object) = input.as_object_mut() else {
        return input;
    };

    // Schemars represents tagged and untagged request enums with root-level
    // oneOf/anyOf branches, often through local $defs references. Walk every
    // branch that defines a supplied property instead of relying on the root
    // `properties`, which may intentionally be empty.
    let keys = object.keys().cloned().collect::<Vec<_>>();
    for key in keys {
        let mut property_schemas = Vec::new();
        collect_property_schemas(schema, schema, &key, &mut property_schemas, 0);
        let mut expected = Vec::new();
        for property_schema in property_schemas {
            collect_schema_type_names(schema, property_schema, &mut expected, 0);
        }
        // A union that genuinely permits a string is ambiguous. Preserve the
        // provider value and let the complete validator choose the branch.
        if expected.contains(&"string") {
            continue;
        }
        let Some(raw) = object.get(&key).and_then(Value::as_str).map(str::to_owned) else {
            continue;
        };
        if let Some(coerced) = coerce_string_to_types(&raw, &expected) {
            object.insert(key, coerced);
        }
    }

    input
}

fn collect_property_schemas<'a>(
    root: &'a Value,
    schema: &'a Value,
    key: &str,
    out: &mut Vec<&'a Value>,
    depth: usize,
) {
    if depth > 32 {
        return;
    }
    if let Some(resolved) = resolve_local_schema_ref(root, schema) {
        collect_property_schemas(root, resolved, key, out, depth + 1);
        return;
    }
    if let Some(property) = schema
        .get("properties")
        .and_then(Value::as_object)
        .and_then(|properties| properties.get(key))
    {
        out.push(property);
    }
    for branch_key in ["oneOf", "anyOf", "allOf"] {
        if let Some(branches) = schema.get(branch_key).and_then(Value::as_array) {
            for branch in branches {
                collect_property_schemas(root, branch, key, out, depth + 1);
            }
        }
    }
}

fn collect_schema_type_names<'a>(
    root: &'a Value,
    schema: &'a Value,
    out: &mut Vec<&'a str>,
    depth: usize,
) {
    if depth > 32 {
        return;
    }
    if let Some(resolved) = resolve_local_schema_ref(root, schema) {
        collect_schema_type_names(root, resolved, out, depth + 1);
        return;
    }
    match schema.get("type") {
        Some(Value::String(kind)) => out.push(kind),
        Some(Value::Array(kinds)) => {
            for kind in kinds {
                if let Some(kind) = kind.as_str() {
                    out.push(kind);
                }
            }
        }
        _ => {}
    }
    for branch_key in ["oneOf", "anyOf", "allOf"] {
        if let Some(branches) = schema.get(branch_key).and_then(Value::as_array) {
            for branch in branches {
                collect_schema_type_names(root, branch, out, depth + 1);
            }
        }
    }
}

fn resolve_local_schema_ref<'a>(root: &'a Value, schema: &Value) -> Option<&'a Value> {
    let reference = schema.get("$ref")?.as_str()?;
    let pointer = reference.strip_prefix('#')?;
    root.pointer(pointer)
}

fn coerce_string_to_types(raw: &str, expected: &[&str]) -> Option<Value> {
    if (expected.contains(&"array") || expected.contains(&"object"))
        && let Ok(parsed) = serde_json::from_str::<Value>(raw)
        && ((expected.contains(&"array") && parsed.is_array())
            || (expected.contains(&"object") && parsed.is_object()))
    {
        return Some(parsed);
    }

    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    if expected.contains(&"integer")
        && let Ok(number) = trimmed.parse::<i64>()
    {
        return Some(Value::Number(number.into()));
    }
    if expected.contains(&"number")
        && let Ok(number) = trimmed.parse::<f64>()
    {
        return serde_json::Number::from_f64(number).map(Value::Number);
    }
    if expected.contains(&"boolean") {
        if trimmed.eq_ignore_ascii_case("true") {
            return Some(Value::Bool(true));
        }
        if trimmed.eq_ignore_ascii_case("false") {
            return Some(Value::Bool(false));
        }
    }
    None
}

fn compile_input_validator(tool_name: &str, schema: &Value) -> Result<Validator, String> {
    let Some(schema_object) = schema.as_object() else {
        return Err("tool input schema must be a JSON object".to_string());
    };
    if let Some(root_type) = schema_object.get("type") {
        let allows_object = match root_type {
            Value::String(kind) => kind == "object",
            Value::Array(kinds) => kinds.iter().any(|kind| kind.as_str() == Some("object")),
            _ => false,
        };
        if !allows_object {
            return Err("tool input schema root `type` must allow `object`".to_string());
        }
    }
    validate_schema_resource_limits(schema)?;
    jsonschema::options()
        // Tool schemas may originate from dynamic MCP servers. The linear-time
        // regex engine prevents schema-controlled catastrophic backtracking.
        .with_pattern_options(PatternOptions::regex())
        .build(schema)
        .map_err(|error| format!("input schema for '{tool_name}' could not be compiled: {error}"))
}

fn validate_schema_resource_limits(schema: &Value) -> Result<(), String> {
    let encoded_size = serde_json::to_vec(schema)
        .map_err(|error| format!("input schema could not be serialized: {error}"))?
        .len();
    if encoded_size > MAX_TOOL_SCHEMA_BYTES {
        return Err(format!(
            "input schema is {encoded_size} bytes; maximum is {MAX_TOOL_SCHEMA_BYTES}"
        ));
    }

    let mut nodes = 0usize;
    let mut stack = vec![(schema, 0usize)];
    while let Some((value, depth)) = stack.pop() {
        nodes += 1;
        if nodes > MAX_TOOL_SCHEMA_NODES {
            return Err(format!(
                "input schema exceeds the {MAX_TOOL_SCHEMA_NODES}-node structural limit"
            ));
        }
        if depth > MAX_TOOL_SCHEMA_DEPTH {
            return Err(format!(
                "input schema exceeds the maximum nesting depth of {MAX_TOOL_SCHEMA_DEPTH}"
            ));
        }
        match value {
            Value::Object(object) => {
                for (key, child) in object {
                    if matches!(key.as_str(), "$ref" | "$dynamicRef" | "$recursiveRef") {
                        let Some(reference) = child.as_str() else {
                            return Err(format!("schema keyword '{key}' must be a string"));
                        };
                        if !reference.starts_with('#') {
                            return Err(format!(
                                "external schema reference '{reference}' is not allowed; only local '#...' references are permitted"
                            ));
                        }
                    }
                    stack.push((child, depth + 1));
                }
            }
            Value::Array(items) => {
                stack.extend(items.iter().map(|item| (item, depth + 1)));
            }
            _ => {}
        }
    }
    Ok(())
}

fn format_validation_error(error: &jsonschema::ValidationError<'_>) -> String {
    let path = error.instance_path().to_string();
    let path = if path.is_empty() { "$" } else { &path };
    truncate_error_text(
        format!("at {path}: {error}"),
        MAX_SINGLE_VALIDATION_ERROR_BYTES,
    )
}

fn truncate_error_text(message: String, max_bytes: usize) -> String {
    if message.len() <= max_bytes {
        return message;
    }
    const SUFFIX: &str = "…[truncated]";
    let content_budget = max_bytes.saturating_sub(SUFFIX.len());
    format!("{}{}", crate::truncate_utf8(&message, content_budget), SUFFIX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Tool;
    use async_trait::async_trait;
    use nomi_protocol::events::ToolCategory;
    use nomi_types::tool::ToolResult;

    /// A minimal Tool implementation used only in tests
    struct MockTool {
        tool_name: String,
        tool_description: String,
        tool_category: ToolCategory,
    }

    struct SchemaMockTool {
        name: String,
        schema: Value,
    }

    #[async_trait]
    impl Tool for SchemaMockTool {
        fn name(&self) -> &str {
            &self.name
        }

        fn description(&self) -> &str {
            "schema registration fixture"
        }

        fn input_schema(&self) -> Value {
            self.schema.clone()
        }

        fn is_concurrency_safe(&self, _input: &Value) -> bool {
            true
        }

        async fn execute(&self, _input: Value) -> ToolResult {
            ToolResult::text("ok")
        }

        fn category(&self) -> ToolCategory {
            ToolCategory::Info
        }
    }

    fn schema_tool(name: &str, schema: Value) -> Box<SchemaMockTool> {
        Box::new(SchemaMockTool {
            name: name.to_owned(),
            schema,
        })
    }

    #[async_trait]
    impl Tool for MockTool {
        fn name(&self) -> &str {
            &self.tool_name
        }

        fn description(&self) -> &str {
            &self.tool_description
        }

        fn input_schema(&self) -> serde_json::Value {
            serde_json::json!({"type": "object"})
        }

        fn is_concurrency_safe(&self, _input: &serde_json::Value) -> bool {
            true
        }

        async fn execute(&self, _input: serde_json::Value) -> ToolResult {
            ToolResult::text("ok")
        }

        fn category(&self) -> ToolCategory {
            self.tool_category
        }
    }

    /// Helper to create a MockTool with the given name and description
    fn make_tool(name: &str, description: &str) -> Box<MockTool> {
        Box::new(MockTool {
            tool_name: name.to_string(),
            tool_description: description.to_string(),
            tool_category: ToolCategory::Info,
        })
    }

    fn make_tool_with_category(
        name: &str,
        description: &str,
        category: ToolCategory,
    ) -> Box<MockTool> {
        Box::new(MockTool {
            tool_name: name.to_string(),
            tool_description: description.to_string(),
            tool_category: category,
        })
    }

    #[test]
    fn test_register_and_get() {
        let mut registry = ToolRegistry::new();
        registry.register(make_tool("my_tool", "does something"));

        let found = registry.get("my_tool");
        assert!(
            found.is_some(),
            "registered tool should be retrievable by name"
        );
        assert_eq!(found.unwrap().name(), "my_tool");
    }

    #[test]
    fn duplicate_toolsearch_registration_preserves_native_without_provider_duplicates() {
        let mut registry = ToolRegistry::new();
        assert!(registry.register(make_tool("ToolSearch", "native ToolSearch")));
        assert!(!registry.register(Box::new(DeferredMockTool {
            tool_name: "ToolSearch".to_owned(),
        })));

        assert_eq!(registry.tool_names(), vec!["ToolSearch"]);
        let definitions = registry.to_tool_defs();
        assert_eq!(definitions.len(), 1);
        assert_eq!(definitions[0].name, "ToolSearch");
        assert_eq!(definitions[0].description, "native ToolSearch");
        assert!(!definitions[0].deferred);
        assert_eq!(registry.get("ToolSearch").unwrap().description(), "native ToolSearch");
        assert!(registry
            .deferred_state()
            .search_and_activate("ToolSearch")
            .is_empty());
    }

    #[test]
    fn register_batch_is_atomic_when_one_name_conflicts() {
        let mut registry = ToolRegistry::new();
        assert!(registry.register(make_tool("existing", "old route")));

        let accepted = registry.register_batch(vec![
            make_tool("new_route", "must be rolled back"),
            make_tool("existing", "conflicting replacement"),
        ]);

        assert!(accepted.is_empty());
        assert!(registry.get("new_route").is_none());
        assert_eq!(registry.tool_names(), vec!["existing"]);
        assert_eq!(registry.get("existing").unwrap().description(), "old route");
    }

    #[test]
    fn register_batch_is_atomic_when_one_schema_is_not_an_object() {
        let mut registry = ToolRegistry::new();

        let accepted = registry.register_batch(vec![
            schema_tool(
                "valid_properties_only",
                serde_json::json!({
                    "properties": { "kb_id": { "type": "string" } },
                    "required": ["kb_id"]
                }),
            ),
            schema_tool("boolean_schema", Value::Bool(true)),
        ]);

        assert!(accepted.is_empty());
        assert!(registry.get("valid_properties_only").is_none());
        assert!(registry.get("boolean_schema").is_none());
        assert!(registry.to_tool_defs().is_empty());
    }

    #[test]
    fn properties_only_object_schema_registers_and_validates() {
        let mut registry = ToolRegistry::new();
        assert!(registry.register(schema_tool(
            "knowledge_search",
            serde_json::json!({
                "properties": { "kb_id": { "type": "string" } },
                "required": ["kb_id"]
            }),
        )));

        assert!(registry
            .validate_input("knowledge_search", &serde_json::json!({"kb_id": "kb-1"}))
            .is_ok());
        assert!(registry
            .validate_input("knowledge_search", &serde_json::json!({}))
            .unwrap_err()
            .contains("kb_id"));
    }

    fn delegate_union_schema() -> Value {
        serde_json::json!({
            "$defs": {
                "Planned": {
                    "type": "object",
                    "properties": {
                        "strategy": {"type": "string", "const": "planned"},
                        "goal": {"type": "string"},
                        "work_dir": {"type": ["string", "null"]},
                        "model_pool": {
                            "anyOf": [
                                {"$ref": "#/$defs/ModelPool"},
                                {"type": "null"}
                            ]
                        },
                        "max_parallel": {"type": ["integer", "null"]}
                    },
                    "required": ["strategy", "goal"],
                    "additionalProperties": false
                },
                "Parallel": {
                    "type": "object",
                    "properties": {
                        "strategy": {"type": "string", "const": "parallel"},
                        "tasks": {
                            "type": "array",
                            "minItems": 1,
                            "maxItems": 16,
                            "items": {
                                "type": "object",
                                "properties": {
                                    "name": {"type": "string"},
                                    "prompt": {"type": "string"}
                                },
                                "required": ["name", "prompt"],
                                "additionalProperties": false
                            }
                        },
                        "synthesize": {"type": "boolean"}
                    },
                    "required": ["strategy", "tasks"],
                    "additionalProperties": false
                },
                "ModelPool": {
                    "oneOf": [
                        {
                            "type": "object",
                            "properties": {"mode": {"type": "string", "const": "automatic"}},
                            "required": ["mode"],
                            "additionalProperties": false
                        },
                        {
                            "type": "object",
                            "properties": {
                                "mode": {"type": "string", "const": "range"},
                                "models": {"type": "array"}
                            },
                            "required": ["mode", "models"],
                            "additionalProperties": false
                        }
                    ]
                }
            },
            "type": "object",
            "properties": {},
            "anyOf": [
                {"$ref": "#/$defs/Planned"},
                {"$ref": "#/$defs/Parallel"}
            ]
        })
    }

    fn delegate_union_registry() -> ToolRegistry {
        let mut registry = ToolRegistry::new();
        assert!(registry.register(schema_tool("nomi_delegate", delegate_union_schema())));
        registry
    }

    #[test]
    fn prepare_input_recovers_planned_union_fields_through_defs() {
        let registry = delegate_union_registry();
        let raw = serde_json::json!({
            "strategy": "planned",
            "goal": "{keep this as a real string}",
            "work_dir": "/tmp/work",
            "model_pool": "{\"mode\":\"automatic\"}",
            "max_parallel": "4"
        });
        assert!(registry.validate_input("nomi_delegate", &raw).is_err());

        let prepared = registry.prepare_input("nomi_delegate", raw).unwrap();
        assert_eq!(prepared["strategy"], "planned");
        assert_eq!(prepared["goal"], "{keep this as a real string}");
        assert_eq!(prepared["work_dir"], "/tmp/work");
        assert_eq!(prepared["model_pool"], serde_json::json!({"mode": "automatic"}));
        assert_eq!(prepared["max_parallel"], 4);
        assert!(registry.validate_input("nomi_delegate", &prepared).is_ok());
    }

    #[test]
    fn prepare_input_recovers_parallel_array_and_boolean_strings() {
        let registry = delegate_union_registry();
        let raw = serde_json::json!({
            "strategy": "parallel",
            "tasks": "[{\"name\":\"research\",\"prompt\":\"inspect\"}]",
            "synthesize": "True"
        });

        let prepared = registry.prepare_input("nomi_delegate", raw).unwrap();
        assert_eq!(prepared["tasks"][0]["name"], "research");
        assert_eq!(prepared["synthesize"], true);
        assert!(registry.validate_input("nomi_delegate", &prepared).is_ok());
    }

    #[test]
    fn prepare_input_keeps_strict_union_and_root_object_boundaries() {
        let registry = delegate_union_registry();
        for invalid in [
            serde_json::json!({
                "strategy": "planned",
                "goal": "inspect",
                "synthesize": "True"
            }),
            serde_json::json!({
                "strategy": "parallel",
                "tasks": "{\"name\":\"wrong shape\"}"
            }),
            serde_json::json!({
                "strategy": "parallel",
                "tasks": "[]",
                "unknown": "must not be dropped"
            }),
            Value::String(
                "{\"strategy\":\"planned\",\"goal\":\"whole object string\"}".to_owned(),
            ),
        ] {
            assert!(registry.prepare_input("nomi_delegate", invalid).is_err());
        }
    }

    #[test]
    fn validation_error_text_is_bounded_and_does_not_echo_large_input() {
        let mut registry = ToolRegistry::new();
        assert!(registry.register(schema_tool(
            "bounded_error",
            serde_json::json!({
                "type": "object",
                "properties": { "mode": { "enum": ["semantic", "keyword"] } },
                "required": ["mode"]
            }),
        )));
        let oversized = "x".repeat(20_000);

        let error = registry
            .validate_input("bounded_error", &serde_json::json!({"mode": oversized}))
            .unwrap_err();

        assert!(error.len() <= MAX_INPUT_VALIDATION_MESSAGE_BYTES);
        assert!(error.contains("/mode"));
        assert!(!error.contains(&"x".repeat(1_000)));
    }

    #[test]
    fn test_get_nonexistent_returns_none() {
        let registry = ToolRegistry::new();

        let result = registry.get("ghost");
        assert!(
            result.is_none(),
            "looking up an unregistered name should return None"
        );
    }

    #[test]
    fn test_tool_names() {
        let mut registry = ToolRegistry::new();
        registry.register(make_tool("alpha", "first tool"));
        registry.register(make_tool("beta", "second tool"));
        registry.register(make_tool("gamma", "third tool"));

        let mut names = registry.tool_names();
        names.sort(); // sort for a stable assertion order
        assert_eq!(names, vec!["alpha", "beta", "gamma"]);
    }

    #[test]
    fn test_to_tool_defs() {
        let mut registry = ToolRegistry::new();
        registry.register(make_tool("tool_a", "description A"));
        registry.register(make_tool("tool_b", "description B"));

        let defs = registry.to_tool_defs();
        assert_eq!(
            defs.len(),
            2,
            "to_tool_defs should return one entry per registered tool"
        );

        // Collect (name, description) pairs for assertion independent of order
        let mut pairs: Vec<(&str, &str)> = defs
            .iter()
            .map(|d| (d.name.as_str(), d.description.as_str()))
            .collect();
        pairs.sort();

        assert_eq!(pairs[0], ("tool_a", "description A"));
        assert_eq!(pairs[1], ("tool_b", "description B"));

        // Verify the input_schema field is populated correctly
        let expected_schema = serde_json::json!({"type": "object"});
        for def in &defs {
            assert_eq!(def.input_schema, expected_schema);
        }
    }

    // --- to_tool_defs_filtered tests ---

    #[test]
    fn filtered_by_category_returns_matching_tools() {
        let mut registry = ToolRegistry::new();
        registry.register(make_tool_with_category(
            "Read",
            "read files",
            ToolCategory::Info,
        ));
        registry.register(make_tool_with_category(
            "Write",
            "write files",
            ToolCategory::Edit,
        ));
        registry.register(make_tool_with_category(
            "Bash",
            "run commands",
            ToolCategory::Exec,
        ));
        registry.register(make_tool_with_category(
            "ExitPlanMode",
            "exit plan mode",
            ToolCategory::Info,
        ));

        let defs = registry.to_tool_defs_filtered(|t| t.category() == ToolCategory::Info);

        let names: Vec<&str> = defs.iter().map(|d| d.name.as_str()).collect();
        assert!(names.contains(&"Read"));
        assert!(names.contains(&"ExitPlanMode"));
        assert!(!names.contains(&"Write"));
        assert!(!names.contains(&"Bash"));
    }

    #[test]
    fn filtered_by_name_excludes_specific_tool() {
        let mut registry = ToolRegistry::new();
        registry.register(make_tool("alpha", "first"));
        registry.register(make_tool("beta", "second"));
        registry.register(make_tool("gamma", "third"));

        let defs = registry.to_tool_defs_filtered(|t| t.name() != "beta");

        let names: Vec<&str> = defs.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(names.len(), 2);
        assert!(names.contains(&"alpha"));
        assert!(names.contains(&"gamma"));
        assert!(!names.contains(&"beta"));
    }

    #[test]
    fn filtered_accept_all_matches_to_tool_defs() {
        let mut registry = ToolRegistry::new();
        registry.register(make_tool("a", "tool a"));
        registry.register(make_tool("b", "tool b"));

        let all = registry.to_tool_defs();
        let filtered = registry.to_tool_defs_filtered(|_| true);

        assert_eq!(all.len(), filtered.len());
        for (a, f) in all.iter().zip(filtered.iter()) {
            assert_eq!(a.name, f.name);
        }
    }

    #[test]
    fn filtered_reject_all_returns_empty() {
        let mut registry = ToolRegistry::new();
        registry.register(make_tool("a", "tool a"));

        let defs = registry.to_tool_defs_filtered(|_| false);
        assert!(defs.is_empty());
    }

    #[test]
    fn filtered_empty_registry_returns_empty() {
        let registry = ToolRegistry::new();
        let defs = registry.to_tool_defs_filtered(|_| true);
        assert!(defs.is_empty());
    }

    // --- retain_named (per-node tool whitelist) tests ---

    #[test]
    fn retain_named_keeps_only_allowed_and_empty_is_noop() {
        let mut registry = ToolRegistry::new();
        registry.register(make_tool("Glob", "find files"));
        registry.register(make_tool("Grep", "search content"));
        registry.register(make_tool("Bash", "run commands"));

        // 空 allowlist = 不限制（默认，零回归）。
        registry.retain_named(&[]);
        assert!(registry.get("Glob").is_some());
        assert!(registry.get("Grep").is_some());
        assert!(registry.get("Bash").is_some());

        // 非空 = 只保留白名单内的（含 MCP 代理等一切已注册工具）。
        registry.retain_named(&["Glob".to_string(), "Grep".to_string()]);
        assert!(registry.get("Glob").is_some());
        assert!(registry.get("Grep").is_some());
        assert!(registry.get("Bash").is_none(), "白名单外的工具必须被移除");
        assert_eq!(registry.tool_names().len(), 2);
    }

    #[test]
    fn retain_named_persists_for_late_registration_and_only_narrows() {
        let mut registry = ToolRegistry::new();
        registry.register(make_tool("existing", "already registered"));

        registry.retain_named(&["existing".to_string(), "allowed_late".to_string()]);
        registry.register(make_tool("allowed_late", "allowed after policy installation"));
        registry.register(make_tool("denied_late", "must not bypass policy"));

        assert!(registry.get("existing").is_some());
        assert!(registry.get("allowed_late").is_some());
        assert!(registry.get("denied_late").is_none());

        // A later, different allowlist cannot widen the original authority.
        registry.retain_named(&["existing".to_string(), "denied_late".to_string()]);
        registry.register(make_tool("denied_late", "still denied"));

        assert_eq!(registry.tool_names(), vec!["existing"]);
    }

    #[test]
    fn clear_removes_every_registered_tool() {
        let mut registry = ToolRegistry::new();
        registry.register(make_tool("Read", "read files"));
        registry.register(make_tool("exec_command", "run commands"));
        registry.clear();
        assert!(registry.tool_names().is_empty());

        registry.register(make_tool("late_tool", "must remain denied"));
        registry.retain_named(&[]);
        registry.register(make_tool("another_late_tool", "empty policy cannot reopen"));
        assert!(registry.tool_names().is_empty());
    }

    // --- deferred flag tests ---

    /// A minimal Tool that overrides is_deferred() to return true
    struct DeferredMockTool {
        tool_name: String,
    }

    #[async_trait]
    impl Tool for DeferredMockTool {
        fn name(&self) -> &str {
            &self.tool_name
        }

        fn description(&self) -> &str {
            "a deferred tool"
        }

        fn input_schema(&self) -> serde_json::Value {
            serde_json::json!({"type": "object", "properties": {"x": {"type": "string"}}})
        }

        fn is_concurrency_safe(&self, _input: &serde_json::Value) -> bool {
            true
        }

        fn is_deferred(&self) -> bool {
            true
        }

        async fn execute(&self, _input: serde_json::Value) -> ToolResult {
            ToolResult::text("ok")
        }

        fn category(&self) -> ToolCategory {
            ToolCategory::Info
        }
    }

    #[test]
    fn to_tool_defs_includes_deferred_flag() {
        let mut registry = ToolRegistry::new();
        registry.register(make_tool("core_tool", "a core tool"));
        let defs = registry.to_tool_defs();
        assert!(!defs[0].deferred, "default tools should not be deferred");
    }

    #[test]
    fn to_tool_defs_deferred_tool_flagged() {
        let mut registry = ToolRegistry::new();
        registry.register(Box::new(DeferredMockTool {
            tool_name: "lazy_tool".to_string(),
        }));
        let defs = registry.to_tool_defs();
        assert!(defs[0].deferred, "deferred tool should have deferred=true");
    }

    #[test]
    fn activated_deferred_tool_emits_full_provider_definition() {
        let mut registry = ToolRegistry::new();
        registry.register(Box::new(DeferredMockTool {
            tool_name: "lazy_tool".to_string(),
        }));

        assert_eq!(
            registry
                .deferred_state()
                .search_and_activate("lazy_tool")
                .len(),
            1
        );
        let defs = registry.to_tool_defs();

        assert!(!defs[0].deferred, "activated tool must no longer be a stub");
        assert_eq!(defs[0].input_schema["properties"]["x"]["type"], "string");
        assert_eq!(
            registry.activated_deferred_tool_identities(),
            vec!["lazy_tool".to_string()]
        );
    }

    #[test]
    fn filtered_definitions_observe_activation_state() {
        let mut registry = ToolRegistry::new();
        registry.register(Box::new(DeferredMockTool {
            tool_name: "lazy_tool".to_string(),
        }));
        let state = registry.deferred_state();
        assert_eq!(state.search_and_activate("lazy_tool").len(), 1);

        let defs = registry.to_tool_defs_filtered(|_| true);

        assert_eq!(defs.len(), 1);
        assert!(!defs[0].deferred);
        assert_eq!(defs[0].input_schema["properties"]["x"]["type"], "string");
    }

    #[test]
    fn persisted_activation_rejects_unknown_or_non_deferred_names() {
        let mut registry = ToolRegistry::new();
        registry.register(make_tool("core_tool", "always visible"));

        let state = registry.deferred_state();
        assert!(state.search_and_activate("missing").is_empty());
        assert!(state.search_and_activate("core_tool").is_empty());
        assert!(registry.activated_deferred_tool_identities().is_empty());
    }

    #[test]
    fn restored_activation_waits_for_late_deferred_registration() {
        let mut registry = ToolRegistry::new();

        registry.restore_deferred_tool_activation("late_dynamic_tool");

        assert!(registry.activated_deferred_tool_identities().is_empty());
        assert_eq!(
            registry.session_deferred_tool_identities(),
            vec!["late_dynamic_tool".to_string()]
        );

        registry.register(Box::new(DeferredMockTool {
            tool_name: "late_dynamic_tool".to_string(),
        }));

        assert_eq!(
            registry.activated_deferred_tool_identities(),
            vec!["late_dynamic_tool".to_string()]
        );
        assert_eq!(
            registry.session_deferred_tool_identities(),
            vec!["late_dynamic_tool".to_string()]
        );
        let definition = registry.to_tool_defs().pop().unwrap();
        assert!(!definition.deferred);
        assert_eq!(definition.input_schema["properties"]["x"]["type"], "string");
    }

    #[test]
    fn restored_activation_is_discarded_if_name_becomes_non_deferred() {
        let mut registry = ToolRegistry::new();
        registry.restore_deferred_tool_activation("changed_tool");

        registry.register(make_tool("changed_tool", "now always visible"));

        assert!(registry.session_deferred_tool_identities().is_empty());
        assert!(!registry.to_tool_defs()[0].deferred);
    }

    #[test]
    fn live_catalog_sees_deferred_tools_registered_after_search_creation() {
        let mut registry = ToolRegistry::new();
        let search_state = registry.deferred_state();
        registry.register(Box::new(DeferredMockTool {
            tool_name: "dynamic_lazy_tool".to_string(),
        }));

        let matches = search_state.search_and_activate("dynamic_lazy");

        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].name, "dynamic_lazy_tool");
        let definition = registry
            .to_tool_defs()
            .into_iter()
            .find(|definition| definition.name == "dynamic_lazy_tool")
            .unwrap();
        assert!(!definition.deferred);
        assert_eq!(definition.input_schema["properties"]["x"]["type"], "string");
    }

    #[test]
    fn retain_named_removes_tools_from_live_deferred_catalog() {
        let mut registry = ToolRegistry::new();
        let search_state = registry.deferred_state();
        registry.register(Box::new(DeferredMockTool {
            tool_name: "keep_lazy".to_string(),
        }));
        registry.register(Box::new(DeferredMockTool {
            tool_name: "drop_lazy".to_string(),
        }));

        registry.retain_named(&["keep_lazy".to_string()]);

        assert!(search_state.search_and_activate("drop_lazy").is_empty());
        assert_eq!(search_state.search_and_activate("keep_lazy").len(), 1);
    }

    #[test]
    fn retain_named_does_not_persist_an_activation_removed_by_policy() {
        let mut registry = ToolRegistry::new();
        registry.register(Box::new(DeferredMockTool {
            tool_name: "drop_lazy".to_string(),
        }));
        assert_eq!(
            registry
                .deferred_state()
                .search_and_activate("drop_lazy")
                .len(),
            1
        );

        registry.retain_named(&["keep_only".to_string()]);

        assert!(registry.session_deferred_tool_identities().is_empty());
    }

    #[test]
    fn clear_removes_tools_from_live_deferred_catalog_and_activation_set() {
        let mut registry = ToolRegistry::new();
        let search_state = registry.deferred_state();
        registry.register(Box::new(DeferredMockTool {
            tool_name: "lazy_tool".to_string(),
        }));
        assert_eq!(search_state.search_and_activate("lazy_tool").len(), 1);

        registry.clear();

        assert!(search_state.search_and_activate("lazy_tool").is_empty());
        assert!(search_state.activated_identities().is_empty());
    }
}
