#!/usr/bin/env node

import { execFileSync } from 'node:child_process';
import { readFileSync } from 'node:fs';
import { dirname, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const PLATFORM_PREFIX = 'crates/shared/nomi-execution/src/platform/';
const UNIX_PTY_PATH =
  'crates/shared/nomi-execution/src/platform/unix_pty.rs';
const TOOLS_MANIFEST = 'crates/agent/nomi-tools/Cargo.toml';
const LEGACY_TOOL_FILES = new Set([
  'crates/agent/nomi-tools/src/bash.rs',
  'crates/agent/nomi-tools/src/exec_command.rs',
  'crates/agent/nomi-tools/src/write_stdin.rs',
]);
const LEGACY_TEST_ONLY_FILES = [
  'crates/agent/nomi-tools/src/pty.rs',
  'crates/agent/nomi-tools/src/persistent_shell.rs',
];
// Existing CLI and user-terminal runtimes are outside the Wave A Agent
// command paths. Pin the exact reviewed primitive counts until their own
// migration wave so any added or changed ownership path fails closed.
const REVIEWED_EXTERNAL_OWNERSHIP = new Map([
  [
    'crates/backend/nomifun-ai-agent/src/capability/cli_process/stderr_monitor.rs',
    new Map([
      ['unix-group-owner', 1],
      ['windows-tree-kill-owner', 1],
    ]),
  ],
  [
    'crates/backend/nomifun-terminal/src/pty.rs',
    new Map([
      ['pty-owner', 1],
      ['unix-group-owner', 1],
    ]),
  ],
]);
const HAND_OFF_ALLOWLIST = new Set([
  'crates/agent/nomi-computer/src/launch.rs',
  'crates/backend/nomifun-runtime/src/spawn.rs',
  'crates/backend/nomifun-shell/src/opener.rs',
  'crates/shared/nomi-execution/src/command_builder.rs',
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
      '*.rs',
      TOOLS_MANIFEST,
      'crates/shared/nomi-execution/Cargo.toml',
    ],
    { cwd: ROOT, encoding: 'utf8', maxBuffer: 32 * 1024 * 1024 },
  );
  return [...new Set(output.split('\0').filter(Boolean).map(normalizePath))];
}

function isIdent(byte) {
  return (
    (byte >= 48 && byte <= 57) ||
    (byte >= 65 && byte <= 90) ||
    (byte >= 97 && byte <= 122) ||
    byte === 95
  );
}

function replaceNonNewline(source, start, end) {
  return (
    source.slice(0, start) +
    source
      .slice(start, end)
      .replace(/[^\r\n]/g, ' ') +
    source.slice(end)
  );
}

function rawStringEnd(source, index) {
  let cursor = index;
  if (source[cursor] === 'b') cursor += 1;
  if (source[cursor] !== 'r') return null;
  cursor += 1;
  let hashes = 0;
  while (source[cursor] === '#') {
    hashes += 1;
    cursor += 1;
  }
  if (source[cursor] !== '"') return null;
  const terminator = `"${'#'.repeat(hashes)}`;
  const end = source.indexOf(terminator, cursor + 1);
  return end === -1 ? source.length : end + terminator.length;
}

function quotedEnd(source, index, quote) {
  let cursor = index + 1;
  while (cursor < source.length) {
    if (source[cursor] === '\\') {
      cursor += 2;
    } else if (source[cursor] === quote) {
      return cursor + 1;
    } else {
      cursor += 1;
    }
  }
  return source.length;
}

