/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import { isBackendHttpError } from '@/common/adapter/httpBridge';
import { parseError } from '@/common/utils';
import type { TFunction } from 'i18next';

export type WorkspacePathErrorCode =
  | 'WORKSPACE_PATH_EDGE_WHITESPACE_UNSUPPORTED'
  | 'WORKSPACE_PATH_EDGE_WHITESPACE_RUNTIME_UNSUPPORTED';

export type ConversationCreateErrorCode = 'WORKSPACE_PATH_EDGE_WHITESPACE_UNSUPPORTED';
export type ConversationRuntimeWorkspaceErrorCode = 'WORKSPACE_PATH_EDGE_WHITESPACE_RUNTIME_UNSUPPORTED';

const BACKEND_ERROR_CODE_MAP: Record<string, WorkspacePathErrorCode> = {
  WORKSPACE_PATH_EDGE_WHITESPACE_UNSUPPORTED: 'WORKSPACE_PATH_EDGE_WHITESPACE_UNSUPPORTED',
  WORKSPACE_PATH_EDGE_WHITESPACE_RUNTIME_UNSUPPORTED: 'WORKSPACE_PATH_EDGE_WHITESPACE_RUNTIME_UNSUPPORTED',
};

const PROVIDER_ERROR_CODES = new Set(['PROVIDER_UNAVAILABLE']);

/** i18n key for a provider-config error code, or undefined if not one. */
export function providerErrorI18nKey(code: string): string | undefined {
  return PROVIDER_ERROR_CODES.has(code) ? `conversation.agentError.codes.${code}.body` : undefined;
}

type EmbeddedBackendErrorPayload = {
  code?: string;
  error?: string;
  details?: unknown;
};

type WorkspacePathErrorDetails = {
  workspace_path?: string;
};

const getEmbeddedBackendErrorPayload = (error: unknown): EmbeddedBackendErrorPayload | undefined => {
  const parsedError = parseError(error);
  const rawMessage =
    typeof error === 'string'
      ? error
      : error instanceof Error
        ? error.message
        : typeof parsedError === 'string'
          ? parsedError
          : '';

  if (!rawMessage) {
    return undefined;
  }

  const jsonStart = rawMessage.indexOf('{');
  if (jsonStart < 0) {
    return undefined;
  }

  try {
    const payload = JSON.parse(rawMessage.slice(jsonStart)) as EmbeddedBackendErrorPayload;
    if (payload && typeof payload === 'object') {
      return payload;
    }
  } catch {
    return undefined;
  }

  return undefined;
};

const getWorkspacePathFromDetails = (details: unknown): string | undefined => {
  if (!details || typeof details !== 'object') {
    return undefined;
  }

  const workspacePath = (details as WorkspacePathErrorDetails).workspace_path;
  return typeof workspacePath === 'string' && workspacePath.trim() ? workspacePath : undefined;
};

const getWorkspacePathErrorPayload = (error: unknown): EmbeddedBackendErrorPayload | undefined => {
  if (isBackendHttpError(error)) {
    return {
      code: error.code,
      error: error.backendMessage,
      details: error.details,
    };
  }

  return getEmbeddedBackendErrorPayload(error);
};

export const getWorkspacePathFromErrorDetails = (error: unknown): string | undefined => {
  const payload = getWorkspacePathErrorPayload(error);
  return getWorkspacePathFromDetails(payload?.details);
};

export const normalizeWorkspacePathErrorCode = (error: unknown): WorkspacePathErrorCode | undefined => {
  const payload = getWorkspacePathErrorPayload(error);
  if (payload) {
    const mappedCode = payload.code ? BACKEND_ERROR_CODE_MAP[payload.code] : undefined;
    if (mappedCode) {
      return mappedCode;
    }
  }

  return undefined;
};

export const normalizeConversationCreateErrorCode = (error: unknown): ConversationCreateErrorCode | undefined => {
  const code = normalizeWorkspacePathErrorCode(error);
  return code === 'WORKSPACE_PATH_EDGE_WHITESPACE_UNSUPPORTED' ? code : undefined;
};

export const normalizeConversationRuntimeWorkspaceErrorCode = (
  error: unknown
): ConversationRuntimeWorkspaceErrorCode | undefined => {
  const code = normalizeWorkspacePathErrorCode(error);
  return code === 'WORKSPACE_PATH_EDGE_WHITESPACE_RUNTIME_UNSUPPORTED' ? code : undefined;
};

export const getConversationCreateErrorMessage = (error: unknown, t: TFunction): string => {
  const normalizedCode = normalizeConversationCreateErrorCode(error);
  const payload = getWorkspacePathErrorPayload(error);
  const workspacePath = getWorkspacePathFromErrorDetails(error);
  const rawMessage = payload?.error || parseError(error) || t('conversation.createFailed');

  if (normalizedCode && workspacePath) {
    return t(`conversation.createError.pathVariants.${normalizedCode}`, {
      workspacePath,
      defaultValue: rawMessage,
    });
  }

  return rawMessage;
};

export const getConversationRuntimeWorkspaceErrorMessage = (error: unknown, t: TFunction): string => {
  const payload = getWorkspacePathErrorPayload(error);
  const providerKey = payload?.code ? providerErrorI18nKey(payload.code) : undefined;
  if (providerKey) {
    return t(providerKey);
  }
  const normalizedCode = normalizeConversationRuntimeWorkspaceErrorCode(error);
  const workspacePath = getWorkspacePathFromErrorDetails(error);
  const rawMessage = payload?.error || parseError(error) || t('common.unknownError');

  if (normalizedCode) {
    if (workspacePath) {
      return t(`conversation.agentError.codes.${normalizedCode}.bodyWithPath`, {
        workspacePath,
        defaultValue: rawMessage,
      });
    }

    return t(`conversation.agentError.codes.${normalizedCode}.body`, {
      defaultValue: rawMessage,
    });
  }

  return rawMessage;
};
