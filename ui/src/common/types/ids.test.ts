/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import { describe, expect, test } from 'bun:test';
import {
  InvalidEntityIdError,
  conversationTarget,
  isSameSessionTarget,
  parseCompanionEvolutionFeedbackId,
  parseConnectorCredentialId,
  parseChannelPluginId,
  parseChannelUserId,
  parseConversationId,
  parseConversationArtifactId,
  parseCreationTaskId,
  parseCronJobId,
  parseCronJobRunId,
  parseFigureId,
  parseIdmmInterventionId,
  parseMcpServerId,
  parseMessageId,
  parseOptionalEntityId,
  parsePresetId,
  parseProviderId,
  parsePublicAgentAuditEntryId,
  parseRemoteAgentId,
  parseRequirementId,
  parseTerminalId,
  parseWebhookId,
  parseWorkshopEdgeId,
  parseWorkshopNodeId,
  terminalTarget,
  tryParseEntityId,
} from './ids';

const UUID_V4 = '550e8400-e29b-41d4-a716-446655440000';

const expectInvalidEntityId = (action: () => unknown): void => {
  let error: unknown;
  try {
    action();
  } catch (caught) {
    error = caught;
  }
  expect(error instanceof InvalidEntityIdError).toBe(true);
};

const invalidBusinessIdValues = (id: string, prefix: string): unknown[] => [
  7,
  '7',
  UUID_V4,
  id.toUpperCase(),
  `${prefix}_${id}`,
  { id },
];

