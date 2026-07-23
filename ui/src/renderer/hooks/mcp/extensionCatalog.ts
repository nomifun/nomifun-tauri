import { CANONICAL_UUID_V7, type McpServerId } from '@/common/types/ids';

declare const extensionMcpSourceKeyBrand: unique symbol;

export type ExtensionMcpSourceKey = string & {
  readonly [extensionMcpSourceKeyBrand]: 'extension-mcp-source-key';
};

/**
 * Read-only MCP contribution exposed by an extension.
 *
 * `source_key` is intentionally opaque: extension contributions are not
 * repository-backed MCP entities and must never be passed to APIs that require
 * a canonical `McpServerId`.
 */
export interface ExtensionMcpServerContribution {
  readonly source_key: ExtensionMcpSourceKey;
  readonly name: string;
  readonly description?: string;
}

const isRecord = (value: unknown): value is Record<string, unknown> =>
  Boolean(value) && typeof value === 'object' && !Array.isArray(value);

const nonEmptyString = (value: unknown): string | undefined =>
  typeof value === 'string' && value.trim() ? value : undefined;

const hasOwn = (value: Record<string, unknown>, key: string): boolean =>
  Object.prototype.hasOwnProperty.call(value, key);

const extensionSourceKey = (value: unknown): ExtensionMcpSourceKey | undefined => {
  const key = nonEmptyString(value);
  if (!key || key.trim() !== key || key.length > 255) return undefined;

  const [extensionName, localKey, ...extraParts] = key.split(':');
  const validPart = (part: string): boolean => /^[a-z0-9._-]+$/.test(part);
  if (extraParts.length > 0 || !validPart(extensionName) || !validPart(localKey)) return undefined;
  if (CANONICAL_UUID_V7.test(localKey)) return undefined;

  const uuidSuffix = localKey.slice(localKey.lastIndexOf('_') + 1);
  if (uuidSuffix !== localKey && CANONICAL_UUID_V7.test(uuidSuffix)) return undefined;

  return key as ExtensionMcpSourceKey;
};

export const extensionMcpUiKey = (key: ExtensionMcpSourceKey): `extension:${string}` => `extension:${key}`;

/** Namespace repository-backed MCP IDs away from extension contribution keys in shared UI state. */
export const mcpServerUiKey = (id: McpServerId): `server:${string}` => `server:${id}`;

const parseExtensionMcpServer = (value: unknown): ExtensionMcpServerContribution | undefined => {
  if (!isRecord(value)) return undefined;
  if (['id', 'sourceKey', 'contributionKey', 'contribution_key'].some((key) => hasOwn(value, key))) return undefined;

  const source_key = extensionSourceKey(value.source_key);
  const name = nonEmptyString(value.name);
  const description = nonEmptyString(value.description);
  if (!source_key || !name) return undefined;

  return {
    source_key,
    name,
    ...(description ? { description } : {}),
  };
};

/** Parse each contribution independently so one malformed extension cannot hide its siblings. */
export const parseExtensionMcpServers = (values: readonly unknown[]): ExtensionMcpServerContribution[] => {
  const seen = new Set<string>();
  const contributions: ExtensionMcpServerContribution[] = [];

  for (const value of values) {
    const parsed = parseExtensionMcpServer(value);
    if (!parsed || seen.has(parsed.source_key)) continue;

    seen.add(parsed.source_key);
    contributions.push(parsed);
  }

  return contributions;
};
