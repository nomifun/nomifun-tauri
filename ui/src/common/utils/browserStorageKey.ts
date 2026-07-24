/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import type {
  EntityId,
  EntityKind,
  SessionTarget,
} from '@/common/types/ids';
import { parseEntityId } from '@/common/types/ids';

export const BROWSER_STORAGE_SCHEMA_VERSION = 1 as const;

export type BrowserStorageEntityKind = EntityKind;

export type BrowserStorageFeature =
  | 'workspace-collapse'
  | 'workspace-panel-tab'
  | 'workspace-preview'
  | 'draft'
  | 'initial-message-acp'
  | 'initial-message-nanobot'
  | 'initial-message-nomi'
  | 'initial-message-openclaw'
  | 'initial-message-remote'
  | 'initial-message-processed'
  | 'command-queue'
  | 'cron-unread'
  | (string & {});

const KEY_ROOT = 'nomifun';
let storageGeneration: string | null = null;

/**
 * Sets the identity of the currently mounted backend dataset.
 *
 * Call this with `application.systemInfo.storageGeneration` during renderer
 * bootstrap. Keeping the generation in every entity-scoped key prevents
 * browser state surviving a reset or restore from binding to a new graph.
 */
export function setBrowserStorageGeneration(value: string): void {
  try {
    parseEntityId('user', value);
  } catch {
    throw new TypeError('storage generation must be a canonical lowercase UUIDv7 string');
  }
  storageGeneration = value;
}

export function getBrowserStorageGeneration(): string {
  if (!storageGeneration) {
    throw new Error('browser storage generation has not been initialized');
  }
  return storageGeneration;
}

/** A generation-scoped key for UI state that is not owned by one entity. */
export function browserStorageGenerationKey(feature: BrowserStorageFeature): string {
  return [
    KEY_ROOT,
    `v${BROWSER_STORAGE_SCHEMA_VERSION}`,
    encodeSegment(getBrowserStorageGeneration()),
    encodeSegment(feature),
  ].join('|');
}

function encodeSegment(value: string): string {
  return `${value.length}:${value}`;
}

/**
 * Produces an unambiguous, versioned entity-scoped browser storage key.
 *
 * Length-prefixed segments ensure tuples such as (`ab`, `c`) and (`a`, `bc`)
 * can never collide. Entity kind is mandatory, so conversation "1" and
 * terminal "1" occupy distinct namespaces.
 */
export function browserStorageKey<Kind extends EntityKind>(
  feature: BrowserStorageFeature,
  entityKind: Kind,
  entityId: EntityId<Kind>
): string;
export function browserStorageKey(
  feature: BrowserStorageFeature,
  entityKind: BrowserStorageEntityKind,
  entityId: string
): string {
  const generation = getBrowserStorageGeneration();
  return [
    KEY_ROOT,
    `v${BROWSER_STORAGE_SCHEMA_VERSION}`,
    encodeSegment(generation),
    encodeSegment(feature),
    encodeSegment(entityKind),
    encodeSegment(String(entityId)),
  ].join('|');
}

export function sessionStorageKey(feature: BrowserStorageFeature, target: SessionTarget): string {
  return browserStorageKey(feature, target.kind, target.id);
}