describe('entity ids', () => {
  test('strict parsers accept only canonical bare lowercase UUIDv7 strings', () => {
    const validConversation = '0190f5fe-7c00-7a00-8000-000000000001';
    expect(parseConversationId(validConversation)).toBe(validConversation);
    for (const value of [
      1,
      `conv_${validConversation}`,
      '0190f5fe-7c00-8a00-8000-000000000001',
      '0190f5fe-7c00-7a00-8000-000000000001 ',
      '0190f5fe-7c00-4a00-8000-000000000001',
      '0190f5fe7c007a008000000000000001',
      '',
    ]) {
      let error: unknown;
      try {
        parseConversationId(value);
      } catch (caught) {
        error = caught;
      }
      expect(error instanceof InvalidEntityIdError).toBe(true);
    }
    expect(tryParseEntityId('conversation', null)).toBeNull();
  });

  test('rejects legacy prefixes and non-canonical UUID forms for every business kind', () => {
    const id = '0190f5fe-7c00-7a00-8000-000000000001';
    for (const parse of [
      () => parseConversationId(`conv_${id}`),
      () => parseTerminalId(`term_${id}`),
      () => parseRequirementId(`req_${id}`),
      () => parseProviderId(`prov_${id}`),
      () => parseMessageId(`msg_${id}`),
      () => parseCronJobId(`cron_${id}`),
      () => parseCronJobRunId(`cronrun_${id}`),
      () => parseRemoteAgentId(`ragent_${id}`),
      () => parseTerminalId('0190F5FE-7C00-7A00-8000-000000000001'),
      () => parseTerminalId('{0190f5fe-7c00-7a00-8000-000000000001}'),
    ]) {
      let error: unknown;
      try {
        parse();
      } catch (caught) {
        error = caught;
      }
      expect(error instanceof InvalidEntityIdError).toBe(true);
    }
  });

  test('accepts an absent optional ID but still rejects non-UUIDv7 values when present', () => {
    const conversationId = '0190f5fe-7c00-7a00-8000-000000000001';
    expect(parseOptionalEntityId('conversation', undefined)).toBeUndefined();
    expect(parseOptionalEntityId('conversation', null)).toBeUndefined();
    expect(parseOptionalEntityId('conversation', conversationId)).toBe(conversationId);
    let error: unknown;
    try {
      parseOptionalEntityId('conversation', '550e8400-e29b-41d4-a716-446655440000');
    } catch (caught) {
      error = caught;
    }
    expect(error instanceof InvalidEntityIdError).toBe(true);
  });

  test('session target comparison includes the entity namespace', () => {
    const conversationId = '0190f5fe-7c00-7a00-8000-000000000001';
    const terminalId = '0190f5fe-7c00-7a00-8000-000000000001';
    expect(isSameSessionTarget(conversationTarget(conversationId), conversationTarget(conversationId))).toBe(true);
    expect(isSameSessionTarget(conversationTarget(conversationId), terminalTarget(terminalId))).toBe(false);
  });

  test('validates newly registered durable file and document entity ids', () => {
    expect(parseFigureId('0190f5fe-7c00-7a00-8000-000000000001')).toBe(
      '0190f5fe-7c00-7a00-8000-000000000001',
    );
    expect(parsePublicAgentAuditEntryId('0190f5fe-7c00-7a00-8000-000000000002')).toBe(
      '0190f5fe-7c00-7a00-8000-000000000002',
    );
    expect(parseCompanionEvolutionFeedbackId('0190f5fe-7c00-7a00-8000-000000000003')).toBe(
      '0190f5fe-7c00-7a00-8000-000000000003',
    );
    expect(parseWorkshopNodeId('0190f5fe-7c00-7a00-8000-000000000004')).toBe(
      '0190f5fe-7c00-7a00-8000-000000000004',
    );
    expect(parseWorkshopEdgeId('0190f5fe-7c00-7a00-8000-000000000005')).toBe(
      '0190f5fe-7c00-7a00-8000-000000000005',
    );
  });

  test('channel plugins and users use bare UUIDv7 business IDs', () => {
    const channelId = '0190f5fe-7c00-7a00-8000-000000000007';
    const channelUserId = '0190f5fe-7c00-7a00-8000-000000000008';
    expect(parseChannelPluginId(channelId)).toBe(channelId);
    expect(parseChannelUserId(channelUserId)).toBe(channelUserId);
    for (const value of [
      '7',
      0,
      -1,
    ]) {
      let error: unknown;
      try {
        parseChannelPluginId(value);
      } catch (caught) {
        error = caught;
      }
      expect(error instanceof InvalidEntityIdError).toBe(true);
    }
  });

  test('requirements use bare UUIDv7 business IDs rather than local row ids', () => {
    const requirementId = '0190f5fe-7c00-7a00-8000-000000000009';
    expect(parseRequirementId(requirementId)).toBe(requirementId);
    for (const value of [9, '9', `req_${requirementId}`]) {
      let error: unknown;
      try {
        parseRequirementId(value);
      } catch (caught) {
        error = caught;
      }
      expect(error instanceof InvalidEntityIdError).toBe(true);
    }
  });

  test('webhook_id accepts only a bare canonical lowercase UUIDv7', () => {
    const webhookId = '0190f5fe-7c00-7a00-8000-000000000042';
    expect(parseWebhookId(webhookId)).toBe(webhookId);
    for (const value of invalidBusinessIdValues(webhookId, 'webhook')) {
      expectInvalidEntityId(() => parseWebhookId(value));
    }
  });

  test('credential_id accepts only a bare canonical lowercase UUIDv7', () => {
    const credentialId = '0190f5fe-7c00-7a00-8000-00000000000a';
    expect(parseConnectorCredentialId(credentialId)).toBe(credentialId);

    for (const value of [
      ...invalidBusinessIdValues(credentialId, 'credential'),
      credentialId.replaceAll('-', ''),
      `${credentialId} `,
    ]) {
      expectInvalidEntityId(() => parseConnectorCredentialId(value));
    }
  });

  test('creation_task_id accepts only a bare canonical lowercase UUIDv7', () => {
    const creationTaskId = '0190f5fe-7c00-7a00-8000-000000000012';
    expect(parseCreationTaskId(creationTaskId)).toBe(creationTaskId);

    for (const value of [
      ...invalidBusinessIdValues(creationTaskId, 'task'),
      creationTaskId.replaceAll('-', ''),
      `${creationTaskId} `,
    ]) {
      expectInvalidEntityId(() => parseCreationTaskId(value));
    }
  });

  test('cron_job_id and cron_job_run_id accept only bare canonical lowercase UUIDv7 values', () => {
    const cronJobId = '0190f5fe-7c00-7a00-8000-000000000010';
    const cronJobRunId = '0190f5fe-7c00-7a00-8000-000000000011';
    expect(parseCronJobId(cronJobId)).toBe(cronJobId);
    expect(parseCronJobRunId(cronJobRunId)).toBe(cronJobRunId);

    for (const value of invalidBusinessIdValues(cronJobId, 'cron')) {
      expectInvalidEntityId(() => parseCronJobId(value));
    }
    for (const value of invalidBusinessIdValues(cronJobRunId, 'cronrun')) {
      expectInvalidEntityId(() => parseCronJobRunId(value));
    }
  });

  test('mcp_server_id accepts only a bare canonical lowercase UUIDv7', () => {
    const mcpServerId = '0190f5fe-7c00-7a00-8000-000000000013';
    expect(parseMcpServerId(mcpServerId)).toBe(mcpServerId);
    for (const value of invalidBusinessIdValues(mcpServerId, 'mcp')) {
      expectInvalidEntityId(() => parseMcpServerId(value));
    }
  });

  test('conversation_artifact_id accepts only a bare canonical lowercase UUIDv7', () => {
    const artifactId = '0190f5fe-7c00-7a00-8000-000000000014';
    expect(parseConversationArtifactId(artifactId)).toBe(artifactId);
    for (const value of invalidBusinessIdValues(artifactId, 'artifact')) {
      expectInvalidEntityId(() => parseConversationArtifactId(value));
    }
  });

  test('intervention_id accepts only a bare canonical lowercase UUIDv7', () => {
    const interventionId = '0190f5fe-7c00-7a00-8000-000000000015';
    expect(parseIdmmInterventionId(interventionId)).toBe(interventionId);
    for (const value of invalidBusinessIdValues(interventionId, 'intervention')) {
      expectInvalidEntityId(() => parseIdmmInterventionId(value));
    }
  });

  test('provider_id accepts only a bare canonical lowercase UUIDv7', () => {
    const providerId = '0190f5fe-7c00-7a00-8000-000000000016';
    expect(parseProviderId(providerId)).toBe(providerId);
    for (const value of invalidBusinessIdValues(providerId, 'provider')) {
      expectInvalidEntityId(() => parseProviderId(value));
    }
  });

  test('preset_id accepts only a bare canonical lowercase UUIDv7', () => {
    const presetId = '0190f5fe-7c00-7a00-8000-000000000017';
    expect(parsePresetId(presetId)).toBe(presetId);
    for (const value of [
      ...invalidBusinessIdValues(presetId, 'preset'),
      'office',
      'builtin:office',
    ]) {
      expectInvalidEntityId(() => parsePresetId(value));
    }
  });
});