function charLiteralEnd(source, index) {
  const end = quotedEnd(source, index, "'");
  if (end >= source.length || source[end - 1] !== "'") return null;
  const body = source.slice(index + 1, end - 1);
  if (
    body.length === 1 ||
    /^\\(?:[nrt0'"\\]|x[0-9A-Fa-f]{2}|u\{[0-9A-Fa-f_]+\})$/.test(body)
  ) {
    return end;
  }
  return null;
}

function lexicalMask(source) {
  let output = source;
  let index = 0;
  while (index < source.length) {
    if (source.startsWith('//', index)) {
      const end = source.indexOf('\n', index + 2);
      const stop = end === -1 ? source.length : end;
      output = replaceNonNewline(output, index, stop);
      index = stop;
      continue;
    }
    if (source.startsWith('/*', index)) {
      let cursor = index + 2;
      let depth = 1;
      while (cursor < source.length && depth > 0) {
        if (source.startsWith('/*', cursor)) {
          depth += 1;
          cursor += 2;
        } else if (source.startsWith('*/', cursor)) {
          depth -= 1;
          cursor += 2;
        } else {
          cursor += 1;
        }
      }
      output = replaceNonNewline(output, index, cursor);
      index = cursor;
      continue;
    }
    const rawEnd = rawStringEnd(source, index);
    if (rawEnd !== null) {
      output = replaceNonNewline(output, index, rawEnd);
      index = rawEnd;
      continue;
    }
    if (source[index] === '"' || source.startsWith('b"', index)) {
      const quote = source[index] === '"' ? index : index + 1;
      const end = quotedEnd(source, quote, '"');
      output = replaceNonNewline(output, index, end);
      index = end;
      continue;
    }
    if (
      source[index] === "'" &&
      (index === 0 || !isIdent(source.charCodeAt(index - 1)))
    ) {
      const end = charLiteralEnd(source, index);
      if (end !== null) {
        output = replaceNonNewline(output, index, end);
        index = end;
        continue;
      }
    }
    index += 1;
  }
  return output;
}

function skipSpace(source, index) {
  while (index < source.length && /\s/.test(source[index])) index += 1;
  return index;
}

function attributeEnd(source, index) {
  if (source[index] !== '#' || source[index + 1] !== '[') return null;
  let depth = 1;
  for (let cursor = index + 2; cursor < source.length; cursor += 1) {
    if (source[cursor] === '[') depth += 1;
    if (source[cursor] === ']') {
      depth -= 1;
      if (depth === 0) return cursor + 1;
    }
  }
  return source.length;
}

function matchingBrace(source, open) {
  let depth = 1;
  for (let cursor = open + 1; cursor < source.length; cursor += 1) {
    if (source[cursor] === '{') depth += 1;
    if (source[cursor] === '}') {
      depth -= 1;
      if (depth === 0) return cursor + 1;
    }
  }
  return source.length;
}

function isTopLevelComparison(source, index) {
  const previous = source[index - 1] ?? '';
  const next = source[index + 1] ?? '';
  return (
    source[index] === '<' &&
    !/[A-Za-z0-9_:'">=]/.test(previous) &&
    next !== '<' &&
    next !== '='
  );
}

function attributedItemEnd(source, index) {
  let cursor = skipSpace(source, index);
  while (source[cursor] === '#') {
    const end = attributeEnd(source, cursor);
    if (end === null) break;
    cursor = skipSpace(source, end);
  }
  let paren = 0;
  let bracket = 0;
  for (; cursor < source.length; cursor += 1) {
    const char = source[cursor];
    if (char === '(') paren += 1;
    if (char === ')') paren = Math.max(0, paren - 1);
    if (char === '[') bracket += 1;
    if (char === ']') bracket = Math.max(0, bracket - 1);
    if (paren === 0 && bracket === 0) {
      if (char === ';') return cursor + 1;
      if (char === '{') return matchingBrace(source, cursor);
      if (isTopLevelComparison(source, cursor)) {
        // A comparison in a const/static initializer cannot begin an item body.
        continue;
      }
    }
  }
  return source.length;
}

function splitTopLevelArguments(source) {
  const arguments_ = [];
  let start = 0;
  let depth = 0;
  for (let index = 0; index < source.length; index += 1) {
    if (source[index] === '(') depth += 1;
    if (source[index] === ')') depth = Math.max(0, depth - 1);
    if (source[index] === ',' && depth === 0) {
      arguments_.push(source.slice(start, index));
      start = index + 1;
    }
  }
  arguments_.push(source.slice(start));
  return arguments_;
}

function isTestOnlyCfgAttribute(attribute) {
  const compact = attribute.replace(/\s/g, '');
  if (compact === '#[cfg(test)]') return true;
  const prefix = '#[cfg(all(';
  const suffix = '))]';
  if (!compact.startsWith(prefix) || !compact.endsWith(suffix)) return false;
  const arguments_ = compact.slice(prefix.length, -suffix.length);
  return splitTopLevelArguments(arguments_).includes('test');
}

function testOnlyRanges(source) {
  const masked = lexicalMask(source);
  const ranges = [];
  let index = 0;
  while (index < masked.length) {
    if (masked[index] !== '#' || masked[index + 1] !== '[') {
      index += 1;
      continue;
    }
    const end = attributeEnd(masked, index);
    if (end === null) break;
    if (isTestOnlyCfgAttribute(masked.slice(index, end))) {
      const itemEnd = attributedItemEnd(masked, end);
      ranges.push([index, itemEnd]);
      index = itemEnd;
    } else {
      index = end;
    }
  }
  return { masked, ranges };
}

function productionMask(source) {
  const { masked, ranges } = testOnlyRanges(source);
  let output = masked;
  for (const [start, end] of ranges) {
    output = replaceNonNewline(output, start, end);
  }
  return output;
}

function productionText(source) {
  const { ranges } = testOnlyRanges(source);
  let output = source;
  for (const [start, end] of ranges) {
    output = replaceNonNewline(output, start, end);
  }
  return output;
}

function lineNumber(source, index) {
  return source.slice(0, index).split('\n').length;
}

function snippetAt(source, index) {
  const start = source.lastIndexOf('\n', index - 1) + 1;
  const next = source.indexOf('\n', index);
  return source.slice(start, next === -1 ? source.length : next).trim();
}

function findMatches(source, pattern) {
  pattern.lastIndex = 0;
  return [...source.matchAll(pattern)].map((match) => ({
    index: match.index ?? 0,
    text: match[0],
  }));
}

function manifestSections(source) {
  const sections = [];
  let current = '';
  for (const [lineIndex, line] of source.split(/\r?\n/).entries()) {
    const header = line.match(/^\s*\[([^\]]+)\]\s*(?:#.*)?$/);
    if (header) {
      current = header[1].trim();
      continue;
    }
    sections.push({ section: current, line, lineIndex });
  }
  return sections;
}

function stringLiterals(source) {
  const literals = [];
  let index = 0;
  while (index < source.length) {
    if (source.startsWith('//', index)) {
      index = source.indexOf('\n', index + 2);
      if (index === -1) break;
      continue;
    }
    if (source.startsWith('/*', index)) {
      let cursor = index + 2;
      let depth = 1;
      while (cursor < source.length && depth > 0) {
        if (source.startsWith('/*', cursor)) {
          depth += 1;
          cursor += 2;
        } else if (source.startsWith('*/', cursor)) {
          depth -= 1;
          cursor += 2;
        } else {
          cursor += 1;
        }
      }
      index = cursor;
      continue;
    }
    const rawEnd = rawStringEnd(source, index);
    if (rawEnd !== null) {
      const openingQuote = source.indexOf('"', index);
      const hashes = source.slice(index, openingQuote).split('#').length - 1;
      literals.push({
        index,
        value: source.slice(openingQuote + 1, rawEnd - 1 - hashes),
      });
      index = rawEnd;
      continue;
    }
    if (source[index] === '"' || source.startsWith('b"', index)) {
      const quote = source[index] === '"' ? index : index + 1;
      const end = quotedEnd(source, quote, '"');
      literals.push({
        index,
        value: source.slice(quote + 1, Math.max(quote + 1, end - 1)),
      });
      index = end;
      continue;
    }
    index += 1;
  }
  return literals;
}

function hasTaskkillTreeCall(source) {
  const literals = stringLiterals(source);
  for (const [index, literal] of literals.entries()) {
    if (!/^taskkill(?:\.exe)?$/i.test(literal.value.trim())) continue;
    if (
      literals
        .slice(index + 1)
        .some(
          (candidate) =>
            candidate.index - literal.index <= 600 &&
            /^\/T$/i.test(candidate.value.trim()),
        )
    ) {
      return literal.index;
    }
  }
  return null;
}

function importedLibcNames(source, original) {
  const names = new Set([original]);
  const escaped = original.replace(/[.*+?^${}()|[\]\\]/g, '\\$&');
  for (const match of source.matchAll(
    new RegExp(
      String.raw`\buse\s+libc\s*::\s*${escaped}(?:\s+as\s+([A-Za-z_][A-Za-z0-9_]*))?\s*;`,
      'g',
    ),
  )) {
    names.add(match[1] ?? original);
  }
  for (const match of source.matchAll(
    /\buse\s+libc\s*::\s*\{([^}]*)\}\s*;/gs,
  )) {
    for (const item of match[1].split(',')) {
      const alias = item
        .trim()
        .match(
          new RegExp(
            String.raw`^${escaped}(?:\s+as\s+([A-Za-z_][A-Za-z0-9_]*))?$`,
          ),
        );
      if (alias) names.add(alias[1] ?? original);
    }
  }
  if (/\buse\s+libc\s*::\s*\*\s*;/.test(source)) names.add(original);
  return names;
}

function callPattern(names, prefix, suffix) {
  const alternatives = [...names]
    .map((name) => name.replace(/[.*+?^${}()|[\]\\]/g, '\\$&'))
    .join('|');
  return new RegExp(`${prefix}(?:${alternatives})${suffix}`, 'g');
}

function isDevDependencySection(section) {
  return section === 'dev-dependencies' || section.endsWith('.dev-dependencies');
}

function scanEntries(entries) {
  const normalizedEntries = entries.map(({ path, source }) => ({
    path: normalizePath(path),
    source,
  }));
  const violations = [];
  const byPath = new Map(
    normalizedEntries.map((entry) => [entry.path, entry.source]),
  );
  const report = (path, original, scanned, index, rule, detail) => {
    violations.push({
      path,
      line: lineNumber(scanned, index),
      rule,
      detail,
      snippet: snippetAt(original, index),
    });
  };

  for (const { path, source } of normalizedEntries) {
    if (!path.endsWith('.rs')) continue;
    if (
      LEGACY_TEST_ONLY_FILES.includes(path) ||
      path.split('/').includes('tests')
    ) {
      continue;
    }
    const production = productionMask(source);

    const handOffPatterns = [
      /\.(?:hand_off|with_detached|that_detached)\s*\(/g,
      /\bopen\s*::\s*(?:that_detached|with_detached)\s*\(/g,
    ];
    for (const pattern of handOffPatterns) {
      for (const match of findMatches(production, pattern)) {
        if (!HAND_OFF_ALLOWLIST.has(path)) {
          report(
            path,
            source,
            production,
            match.index,
            'hand-off-allowlist',
            'explicit hand-off is permitted only in exact allowlisted files',
          );
        }
      }
    }

    const ownershipViolations = [];
    const collectOwnership = (index, rule, detail) => {
      ownershipViolations.push({ index, rule, detail });
    };
    for (const match of findMatches(production, /\bnative_pty_system\s*\(/g)) {
      if (path !== UNIX_PTY_PATH) {
        collectOwnership(
          match.index,
          'pty-owner',
          `native_pty_system is permitted only in ${UNIX_PTY_PATH}`,
        );
      }
    }

    const kqueueNames = importedLibcNames(production, 'kqueue');
    const killNames = importedLibcNames(production, 'kill');
    const ownershipPatterns = [
      ['windows-job-owner', /\bJOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE\b/g],
      ['linux-parent-death-owner', /\bPR_SET_PDEATHSIG\b/g],
      [
        'macos-parent-death-owner',
        callPattern(
          kqueueNames,
          String.raw`(?:\blibc\s*::\s*|\b)`,
          String.raw`\s*\(`,
        ),
      ],
      [
        'unix-group-owner',
        callPattern(
          killNames,
          String.raw`(?:\blibc\s*::\s*|\b)`,
          String.raw`\s*\(\s*-`,
        ),
      ],
    ];
    if (!path.startsWith(PLATFORM_PREFIX)) {
      for (const [rule, pattern] of ownershipPatterns) {
        for (const match of findMatches(production, pattern)) {
          collectOwnership(
            match.index,
            rule,
            'process ownership primitive must live under nomi-execution/src/platform',
          );
        }
      }
      const taskkillIndex = hasTaskkillTreeCall(productionText(source));
      if (taskkillIndex !== null) {
        collectOwnership(
          taskkillIndex,
          'windows-tree-kill-owner',
          'process ownership primitive must live under nomi-execution/src/platform',
        );
      }
    }
    const reviewed = REVIEWED_EXTERNAL_OWNERSHIP.get(path);
    if (reviewed) {
      const observed = new Map();
      for (const violation of ownershipViolations) {
        observed.set(
          violation.rule,
          (observed.get(violation.rule) ?? 0) + 1,
        );
      }
      const exact =
        observed.size === reviewed.size &&
        [...reviewed].every(
          ([rule, expected]) => observed.get(rule) === expected,
        );
      if (exact) ownershipViolations.length = 0;
    }
    for (const violation of ownershipViolations) {
      report(
        path,
        source,
        production,
        violation.index,
        violation.rule,
        violation.detail,
      );
    }

    const inToolSurface =
      LEGACY_TOOL_FILES.has(path) ||
      path === 'crates/agent/nomi-tools/src/process_store.rs' ||
      path === 'crates/agent/nomi-tools/src/lib.rs' ||
      path === 'crates/agent/nomi-agent/src/bootstrap.rs';
    if (!inToolSurface) continue;

    if (LEGACY_TOOL_FILES.has(path)) {
      const forbiddenToolPatterns = [
        ['tool-direct-command', /\btokio\s*::\s*process\s*::\s*Command\b/g],
        ['tool-output-future', /\.output\s*\(/g],
        [
          'tool-timeout-output',
          /\b(?:tokio\s*::\s*time\s*::\s*)?timeout\s*\([\s\S]{0,500}?\.output\s*\(/g,
        ],
        [
          'tool-old-pty',
          /\b(?:PtyParams|Pty\s*::\s*spawn|MasterPty|ChildKiller|ExecSession|collect_until_deadline)\b/g,
        ],
        ['tool-old-pty-module', /\bcrate\s*::\s*pty\b/g],
      ];
      for (const [rule, pattern] of forbiddenToolPatterns) {
        for (const match of findMatches(production, pattern)) {
          report(
            path,
            source,
            production,
            match.index,
            rule,
            'legacy command adapters must delegate OS execution to ProcessSupervisor',
          );
        }
      }
      if (!/\bProcessSupervisor\b/.test(production)) {
        report(
          path,
          source,
          production,
          0,
          'tool-supervisor-required',
          'legacy command adapter must reference ProcessSupervisor',
        );
      }
    }
  }

  const toolsLibPath = 'crates/agent/nomi-tools/src/lib.rs';
  const toolsLib = byPath.get(toolsLibPath) ?? '';
  for (const module of ['pty', 'persistent_shell']) {
    const gate = new RegExp(
      String.raw`#\s*\[\s*cfg\s*\(\s*test\s*\)\s*\]\s*pub\s+mod\s+${module}\s*;`,
      'm',
    );
    if (!gate.test(lexicalMask(toolsLib))) {
      report(
        toolsLibPath,
        toolsLib,
        toolsLib,
        Math.max(0, toolsLib.indexOf(`pub mod ${module};`)),
        'legacy-test-only-gate',
        `${module}.rs must be compiled only under cfg(test)`,
      );
    }
  }
  for (const path of LEGACY_TEST_ONLY_FILES) {
    if (!byPath.has(path)) {
      violations.push({
        path,
        line: 1,
        rule: 'legacy-test-only-source',
        detail: 'expected test-only compatibility source is missing',
        snippet: '',
      });
    }
  }

  const storePath = 'crates/agent/nomi-tools/src/process_store.rs';
  const storeSource = byPath.get(storePath) ?? '';
  const store = productionMask(storeSource);
  for (const required of [
    'ExecutionOwner',
    'SessionId',
    'OutputCursor',
    'Transport',
  ]) {
    if (!store.includes(required)) {
      report(
        storePath,
        storeSource,
        store,
        0,
        'numeric-adapter-shape',
        `ProcessStore must retain ${required} metadata`,
      );
    }
  }
  for (const match of findMatches(
    store,
    /\b(?:PtyParams|Pty\s*::\s*spawn|MasterPty|ChildKiller|ExecSession|std\s*::\s*process\s*::\s*Child|tokio\s*::\s*process\s*::\s*Child)\b/g,
  )) {
    report(
      storePath,
      storeSource,
      store,
      match.index,
      'numeric-adapter-process-free',
      'ProcessStore must not own a PTY or OS process',
    );
  }

  const toolsManifest = byPath.get(TOOLS_MANIFEST) ?? '';
  let portablePtyDev = false;
  for (const { section, line, lineIndex } of manifestSections(toolsManifest)) {
    if (!/^\s*portable-pty(?:\.workspace)?\s*=/.test(line)) continue;
    if (isDevDependencySection(section)) {
      portablePtyDev = true;
    } else {
      const index = toolsManifest
        .split(/\r?\n/)
        .slice(0, lineIndex)
        .reduce((sum, item) => sum + item.length + 1, 0);
      report(
        TOOLS_MANIFEST,
        toolsManifest,
        toolsManifest,
        index,
        'portable-pty-production-dependency',
        'nomi-tools may depend on portable-pty only through dev-dependencies',
      );
    }
  }
  if (!portablePtyDev) {
    report(
      TOOLS_MANIFEST,
      toolsManifest,
      toolsManifest,
      0,
      'portable-pty-test-dependency',
      'test-only legacy PTY modules require portable-pty under dev-dependencies',
    );
  }

  const executionManifestPath =
    'crates/shared/nomi-execution/Cargo.toml';
  const executionManifest = byPath.get(executionManifestPath) ?? '';
  for (const pattern of [
    /\bnomifun-[\w-]*\b/g,
    /\bnomi-(?:types|agent|tools)\b/g,
    /\b(?:rusqlite|sqlx|tauri)\b/g,
  ]) {
    for (const match of findMatches(executionManifest, pattern)) {
      report(
        executionManifestPath,
        executionManifest,
        executionManifest,
        match.index,
        'execution-dependency-boundary',
        'nomi-execution must remain backend-neutral',
      );
    }
  }

  return violations;
}

function workspaceEntries() {
  return workspacePaths().map((path) => ({
    path,
    source: readFileSync(resolve(ROOT, path), 'utf8'),
  }));
}

function assertNoViolation(entries, message) {
  const violations = scanEntries(entries);
  if (violations.length > 0) {
    throw new Error(
      `${message}: ${violations.map((item) => item.rule).join(', ')}`,
    );
  }
}

function assertViolation(entries, rule, message) {
  if (!scanEntries(entries).some((violation) => violation.rule === rule)) {
    throw new Error(message);
  }
}

function selfTest() {
  const base = [
    {
      path: 'crates/agent/nomi-tools/src/lib.rs',
      source:
        '#[cfg(test)]\npub mod persistent_shell;\n#[cfg(test)]\npub mod pty;\n',
    },
    ...LEGACY_TOOL_FILES.values().map((path) => ({
      path,
      source: 'use nomi_execution::ProcessSupervisor;\n',
    })),
    {
      path: 'crates/agent/nomi-tools/src/process_store.rs',
      source:
        'use nomi_execution::{ExecutionOwner, SessionId, OutputCursor, Transport};\n',
    },
    ...LEGACY_TEST_ONLY_FILES.map((path) => ({
      path,
      source: 'fn compatibility_test_helper() {}\n',
    })),
    {
      path: TOOLS_MANIFEST,
      source: '[dependencies]\ntokio = "1"\n[dev-dependencies]\nportable-pty = "0.8"\n',
    },
    {
      path: 'crates/shared/nomi-execution/Cargo.toml',
      source: '[dependencies]\ntokio = "1"\n',
    },
  ];
  assertNoViolation(base, 'baseline unexpectedly violates the boundary');

  assertViolation(
    base.concat({
      path: 'crates/new/untracked_adapter.rs',
      source: 'fn launch(command: &mut C) { command.hand_off(); }\n',
    }),
    'hand-off-allowlist',
    'failed to reject hand_off in an arbitrary new path',
  );
  assertViolation(
    base.concat({
      path: 'crates/new/untracked_open.rs',
      source: 'fn launch() { open::that_detached("target"); }\n',
    }),
    'hand-off-allowlist',
    'failed to reject open::that_detached in an arbitrary new path',
  );
  assertNoViolation(
    base.concat({
      path: 'crates/backend/nomifun-shell/src/opener.rs',
      source:
        'fn launch() { open::that_detached("target"); command.hand_off(); }\n',
    }),
    'exact hand-off allowlist rejected an approved path',
  );
  assertNoViolation(
    base.concat({
      path: UNIX_PTY_PATH.replaceAll('/', '\\'),
      source: 'fn open() { native_pty_system(); }\n',
    }),
    'path normalization or unix_pty allowance failed',
  );
  assertViolation(
    base.concat({
      path: 'crates/shared/nomi-execution/src/platform/unix.rs',
      source: 'fn open() { native_pty_system(); }\n',
    }),
    'pty-owner',
    'failed to enforce the exact native_pty_system owner',
  );
  assertViolation(
    base.concat({
      path: 'crates/backend/nomifun-runtime/src/late.rs',
      source:
        '#[cfg(test)]\nmod tests { fn fake() { libc::kill(-1, 9); } }\nfn production() { libc::kill(-2, 9); }\n',
    }),
    'unix-group-owner',
    'production after a cfg(test) item was not scanned',
  );
  assertViolation(
    base.concat({
      path: 'crates/backend/nomifun-runtime/src/unicode.rs',
      source:
        '// 非 ASCII 注释不得改变后续扫描偏移\nfn production() { libc::kill(-2, 9); }\n',
    }),
    'unix-group-owner',
    'Unicode before production code corrupted scanner offsets',
  );
  assertNoViolation(
    base.concat({
      path: 'crates/backend/nomifun-runtime/src/comments.rs',
      source:
        '// command.hand_off(); libc::kill(-1, 9);\nconst TEXT: &str = "open::that_detached native_pty_system";\n#[cfg(test)]\nmod tests { fn fake() { libc::kill(-2, 9); command.hand_off(); } }\n',
    }),
    'comments, strings, or test modules produced a false positive',
  );
  assertViolation(
    base.concat({
      path: 'crates/backend/nomifun-runtime/src/cfg_attr.rs',
      source:
        '#[cfg_attr(test, allow(dead_code))]\nfn production() { libc::kill(-2, 9); }\n',
    }),
    'unix-group-owner',
    'cfg_attr(test, ...) incorrectly hid production code',
  );
  assertNoViolation(
    base.concat({
      path: 'crates/backend/nomifun-runtime/src/cfg_all_test.rs',
      source:
        '#[cfg(all(test, target_os = "linux"))]\nmod tests { fn fake() { libc::kill(-2, 9); } }\n',
    }),
    'cfg(all(test, ...)) test code produced a false positive',
  );
  assertNoViolation(
    base.concat({
      path: 'crates/backend/nomifun-runtime/src/cfg_all_test_late.rs',
      source:
        '#[cfg(all(target_os = "linux", test))]\nmod tests { fn fake() { libc::kill(-2, 9); } }\n',
    }),
    'cfg(all(..., test)) test code produced a false positive',
  );
  assertViolation(
    base.concat({
      path: 'crates/backend/nomifun-runtime/src/const_comparison.rs',
      source:
        '#[cfg(test)]\nconst TEST_ONLY: bool = 1 < 2;\nfn production() { libc::kill(-2, 9); }\n',
    }),
    'unix-group-owner',
    'comparison in a test-only const swallowed following production code',
  );
  assertViolation(
    base.concat({
      path: 'crates/backend/other-runtime/src/imported_kill.rs',
      source:
        'use libc::{kill, SIGKILL};\nfn cleanup(pgid: i32) { unsafe { kill(-pgid, SIGKILL); } }\n',
    }),
    'unix-group-owner',
    'failed to reject an imported negative-PID group kill',
  );
  assertViolation(
    base.concat({
      path: 'crates/backend/other-runtime/src/imported_kqueue.rs',
      source:
        'use libc::kqueue as make_queue;\nfn watch() { unsafe { make_queue(); } }\n',
    }),
    'macos-parent-death-owner',
    'failed to reject an imported kqueue watchdog primitive',
  );
  assertViolation(
    base.concat({
      path: 'crates/backend/other-runtime/src/taskkill.rs',
      source:
        'fn cleanup() { std::process::Command::new("taskkill").args(["/PID", "7", "/T"]); }\n',
    }),
    'windows-tree-kill-owner',
    'failed to reject taskkill /T hidden in Rust string literals',
  );
  assertNoViolation(
    base.concat({
      path: 'crates/backend/other-runtime/src/taskkill_test.rs',
      source:
        '#[cfg(test)]\nfn cleanup() { std::process::Command::new("taskkill").arg("/T"); }\n',
    }),
    'taskkill /T inside cfg(test) produced a false positive',
  );
  assertViolation(
    base.concat({
      path: 'crates/backend/other-runtime/src/imported_kill_alias.rs',
      source:
        'use libc::kill as group_kill;\nfn cleanup(pgid: i32) { unsafe { group_kill(-pgid, 9); } }\n',
    }),
    'unix-group-owner',
    'failed to reject an aliased negative-PID group kill',
  );
  assertViolation(
    base.concat({
      path: 'crates/backend/other-runtime/src/imported_kill_glob.rs',
      source:
        'use libc::*;\nfn cleanup(pgid: i32) { unsafe { kill(-pgid, SIGKILL); } }\n',
    }),
    'unix-group-owner',
    'failed to reject a glob-imported negative-PID group kill',
  );
  assertNoViolation(
    base.concat({
      path: 'crates/shared/nomi-execution/tests/ownership_fixture.rs',
      source:
        'fn cleanup(pgid: i32) { unsafe { libc::kill(-pgid, 9); } }\n',
    }),
    'integration-test ownership fixture produced a production false positive',
  );
  assertNoViolation(
    base.concat({
      path:
        'crates/backend/nomifun-ai-agent/src/capability/cli_process/stderr_monitor.rs',
      source:
        'fn cleanup(pgid: i32) { unsafe { libc::kill(-pgid, 9); } std::process::Command::new("taskkill").arg("/T"); }\n',
    }),
    'reviewed external ownership signatures were not accepted',
  );
  assertViolation(
    base.concat({
      path:
        'crates/backend/nomifun-ai-agent/src/capability/cli_process/stderr_monitor.rs',
      source:
        'fn cleanup(pgid: i32) { unsafe { libc::kill(-pgid, 9); libc::kill(-pgid, 9); } std::process::Command::new("taskkill").arg("/T"); }\n',
    }),
    'unix-group-owner',
    'reviewed external ownership count changed without failing closed',
  );
  assertViolation(
    base.concat({
      path:
        'crates/backend/nomifun-ai-agent/src/capability/cli_process/stderr_monitor.rs',
      source:
        'fn cleanup(pgid: i32) { unsafe { libc::kill(-pgid, 9); } std::process::Command::new("taskkill").arg("/T"); command.hand_off(); }\n',
    }),
    'hand-off-allowlist',
    'reviewed ownership exception suppressed another boundary rule',
  );
  assertViolation(
    base.concat({
      path:
        'crates/backend/nomifun-ai-agent/src/capability/cli_process/new_owner.rs',
      source:
        'fn cleanup(pgid: i32) { unsafe { libc::kill(-pgid, 9); } }\n',
    }),
    'unix-group-owner',
    'reviewed external runtime exception widened beyond its exact file',
  );
  assertViolation(
    base.map((entry) =>
      entry.path === TOOLS_MANIFEST
        ? {
            ...entry,
            source:
              '[dependencies]\nportable-pty = "0.8"\n[dev-dependencies]\ntempfile = "3"\n',
          }
        : entry,
    ),
    'portable-pty-production-dependency',
    'failed to reject portable-pty in production dependencies',
  );
  assertViolation(
    base.map((entry) =>
      entry.path === TOOLS_MANIFEST
        ? {
            ...entry,
            source:
              "[target.'cfg(unix)'.dependencies]\nportable-pty = \"0.8\"\n[dev-dependencies]\ntempfile = \"3\"\n",
          }
        : entry,
    ),
    'portable-pty-production-dependency',
    'failed to reject portable-pty in target production dependencies',
  );
}

if (process.argv.includes('--self-test')) {
  selfTest();
  console.log('process runtime boundary scanner self-test passed');
  process.exit(0);
}

const violations = scanEntries(workspaceEntries());
if (violations.length > 0) {
  for (const violation of violations) {
    console.error(
      `${violation.path}:${violation.line} [${violation.rule}] ${violation.detail}`,
    );
    if (violation.snippet) console.error(`  ${violation.snippet}`);
  }
  console.error(
    `process runtime boundary check failed: ${violations.length} violation(s)`,
  );
  process.exit(1);
}

console.log('process runtime boundary check passed');
