#!/usr/bin/env node

import { execFileSync } from 'node:child_process';
import { existsSync, readFileSync, readdirSync } from 'node:fs';
import { dirname, extname, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const SELF = 'scripts/check-agent-vocabulary.mjs';
const SOURCE_EXTENSIONS = new Set([
  '.css',
  '.html',
  '.js',
  '.json',
  '.md',
  '.mjs',
  '.ps1',
  '.rs',
  '.sh',
  '.sql',
  '.toml',
  '.ts',
  '.tsx',
  '.yaml',
  '.yml',
]);

// Collaboration has one runtime aggregate: AgentExecution. These terms are
// retired as active product, API, configuration, path, and code identities.
const RETIRED_TERM =
  /orchestrat|sub[-_ ]?agent|agent[-_ ]?cluster|\bfleet(?:s|[_-]?[a-z0-9]+)*\b|orch[_-]?(?:run|fleet|workspace)/i;

// These exact implementation and wire identities previously exposed two
// delegation stacks to configuration/model callers. Internal deployment
// classes such as `LocalDelegateTool` remain valid; only the retired public
// split and its duplicate response envelopes are forbidden.
const RETIRED_EXACT_IDENTITY =
  /DelegationExecutionMode|PersistentDelegateTool|AgentExecutionEnvelope|shared[_-]?tasks|taskboard|local[_-]?immediate|persistent[_-]?execution|shared[_-]?kernel|process[-_ ]kernel|(?<![A-Za-z0-9])agentRuntime(?:Section)?(?![A-Za-z0-9])|multi[-_ ]agent team/i;

// The pre-release ID redesign ships one clean baseline. Retired migration
// files are quarantined outside the active migration directory and are not
// part of the vocabulary contract.
const LEGACY_MIGRATION_ALLOWLIST = new Set();

// Exact one-release migration/rejection fences are intentionally narrow. A
// whole file is never exempted, so a new active use in the same file still
// fails this check.
const LEGACY_LINE_ALLOWLIST = new Map([
  [
    'crates/agent/nomi-config/src/config.rs',
    [/\bsubagent_token_budget\b/],
  ],
  [
    'crates/backend/nomifun-companion/src/migrate.rs',
    [/smart_orchestration/],
  ],
  [
    'crates/backend/nomifun-companion/src/profile.rs',
    [/smart_orchestration/],
  ],
  [
    'crates/backend/nomifun-conversation/src/service.rs',
    [/\bagent_cluster_mode\b/, /\borchestrator_(?:legacy_identity|role)\b/, /key\.starts_with\("orchestrator_"\)/],
  ],
  [
    'crates/backend/nomifun-gateway/src/registry/mod.rs',
    [
      /\/\/ vocabulary-guard: retired-name-deny(?:-fixture)?$/,
    ],
  ],
  [
    'crates/backend/nomifun-db/src/database.rs',
    [/['"](?:fleets|fleet_members|orch_(?:workspaces|runs|run_tasks|run_task_deps|assignments))['"]/],
  ],
  [
    'crates/backend/nomifun-app/tests/agent_execution_e2e.rs',
    [/\/api\/orchestrator\/fleets/, /removed Fleet\/Orchestrator surface/],
  ],
  [
    'docs/architecture/agent-execution.zh.md',
    [/`(?:fleets|fleet_members|orch_(?:workspaces|runs|run_tasks|run_task_deps|assignments)(?:\.[^`]*)?)`/],
  ],
]);

const normalizePath = (path) => path.replaceAll('\\', '/');

function workspacePaths() {
  const output = execFileSync(
    'git',
    [
      'ls-files',
      '--cached',
      '--others',
      '--exclude-standard',
      '-z',
      '--',
      'Cargo.toml',
      'Cargo.lock',
      'README.md',
      'README.zh-CN.md',
      'CONTRIBUTING.md',
      'crates',
      'apps',
      'ui/src',
      'scripts',
      'docs/architecture',
      'docs/guides',
      'docs/reference',
      'docs/skills',
      'docs/images',
    ],
    { cwd: ROOT, encoding: 'utf8', maxBuffer: 32 * 1024 * 1024 },
  );
  return [...new Set(output.split('\0').filter(Boolean).map(normalizePath))];
}

function isExcluded(path) {
  return (
    path === SELF ||
    LEGACY_MIGRATION_ALLOWLIST.has(path) ||
    path.includes('/fixtures/') ||
    path.startsWith('docs/superpowers/') ||
    path.includes('/manager/openclaw/') ||
    /\/tests\/[^/]*migration[^/]*\.[^/]+$/i.test(path)
  );
}

function isAllowedLegacyFence(path, line) {
  return (LEGACY_LINE_ALLOWLIST.get(path) ?? []).some((pattern) =>
    pattern.test(line),
  );
}

const violations = [];
for (const path of workspacePaths()) {
  const absolute = resolve(ROOT, path);
  if (!existsSync(absolute) || isExcluded(path)) continue;

  if (RETIRED_TERM.test(path) || RETIRED_EXACT_IDENTITY.test(path)) {
    violations.push(`${path}: retired collaboration identity in active path`);
  }

  if (!SOURCE_EXTENSIONS.has(extname(path)) && path !== 'Cargo.lock') {
    continue;
  }

  const lines = readFileSync(absolute, 'utf8').split(/\r?\n/);
  for (const [index, line] of lines.entries()) {
    if (
      (RETIRED_TERM.test(line) || RETIRED_EXACT_IDENTITY.test(line)) &&
      !isAllowedLegacyFence(path, line)
    ) {
      violations.push(`${path}:${index + 1}: ${line.trim()}`);
    }
  }
}

function invariant(condition, message) {
  if (!condition) violations.push(`architecture invariant: ${message}`);
}

for (const retiredSample of ['/api/fleets', 'fleet_members', 'FleetList']) {
  invariant(
    RETIRED_TERM.test(retiredSample),
    `retired-term scanner must reject embedded form ${retiredSample}`,
  );
}

for (const retiredSample of [
  'TaskboardPanel',
  'nomi_taskboard_update',
  'agent_taskboard_state',
  'foo_shared_kernel_bar',
]) {
  invariant(
    RETIRED_EXACT_IDENTITY.test(retiredSample),
    `retired-identity scanner must reject embedded form ${retiredSample}`,
  );
}
invariant(
  !RETIRED_EXACT_IDENTITY.test('AgentRuntimeRegistry'),
  'retired config-key scanner must not reject the legitimate Agent runtime type family',
);

function sorted(values) {
  return [...values].sort();
}

// Keep the simplified collaboration architecture mechanically small. These
// checks intentionally read the canonical definition sites instead of relying
// on a long Rust test run, so concept/schema drift fails the ordinary fast
// `check` command immediately.
const canonicalMigration = readFileSync(
  resolve(ROOT, 'crates/backend/nomifun-db/migrations/001_v3_baseline.sql'),
  'utf8',
);

const migrationDirectory = resolve(
  ROOT,
  'crates/backend/nomifun-db/migrations',
);
const nonBaselineExecutionCreates = readdirSync(migrationDirectory)
  .filter((name) => name.endsWith('.sql') && name !== '001_v3_baseline.sql')
  .flatMap((name) => {
    const source = readFileSync(resolve(migrationDirectory, name), 'utf8');
    return [...source.matchAll(/CREATE TABLE\s+(?:IF NOT EXISTS\s+)?([a-z_]+)/gi)]
      .map((match) => match[1])
      .filter(
        (table) =>
          table.startsWith('agent_execution') ||
          table === 'conversation_execution_links',
      )
      .map((table) => `${name}:${table}`);
  });
invariant(
  nonBaselineExecutionCreates.length === 0,
  `the clean migration directory must contain only the ID-contract baseline; found ${nonBaselineExecutionCreates.join(', ')}`,
);
const executionTables = sorted(
  [...canonicalMigration.matchAll(/CREATE TABLE\s+(?:IF NOT EXISTS\s+)?([a-z_]+)/gi)]
    .map((match) => match[1])
    .filter(
      (name) =>
        name.startsWith('agent_execution') ||
        name === 'conversation_execution_links',
    ),
);
const expectedExecutionTables = sorted([
  'agent_execution_templates',
  'agent_execution_template_participants',
  'agent_executions',
  'agent_execution_participants',
  'agent_execution_steps',
  'agent_execution_step_dependencies',
  'agent_execution_attempts',
  'conversation_execution_links',
  'agent_execution_events',
]);
invariant(
  JSON.stringify(executionTables) === JSON.stringify(expectedExecutionTables),
  `the clean baseline must contain exactly the 7 runtime + 2 template AgentExecution tables; found ${executionTables.join(', ')}`,
);

const gatewayExecution = readFileSync(
  resolve(ROOT, 'crates/backend/nomifun-gateway/src/caps_agent_execution.rs'),
  'utf8',
);
const executionTools = sorted(
  [...gatewayExecution.matchAll(/CapabilityMeta::new\(\s*"(nomi_[a-z0-9_]+)"/g)].map(
    (match) => match[1],
  ),
);
const expectedExecutionTools = sorted([
  'nomi_delegate',
  'nomi_execution_get',
  'nomi_execution_update',
]);
invariant(
  JSON.stringify(executionTools) === JSON.stringify(expectedExecutionTools),
  `model execution surface must remain exactly three tools; found ${executionTools.join(', ')}`,
);

const executionDomain = readFileSync(
  resolve(ROOT, 'crates/backend/nomifun-common/src/agent_execution.rs'),
  'utf8',
);
const eventBlock = executionDomain.match(
  /pub enum AgentExecutionEventKind\s*\{([\s\S]*?)\n\s*\}/,
)?.[1];
const eventFacts = sorted(
  [...(eventBlock ?? '').matchAll(/=>\s*"([a-z_]+)"/g)].map(
    (match) => match[1],
  ),
);
const eventSqlCheck = canonicalMigration.match(
  /event_type\s+TEXT NOT NULL CHECK \(event_type IN \(([\s\S]*?)\)\),/,
)?.[1];
const sqlEventFacts = sorted(
  [...(eventSqlCheck ?? '').matchAll(/'([a-z_]+)'/g)].map(
    (match) => match[1],
  ),
);
const generatedEventBinding = readFileSync(
  resolve(ROOT, 'ui/src/common/protocolBindings/AgentExecutionEventKind.ts'),
  'utf8',
);
const generatedEventAlias = generatedEventBinding.match(
  /export type AgentExecutionEventKind\s*=([\s\S]*?);/,
)?.[1];
const generatedEventFacts = sorted(
  [...(generatedEventAlias ?? '').matchAll(/"([a-z_]+)"/g)].map(
    (match) => match[1],
  ),
);
invariant(
  eventFacts.length === 8,
  `durable execution vocabulary must remain exactly eight facts; found ${eventFacts.join(', ')}`,
);
invariant(
  JSON.stringify(sqlEventFacts) === JSON.stringify(eventFacts),
  `AgentExecution SQL CHECK must match the canonical Rust enum; found ${sqlEventFacts.join(', ')}`,
);
invariant(
  JSON.stringify(generatedEventFacts) === JSON.stringify(eventFacts),
  `generated AgentExecution TypeScript binding must match the canonical Rust enum; found ${generatedEventFacts.join(', ')}`,
);

for (const [name, value] of [
  ['MAX_AGENT_EXECUTION_MODELS', '16'],
  ['MAX_AGENT_EXECUTION_PARTICIPANTS', '64'],
  ['MAX_AGENT_EXECUTION_STEPS', '128'],
  ['MAX_AGENT_EXECUTION_PARALLELISM', '64'],
  ['MAX_AGENT_DELEGATION_DEPTH', '4'],
]) {
  invariant(
    new RegExp(`pub const ${name}: [^=]+ = ${value};`).test(executionDomain),
    `${name} must remain the shared hard limit ${value}`,
  );
}

const executionFacade = readFileSync(
  resolve(ROOT, 'crates/backend/nomifun-agent-execution/src/lib.rs'),
  'utf8',
);
invariant(
  !/pub use .*\b(?:Planner|Router|Scheduler|AttemptRunner)\b/.test(executionFacade),
  'Planner, Router, Scheduler, and AttemptRunner must stay private behind AgentExecutionEngine',
);

const agentConfig = readFileSync(
  resolve(ROOT, 'crates/agent/nomi-config/src/config.rs'),
  'utf8',
);
const toolsConfigBlock = agentConfig.match(
  /pub struct ToolsConfig\s*\{([\s\S]*?)\n\}/,
)?.[1];
invariant(
  toolsConfigBlock !== undefined,
  'ToolsConfig must remain discoverable by the vocabulary guard',
);
invariant(
  !/\b(?:delegation_execution|in_process_delegation|in_process_spawn|install_embedded_agent_execution)\b/.test(
    toolsConfigBlock ?? '',
  ),
  'user configuration must not select embedded versus platform Agent Execution deployment',
);

if (violations.length > 0) {
  console.error(
    `Agent vocabulary check failed: ${violations.length} retired active reference(s)`,
  );
  for (const violation of violations) console.error(`  - ${violation}`);
  process.exit(1);
}

console.log('Agent vocabulary check passed');
