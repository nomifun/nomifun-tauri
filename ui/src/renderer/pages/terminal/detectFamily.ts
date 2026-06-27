/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

/**
 * Detect the agent CLI family from a shell command string.
 *
 * Tokenizes the command and checks if any token's basename stem matches a known
 * agent family. Covers wrapped invocations like `stepcode claude`, `npx codex`,
 * as well as direct invocations (`claude`, `/usr/local/bin/gemini`).
 *
 * Returns null when no known family is detected (honest: unknown CLI).
 */
export type AgentFamily = 'claude' | 'codex' | 'gemini';

const KNOWN_FAMILIES: AgentFamily[] = ['claude', 'codex', 'gemini'];

/**
 * Extract the basename stem from a path-like token.
 * "/usr/bin/claude" -> "claude", "stepcode" -> "stepcode"
 */
function basenameStem(token: string): string {
  // Get basename (after last / or \)
  const base = token.replace(/^.*[\\/]/, '');
  // Strip common extensions (.exe, .cmd, .bat)
  return base.replace(/\.(exe|cmd|bat)$/i, '').toLowerCase();
}

/**
 * Simple tokenizer: splits on whitespace, respects double/single quotes.
 */
function tokenize(command: string): string[] {
  const tokens: string[] = [];
  let current = '';
  let quote: '"' | "'" | null = null;
  let hasToken = false;
  for (let i = 0; i < command.length; i++) {
    const ch = command[i];
    if (quote === '"' && ch === '\\' && command[i + 1] === '"') {
      current += '"';
      i++;
      continue;
    }
    if (quote) {
      if (ch === quote) quote = null;
      else current += ch;
      continue;
    }
    if (ch === '"' || ch === "'") {
      quote = ch as '"' | "'";
      hasToken = true;
      continue;
    }
    if (/\s/.test(ch)) {
      if (hasToken) {
        tokens.push(current);
        current = '';
        hasToken = false;
      }
      continue;
    }
    current += ch;
    hasToken = true;
  }
  if (hasToken) tokens.push(current);
  return tokens;
}

export function detectFamily(command: string): AgentFamily | null {
  if (!command) return null;
  const tokens = tokenize(command.trim());
  for (const token of tokens) {
    const stem = basenameStem(token);
    if (KNOWN_FAMILIES.includes(stem as AgentFamily)) {
      return stem as AgentFamily;
    }
  }
  return null;
}

/**
 * Agent families terminal AutoWork can actually drive: only those with a
 * lifecycle-hook renderer on the backend (Stop → TurnEnd). Mirrors the Rust
 * `AgentCli::supports_lifecycle_hooks` (`nomifun-terminal/src/enhance.rs`).
 * Gemini is intentionally excluded — it has no launch-time hook injection, so
 * the orchestrator cannot detect turn-end and the backend gate rejects it.
 */
const AUTOWORK_CAPABLE_FAMILIES: AgentFamily[] = ['claude', 'codex'];

/**
 * Whether a terminal launch is eligible for AutoWork. Mirrors the backend gate
 * (`nomifun_terminal::terminal_autowork_capable`): a declared `backend` wins
 * (preset launches), otherwise resolve the family from the command + args (so
 * wrappers like `stepcode claude` / `npx codex` and bare custom commands count),
 * then require a lifecycle-hook-capable family.
 *
 * Keep in lock-step with the backend — the gate there is authoritative; this is
 * only the UI's disable/enable hint.
 */
export function isTerminalAutoworkCapable(command: string, args: string[] = [], backend?: string): boolean {
  const declared = backend ? (basenameStem(backend) as AgentFamily) : null;
  const family =
    declared && KNOWN_FAMILIES.includes(declared) ? declared : detectFamily([command, ...args].join(' '));
  return !!family && AUTOWORK_CAPABLE_FAMILIES.includes(family);
}
