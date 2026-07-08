/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

/**
 * IPC Bridge → HTTP/WS adapter.
 *
 * This file replaces the original IPC bridge calls with HTTP REST and WebSocket
 * calls routed to nomicore. Electron-native operations (window controls,
 * native dialogs, auto-update, devtools, zoom, CDP, deep links) remain as IPC.
 */

import type { IConfirmation } from '@/common/chat/chatLib';
import {
  noopEmitter,
  shellEmitter,
  shellProvider,
  stubShellProvider,
  subscribeDeepLink,
  subscribeWebuiStatus,
  subscribeWindowMaximized,
  tauriGetPath,
  tauriGetZoom,
  tauriIsAutostartEnabled,
  tauriOpenDialog,
  tauriRelaunch,
  tauriSendNotification,
  tauriSetAutostart,
  tauriSetKeepAwake,
  tauriSetTrayLabels,
  tauriSetZoom,
  tauriWebuiGetStatus,
  tauriWebuiStart,
  tauriWebuiStop,
  tauriWindowClose,
  tauriWindowIsMaximized,
  tauriWindowMaximize,
  tauriWindowMinimize,
  tauriWindowToggleMaximize,
  tauriWindowUnmaximize,
  type ShellOpenDialogOptions,
} from './tauriShell';
import {
  autoUpdateStatusEmitter,
  tauriUpdateCheck,
  tauriUpdateCurrentVersion,
  tauriUpdateDownload,
  tauriUpdateInstallAndRelaunch,
} from './tauriUpdater';
import type {
  ICssTheme,
  IMcpServer,
  IProvider,
  ISessionMcpServer,
  ModelProfile,
  TChatConversation,
  TProviderWithModel,
} from '../config/storage';
import type {
  Assistant,
  AssistantTag,
  CreateAssistantRequest,
  CreateAssistantTagRequest,
  ImportAssistantsRequest,
  ImportAssistantsResult,
  SetAssistantStateRequest,
  UpdateAssistantRequest,
  UpdateAssistantTagRequest,
} from '../types/agent/assistantTypes';
import type { PreviewHistoryTarget, PreviewSnapshotInfo } from '../types/office/preview';
import type { AcpModelInfo } from '../types/platform/acpTypes';
import type {
  CreateProviderRequest,
  FetchModelsAnonymousRequest,
  FetchModelsResponse,
  ModelProfileKeyRequest,
  ModelProfileUpsertRequest,
  ProviderHealthCheckRequest,
  ProviderHealthCheckResponse,
  ResolveModelsRequest,
  ResolveModelsResponse,
  UpdateProviderRequest,
} from '../types/provider/providerApi';
import type { SpeechToTextRequest, SpeechToTextResult } from '../types/provider/speech';
import type {
  TAdjustRunRequest,
  TCreateAdhocRun,
  TReassign,
  TModelRange,
  TReplanRequest,
  TRun,
  TRunDetail,
  TSteer,
  TTaskConfigUpdate,
  TTaskSpecUpdate,
} from '../types/orchestrator/orchestratorTypes';
import type {
  TOrchRunCompletedEvent,
  TOrchRunLeadThinkingEvent,
  TOrchRunPlanUpdatedEvent,
  TOrchRunStatusEvent,
  TOrchTaskAssignedEvent,
  TOrchTaskStatusEvent,
} from '../types/orchestrator/orchestratorEvents';
import type {
  AutoUpdateStatus,
  UpdateCheckRequest,
  UpdateCheckResult,
  UpdateDownloadProgressEvent,
  UpdateDownloadRequest,
  UpdateDownloadResult,
  UpdateReleaseInfo,
} from '../update/updateTypes';
import type { ProtocolDetectionRequest, ProtocolDetectionResponse } from '../utils/protocolDetector';
import { fromApiConversation, fromApiPaginatedConversations, toApiModelOptional } from './apiModelMapper';
import {
  httpDelete,
  httpGet,
  httpPatch,
  httpPost,
  httpPut,
  httpRequest,
  stubProvider,
  withResponseMap,
  wsEmitter,
  wsMappedEmitter,
} from './httpBridge';
import { fromApiSearchResult, type ApiMessageSearchItem } from './searchMapper';
import { fromBackendCompareResult, type RawCompareResult } from './fileSnapshotMapper';
import {
  absoluteToRelativePath,
  fromBackendWorkspaceFlatFiles,
  fromBackendWorkspaceList,
  type RawWorkspaceFlatFile,
} from './workspaceMapper';

// ---------------------------------------------------------------------------
// Shell — routed to POST /api/shell/*
// ---------------------------------------------------------------------------

export const shell = {
  openFile: httpPost<void, string>('/api/shell/open-file', (file_path) => ({ file_path })),
  showItemInFolder: httpPost<void, string>('/api/shell/show-item-in-folder', (file_path) => ({ file_path })),
  openExternal: httpPost<void, string>('/api/shell/open-external', (url) => ({ url })),
  checkToolInstalled: httpPost<boolean, { tool: string }>('/api/shell/check-tool-installed'),
  openFolderWith: httpPost<void, { folder_path: string; tool: 'vscode' | 'terminal' | 'explorer' }>(
    '/api/shell/open-folder-with'
  ),
};

// ---------------------------------------------------------------------------
// Assistants — routed to /api/assistants/*
// ---------------------------------------------------------------------------

export const assistants = {
  list: httpGet<Assistant[], void>('/api/assistants'),
  create: httpPost<Assistant, CreateAssistantRequest>('/api/assistants'),
  update: httpPut<Assistant, UpdateAssistantRequest>((p) => `/api/assistants/${p.id}`),
  delete: httpDelete<void, { id: string }>((p) => `/api/assistants/${p.id}`),
  setState: httpPatch<Assistant, SetAssistantStateRequest>(
    (p) => `/api/assistants/${p.id}/state`,
    (p) => {
      const { id: _id, ...body } = p;
      return body;
    }
  ),
  import: httpPost<ImportAssistantsResult, ImportAssistantsRequest>('/api/assistants/import'),
};

// ---------------------------------------------------------------------------
// Assistant Tags — routed to /api/assistant-tags/*
// ---------------------------------------------------------------------------

export const assistantTags = {
  list: httpGet<AssistantTag[], void>('/api/assistant-tags'),
  create: httpPost<AssistantTag, CreateAssistantTagRequest>('/api/assistant-tags'),
  update: httpPut<AssistantTag, UpdateAssistantTagRequest>(
    (p) => `/api/assistant-tags/${p.key}`,
    (p) => {
      const { key: _key, ...body } = p;
      return body;
    }
  ),
  delete: httpDelete<void, { key: string }>((p) => `/api/assistant-tags/${p.key}`),
};

// ---------------------------------------------------------------------------
// Conversation — REST + WS
// ---------------------------------------------------------------------------

export const conversation = {
  create: withResponseMap(
    httpPost<TChatConversation, ICreateConversationParams>('/api/conversations', (p) => {
      // Top-level `model` is nomi-only on the backend (spec 2026-05-12).
      // Other agent types carry model info via `extra`.
      const isNomi = p.type === 'nomi';
      // Conversations are minted by the backend (INTEGER AUTOINCREMENT,
      // numeric-id spec §5) — never send a client-supplied id.
      const body: Record<string, unknown> = {
        type: p.type,
        name: p.name,
        extra: p.extra,
      };
      if (isNomi) {
        const model = toApiModelOptional(p.model);
        if (model) body.model = model;
      }
      return body;
    }),
    fromApiConversation
  ),
  createWithConversation: withResponseMap(
    httpPost<TChatConversation, { conversation: TChatConversation }>('/api/conversations/clone', (p) => {
      const isNomi = p.conversation.type === 'nomi';
      // Drop `id` here too: conversations use backend-minted INTEGER ids
      // (numeric-id spec §5), so the clone endpoint assigns a fresh one — the
      // source id must never leak into the new row.
      const { model: _rawModel, id: _sourceId, ...rest } = p.conversation as TChatConversation & {
        model?: TProviderWithModel;
      };
      const clonedConversation: Record<string, unknown> = { ...rest };
      if (isNomi) {
        const model = toApiModelOptional(_rawModel);
        if (model) clonedConversation.model = model;
      }
      return {
        conversation: clonedConversation,
      };
    }),
    fromApiConversation
  ),
  get: withResponseMap(
    httpGet<TChatConversation, { id: number }>((p) => `/api/conversations/${p.id}`, { silentStatuses: [404] }),
    fromApiConversation
  ),
  getAssociateConversation: withResponseMap(
    httpGet<TChatConversation[], { conversation_id: number }>(
      (p) => `/api/conversations/${p.conversation_id}/associated`
    ),
    (list) => list.map(fromApiConversation)
  ),
  listByCronJob: withResponseMap(
    httpGet<TChatConversation[], { cron_job_id: string }>((p) => `/api/cron/jobs/${p.cron_job_id}/conversations`),
    (list) => list.map(fromApiConversation)
  ),
  remove: httpDelete<boolean, { id: number }>((p) => `/api/conversations/${p.id}`),
  // updates 额外允许顶层 `pinned`：对应 conversations 表真列（UpdateConversationRequest.pinned，
  // 服务端置位时自动维护 pinned_at）；body 构造的 `...rest` 原样透传该字段。
  update: httpPatch<boolean, { id: number; updates: Partial<TChatConversation> & { pinned?: boolean }; merge_extra?: boolean }>(
    (p) => `/api/conversations/${p.id}`,
    (p) => {
      const updates = p.updates as Record<string, unknown>;
      const { model: rawModel, ...rest } = updates;
      const model = toApiModelOptional(rawModel as TProviderWithModel | undefined);
      return {
        ...rest,
        ...(model ? { model } : {}),
        merge_extra: p.merge_extra,
      };
    }
  ),
  reset: httpPost<void, IResetConversationParams>((p) => `/api/conversations/${p.id}/reset`),
  warmup: httpPost<void, { conversation_id: number }>((p) => `/api/conversations/${p.conversation_id}/warmup`),
  stop: httpPost<void, { conversation_id: number }>((p) => `/api/conversations/${p.conversation_id}/cancel`),
  clearContext: httpPost<void, { conversation_id: number }>(
    (p) => `/api/conversations/${p.conversation_id}/clear-context`
  ),
  /** 清空一条会话的全部消息（保留会话行，不触碰 companion_memories 记忆库）。
   *  伙伴专属会话「清空上下文」按钮调用。 */
  clearMessages: httpPost<boolean, { id: number }>((p) => `/api/conversations/${p.id}/clear-messages`),
  activeCount: httpGet<{ count: number }>('/api/conversations/active-count'),
  sendMessage: httpPost<ISendMessageResult, ISendMessageParams>(
    (p) => `/api/conversations/${p.conversation_id}/messages`,
    (p) => ({
      content: p.input,
      files: p.files,
      loading_id: p.loading_id,
      inject_skills: p.inject_skills,
    })
  ),
  steer: httpPost<ISendMessageResult, ISendMessageParams>(
    (p) => `/api/conversations/${p.conversation_id}/steer`,
    (p) => ({
      content: p.input,
      files: p.files,
      inject_skills: p.inject_skills,
    })
  ),
  editResubmit: httpPost<ISendMessageResult, { conversation_id: number; msg_id: string; input: string; files?: string[] }>(
    (p) => `/api/conversations/${p.conversation_id}/messages/${p.msg_id}/edit-resubmit`,
    (p) => ({
      content: p.input,
      files: p.files,
    })
  ),
  getSlashCommands: httpGet<Array<{ command: string; description: string }>, { conversation_id: number }>(
    (p) => `/api/conversations/${p.conversation_id}/slash-commands`
  ),
  askSideQuestion: httpPost<ConversationSideQuestionResult, { conversation_id: number; question: string }>(
    (p) => `/api/conversations/${p.conversation_id}/side-question`,
    (p) => ({ question: p.question })
  ),
  confirmMessage: httpPost<void, IConfirmMessageParams>(
    (p) => `/api/conversations/${p.conversation_id}/confirmations/${encodeURIComponent(p.call_id)}/confirm`,
    (p) => ({ msg_id: p.msg_id, data: p.confirm_key })
  ),
  listArtifacts: httpGet<IConversationArtifact[], { conversation_id: number }>(
    (p) => `/api/conversations/${p.conversation_id}/artifacts`
  ),
  updateArtifact: httpPatch<
    IConversationArtifact,
    { conversation_id: number; artifact_id: number; status: IConversationArtifactStatus }
  >(
    (p) => `/api/conversations/${p.conversation_id}/artifacts/${p.artifact_id}`,
    (p) => ({ status: p.status })
  ),
  responseStream: wsEmitter<IResponseMessage>('message.stream'),
  /** A user message was persisted (incl. IM channel inbound — see
   *  IUserMessageCreatedEvent). */
  userCreated: wsEmitter<IUserMessageCreatedEvent>('message.userCreated'),
  artifactStream: wsEmitter<IConversationArtifact>('conversation.artifact'),
  turnStarted: wsMappedEmitter<IConversationTurnStartedEvent>('turn.started', (raw) => {
    const r = raw as Record<string, unknown>;
    const rawRuntime = (r.runtime ?? {}) as Record<string, unknown>;
    const rawProcessingStartedAt = rawRuntime.processing_started_at ?? rawRuntime.processingStartedAt;
    const processing_started_at =
      typeof rawProcessingStartedAt === 'number'
        ? rawProcessingStartedAt
        : typeof rawProcessingStartedAt === 'string'
          ? Number(rawProcessingStartedAt)
          : undefined;
    return {
      session_id: Number(r.session_id ?? r.sessionId ?? r.conversation_id ?? 0),
      conversation_id: Number(r.conversation_id ?? r.session_id ?? r.sessionId ?? 0),
      turn_id: (r.turn_id ?? r.turnId) as string | undefined,
      status: (r.status ?? 'running') as IConversationTurnStartedEvent['status'],
      phase: (r.phase ?? 'starting') as IConversationTurnStartedEvent['phase'],
      state: (r.state ?? 'initializing') as IConversationTurnStartedEvent['state'],
      detail: (r.detail ?? '') as string,
      can_send_message: (r.can_send_message ?? r.canSendMessage ?? false) as boolean,
      runtime: {
        state: (rawRuntime.state ?? 'starting') as IConversationTurnStartedEvent['runtime']['state'],
        can_send_message: (rawRuntime.can_send_message ?? rawRuntime.canSendMessage ?? false) as boolean,
        has_task: (rawRuntime.has_task ?? rawRuntime.hasTask ?? true) as boolean,
        task_status: (rawRuntime.task_status ??
          rawRuntime.taskStatus) as IConversationTurnStartedEvent['runtime']['task_status'],
        is_processing: (rawRuntime.is_processing ?? rawRuntime.isProcessing ?? true) as boolean,
        pending_confirmations: (rawRuntime.pending_confirmations ?? rawRuntime.pendingConfirmations ?? 0) as number,
        ...(Number.isFinite(processing_started_at) ? { processing_started_at } : {}),
      },
      companion: r.companion as boolean | undefined,
      companion_id: (r.companion_id ?? r.companionId) as string | null | undefined,
      origin: (r.origin ?? null) as string | null | undefined,
      channel_platform: (r.channel_platform ?? r.channelPlatform) as string | null | undefined,
    };
  }),
  turnCompleted: wsMappedEmitter<IConversationTurnCompletedEvent>('turn.completed', (raw) => {
    const r = raw as Record<string, unknown>;
    const rawLast = (r.last_message ?? r.lastMessage) as Record<string, unknown> | undefined;
    const last_message: IConversationTurnCompletedEvent['last_message'] = rawLast
      ? {
          id: rawLast.id as string | undefined,
          type: rawLast.type as string | undefined,
          content: rawLast.content ?? null,
          status: rawLast.status as string | null | undefined,
          created_at: (rawLast.created_at ?? rawLast.createdAt ?? Date.now()) as number,
        }
      : {
          content: null,
          created_at: Date.now(),
        };
    const rawRuntime = (r.runtime ?? {}) as Record<string, unknown>;
    const runtime: IConversationTurnCompletedEvent['runtime'] = {
      state: (rawRuntime.state ?? 'idle') as IConversationTurnCompletedEvent['runtime']['state'],
      can_send_message: (rawRuntime.can_send_message ?? rawRuntime.canSendMessage ?? true) as boolean,
      has_task: (rawRuntime.has_task ?? rawRuntime.hasTask ?? false) as boolean,
      task_status: (rawRuntime.task_status ??
        rawRuntime.taskStatus) as IConversationTurnCompletedEvent['runtime']['task_status'],
      is_processing: (rawRuntime.is_processing ?? rawRuntime.isProcessing ?? false) as boolean,
      pending_confirmations: (rawRuntime.pending_confirmations ?? rawRuntime.pendingConfirmations ?? 0) as number,
    };
    const rawModel = (r.model ?? {}) as Record<string, unknown>;
    const model: IConversationTurnCompletedEvent['model'] = {
      platform: (rawModel.platform ?? '') as string,
      name: (rawModel.name ?? '') as string,
      use_model: (rawModel.use_model ?? rawModel.useModel ?? '') as string,
    };
    return {
      session_id: Number(r.session_id ?? r.sessionId ?? r.conversation_id ?? 0),
      status: (r.status ?? 'finished') as IConversationTurnCompletedEvent['status'],
      state: (r.state ??
        (r.status === 'finished' ? 'ai_waiting_input' : 'unknown')) as IConversationTurnCompletedEvent['state'],
      detail: (r.detail ?? '') as string,
      can_send_message: (r.can_send_message ?? r.canSendMessage ?? r.status === 'finished') as boolean,
      runtime,
      workspace: (r.workspace ?? '') as string,
      model,
      last_message,
    };
  }),
  listChanged: wsEmitter<IConversationListChangedEvent>('conversation.listChanged'),
  // Uses httpRequest directly (instead of httpGet + withResponseMap) because the
  // response mapper needs `workspace` from params to build fullPath/relativePath,
  // and withResponseMap's map function does not receive the original params.
  getWorkspace: {
    provider: () => {},
    invoke: (async (p: { conversation_id: number; workspace: string; path: string; search?: string }) => {
      const rel = absoluteToRelativePath(p.path, p.workspace);
      const url = `/api/conversations/${p.conversation_id}/workspace?path=${encodeURIComponent(rel)}${p.search ? `&search=${encodeURIComponent(p.search)}` : ''}`;
      const raw = await httpRequest<Array<{ name: string; type: string }>>('GET', url);
      return fromBackendWorkspaceList(raw, p.workspace, rel);
    }) as (p: { conversation_id: number; workspace: string; path: string; search?: string }) => Promise<IDirOrFile[]>,
  },
  responseSearchWorkSpace: stubProvider<void, { file: number; dir: number; match?: IDirOrFile }>(
    'responseSearchWorkSpace',
    undefined as unknown as void
  ),
  confirmation: {
    add: wsEmitter<IConfirmation<unknown> & { conversation_id: number }>('confirmation.add'),
    update: wsEmitter<IConfirmation<unknown> & { conversation_id: number }>('confirmation.update'),
    confirm: httpPost<
      void,
      { conversation_id: number; msg_id: string; data: unknown; call_id: string; always_allow?: boolean }
    >(
      (p) => `/api/conversations/${p.conversation_id}/confirmations/${encodeURIComponent(p.call_id)}/confirm`,
      (p) => ({ msg_id: p.msg_id, data: p.data, always_allow: p.always_allow ?? false })
    ),
    list: httpGet<IConfirmation<unknown>[], { conversation_id: number }>(
      (p) => `/api/conversations/${p.conversation_id}/confirmations`
    ),
    remove: wsEmitter<{ conversation_id: number; id: string }>('confirmation.remove'),
  },
  approval: {
    check: httpGet<{ approved: boolean }, { conversation_id: number; action: string; command_type?: string }>(
      (p) =>
        `/api/conversations/${p.conversation_id}/approvals/check?action=${encodeURIComponent(p.action)}${p.command_type ? `&command_type=${encodeURIComponent(p.command_type)}` : ''}`
    ),
  },
};

// ---------------------------------------------------------------------------
// CDP status / config types (used by application, stays IPC)
// ---------------------------------------------------------------------------

export interface ICdpStatus {
  enabled: boolean;
  port: number | null;
  startupEnabled: boolean;
  instances: Array<{
    pid: number;
    port: number;
    cwd: string;
    startTime: number;
  }>;
  configEnabled: boolean;
  isDevMode: boolean;
}

export interface ICdpConfig {
  enabled?: boolean;
  port?: number;
}

export interface IStartOnBootStatus {
  supported: boolean;
  enabled: boolean;
  isPackaged: boolean;
  platform: string;
}

/** Hardware acceleration / GPU recovery status — see process/utils/gpuRecovery */
export type IGpuOverride = 'force-on' | 'force-off';

export interface IGpuStatus {
  /** User-set override; null means follow auto-recovery */
  userOverride: IGpuOverride | null;
  /** Whether auto-recovery has disabled hardware acceleration after repeated crashes */
  autoDisabled: boolean;
  crashCount: number;
  lastCrashAt: number | null;
}

export type IRendererLogLevel = 'info' | 'warn' | 'error';

export interface IRendererLogEntry {
  level: IRendererLogLevel;
  tag: string;
  message: string;
  data?: unknown;
}

// ---------------------------------------------------------------------------
// Application — stays IPC (Electron-native)
// ---------------------------------------------------------------------------

export const application = {
  restart: shellProvider<void, void>(() => tauriRelaunch(), undefined),
  // Arm a factory reset: the backend writes a marker and the wipe happens early
  // on the next boot (see nomifun_common::factory_reset). Callers should relaunch
  // (application.restart) right after this resolves.
  factoryReset: httpPost<void, void>('/api/system/factory-reset'),
  // DEGRADE_STUB: Tauri v2 has no public JS API to toggle the webview devtools.
  openDevTools: stubShellProvider<boolean, void>(false),
  isDevToolsOpened: stubShellProvider<boolean, void>(false),
  systemInfo: withResponseMap(
    httpGet<{ cache_dir: string; work_dir: string; log_dir: string; platform: string; arch: string }, void>(
      '/api/system/info'
    ),
    (raw) => ({
      cacheDir: raw.cache_dir,
      workDir: raw.work_dir,
      logDir: raw.log_dir,
      platform: raw.platform,
      arch: raw.arch,
    })
  ),
  getPath: shellProvider<string, { name: 'desktop' | 'home' | 'downloads' }>(({ name }) => tauriGetPath(name), ''),
  // Persist the user-chosen work dir to a pre-boot config file that the next
  // boot reads before resolving work_dir (Rust `nomifun_common::dir_config`).
  // The caller restarts right after this resolves; the new dir applies then.
  // `cacheDir` is accepted for back-compat but ignored — it is no longer
  // user-editable (removed from the settings UI), only `workDir` is sent.
  updateSystemInfo: httpPost<void, { cacheDir: string; workDir: string }>(
    '/api/system/work-dir',
    ({ workDir }) => ({ work_dir: workDir })
  ),
  getZoomFactor: shellProvider<number, void>(async () => tauriGetZoom(), 1),
  setZoomFactor: shellProvider<number, { factor: number }>(({ factor }) => tauriSetZoom(factor), 1),
  applyKeepAwake: shellProvider<void, { enabled: boolean }>(({ enabled }) => tauriSetKeepAwake(enabled), undefined),
  // Localize the native system-tray menu. Desktop-only OS effect (no-op on web),
  // mirroring applyKeepAwake — the renderer calls it on mount / language change.
  setTrayLabels: shellProvider<void, { show: string; quit: string }>(
    ({ show, quit }) => tauriSetTrayLabels(show, quit),
    undefined
  ),
  // DEGRADE_STUB: Tauri (WebView2/WKWebView) exposes no Chrome DevTools Protocol surface.
  getCdpStatus: stubShellProvider<IBridgeResponse<ICdpStatus>, void>({
    success: true,
    data: { enabled: false, port: null, startupEnabled: false, instances: [], configEnabled: false, isDevMode: false },
  }),
  updateCdpConfig: stubShellProvider<IBridgeResponse<ICdpConfig>, Partial<ICdpConfig>>({
    success: false,
    msg: 'CDP not supported in the Tauri shell',
  }),
  getStartOnBootStatus: shellProvider<IBridgeResponse<IStartOnBootStatus>, void>(
    async () => ({
      success: true,
      data: { supported: true, enabled: await tauriIsAutostartEnabled(), isPackaged: true, platform: navigator.platform },
    }),
    { success: false }
  ),
  setStartOnBoot: shellProvider<IBridgeResponse<IStartOnBootStatus>, { enabled: boolean }>(
    async ({ enabled }) => {
      await tauriSetAutostart(enabled);
      return { success: true, data: { supported: true, enabled, isPackaged: true, platform: navigator.platform } };
    },
    { success: false }
  ),
  // DEGRADE_STUB: no GPU-process recovery hooks in Tauri's webview.
  getGpuStatus: stubShellProvider<IBridgeResponse<IGpuStatus>, void>({
    success: true,
    data: { userOverride: null, autoDisabled: false, crashCount: 0, lastCrashAt: null },
  }),
  setGpuOverride: stubShellProvider<IBridgeResponse<IGpuStatus>, { override: IGpuOverride | null }>({
    success: true,
    data: { userOverride: null, autoDisabled: false, crashCount: 0, lastCrashAt: null },
  }),
  // DEGRADE_STUB: renderer-log piping to the shell; the in-process backend owns log files.
  writeRendererLog: stubShellProvider<void, IRendererLogEntry>(undefined),
  logStream: noopEmitter<{ level: 'log' | 'warn' | 'error'; tag: string; message: string; data?: unknown }>(),
  devToolsStateChanged: noopEmitter<{ isOpen: boolean }>(),
};

// ---------------------------------------------------------------------------
// Update — stays IPC (Electron-native auto-updater)
// ---------------------------------------------------------------------------

// Tauri-native auto-update, backed by @tauri-apps/plugin-updater (see
// ./tauriUpdater). The in-app UpdateModal drives this flow: it calls
// `autoUpdate.check` then `update.check`, and — because the Tauri updater plugin
// downloads + installs internally (no per-asset manual download, so
// `recommendedAsset` is intentionally absent) — routes the download through
// `autoUpdate.download`. The modal is shell-gated (About entry + startup check
// only render under `isDesktopShell()`), and `shellProvider` additionally guards
// each call with `isTauri()`, so the WebUI browser degrades to the safe fallback.

/** Releases page shown in the modal's "go to release" affordance. */
const GITHUB_RELEASES_PAGE = 'https://github.com/nomifun/nomifun-tauri/releases/latest';

export const update = {
  open: noopEmitter<{ source?: 'menu' | 'about' }>(),
  check: shellProvider<IBridgeResponse<UpdateCheckResult>, UpdateCheckRequest>(async () => {
    // Reuses the check started by autoUpdate.check (the modal calls that first),
    // so this is the SAME round-trip, not a second network call.
    const currentVersion = await tauriUpdateCurrentVersion();
    const info = await tauriUpdateCheck(false);
    if (!info) {
      return { success: true, data: { currentVersion, updateAvailable: false } };
    }
    const latest: UpdateReleaseInfo = {
      tagName: `v${info.version}`,
      version: info.version,
      body: info.releaseNotes,
      htmlUrl: GITHUB_RELEASES_PAGE,
      prerelease: false,
      draft: false,
      assets: [],
      // recommendedAsset intentionally omitted: the plugin handles download +
      // install, so the modal routes through the autoUpdate.* channels below.
    };
    return { success: true, data: { currentVersion, updateAvailable: true, latest } };
  }, { success: false, msg: 'Updater is unavailable outside the desktop shell' }),
  // Unused under Tauri (no recommendedAsset → the modal never takes the manual
  // download path); kept for API compatibility with the modal's manual branch.
  download: stubShellProvider<IBridgeResponse<UpdateDownloadResult>, UpdateDownloadRequest>({
    success: false,
    msg: 'Use the Tauri updater (auto path)',
  }),
  downloadProgress: noopEmitter<UpdateDownloadProgressEvent>(),
};

export const autoUpdate = {
  check: shellProvider<
    IBridgeResponse<{ updateInfo?: { version: string; releaseDate?: string; releaseNotes?: string } }>,
    { includePrerelease?: boolean }
  >(async () => {
    // `force` so each modal open / retry performs a fresh check; update.check
    // (called right after) then reuses this same in-flight result.
    const info = await tauriUpdateCheck(true);
    if (!info) return { success: true, data: {} };
    return {
      success: true,
      data: { updateInfo: { version: info.version, releaseDate: info.releaseDate, releaseNotes: info.releaseNotes } },
    };
  }, { success: false }),
  download: shellProvider<IBridgeResponse, void>(async () => {
    await tauriUpdateDownload((s) => autoUpdateStatusEmitter.emit(s));
    return { success: true };
  }, { success: false }),
  quitAndInstall: shellProvider<void, void>(() => tauriUpdateInstallAndRelaunch(), undefined),
  status: autoUpdateStatusEmitter,
};

// ---------------------------------------------------------------------------
// Star Office — routed to backend
// ---------------------------------------------------------------------------

export const starOffice = {
  detectUrl: httpPost<{ url: string | null }, { preferredUrl?: string; force?: boolean; timeoutMs?: number }>(
    '/api/star-office/detect'
  ),
};

// ---------------------------------------------------------------------------
// Dialog — stays IPC (native file picker)
// ---------------------------------------------------------------------------

export const dialog = {
  showOpen: shellProvider<string[] | undefined, ShellOpenDialogOptions | void>(
    (opts) => tauriOpenDialog(opts || undefined),
    undefined
  ),
};

// ---------------------------------------------------------------------------
// File System — routed to /api/fs/* and /api/skills/*
// ---------------------------------------------------------------------------

export const fs = {
  getFilesByDir: httpPost<Array<IDirOrFile>, { dir: string; root: string }>('/api/fs/dir'),
  listWorkspaceFiles: withResponseMap(
    httpPost<Array<RawWorkspaceFlatFile>, { root: string }>('/api/fs/list'),
    fromBackendWorkspaceFlatFiles
  ),
  getImageBase64: httpPost<string | null, { path: string; workspace?: string }>('/api/fs/image-base64'),
  fetchRemoteImage: httpPost<string, { url: string }>('/api/fs/fetch-remote-image'),
  readFile: httpPost<string | null, { path: string; workspace?: string }>('/api/fs/read'),
  readFileBuffer: httpPost<string | null, { path: string; workspace?: string }>('/api/fs/read-buffer'),
  createTempFile: httpPost<string, { file_name: string }>('/api/fs/temp'),
  writeFile: httpPost<boolean, { path: string; data: string }>('/api/fs/write'),
  createZip: httpPost<
    boolean,
    {
      path: string;
      request_id?: string;
      files: Array<{
        name: string;
        content?: string | Uint8Array;
        source_path?: string;
      }>;
    }
  >('/api/fs/zip'),
  cancelZip: httpPost<boolean, { request_id: string }>('/api/fs/zip/cancel'),
  getFileMetadata: httpPost<IFileMetadata, { path: string; workspace?: string }>('/api/fs/metadata'),
  copyFilesToWorkspace: httpPost<
    { copied_files: string[]; failed_files?: Array<{ path: string; error: string }> },
    { file_paths: string[]; workspace: string; source_root?: string }
  >('/api/fs/copy'),
  removeEntry: httpPost<void, { path: string }>('/api/fs/remove'),
  renameEntry: httpPost<{ new_path: string }, { path: string; new_name: string }>('/api/fs/rename'),
  readBuiltinRule: httpPost<string, { file_name: string }>('/api/skills/builtin-rule'),
  readBuiltinSkill: httpPost<string, { file_name: string }>('/api/skills/builtin-skill'),
  readAssistantRule: httpPost<string, { assistant_id: string; locale?: string }>('/api/skills/assistant-rule/read'),
  writeAssistantRule: httpPost<boolean, { assistant_id: string; content: string; locale?: string }>(
    '/api/skills/assistant-rule/write'
  ),
  deleteAssistantRule: httpDelete<boolean, { assistant_id: string }>(
    (p) => `/api/skills/assistant-rule/${p.assistant_id}`
  ),
  readAssistantSkill: httpPost<string, { assistant_id: string; locale?: string }>('/api/skills/assistant-skill/read'),
  writeAssistantSkill: httpPost<boolean, { assistant_id: string; content: string; locale?: string }>(
    '/api/skills/assistant-skill/write'
  ),
  deleteAssistantSkill: httpDelete<boolean, { assistant_id: string }>(
    (p) => `/api/skills/assistant-skill/${p.assistant_id}`
  ),
  listAvailableSkills: httpGet<
    Array<{
      name: string;
      description: string;
      location: string;
      relative_location?: string;
      is_custom: boolean;
      source: 'builtin' | 'custom' | 'extension';
      audience_tags?: string[];
      scenario_tags?: string[];
    }>,
    void
  >('/api/skills'),
  listBuiltinAutoSkills: httpGet<Array<{ name: string; description: string; location: string }>, void>(
    '/api/skills/builtin-auto'
  ),
  materializeSkillsForAgent: httpPost<
    { skills: Array<{ name: string; source_path: string }> },
    { conversation_id: number; skills: string[] }
  >('/api/skills/materialize-for-agent'),
  readSkillInfo: httpPost<{ name: string; description: string }, { skill_path: string }>('/api/skills/info'),
  importSkill: httpPost<{ skill_name: string }, { skill_path: string }>('/api/skills/import'),
  scanForSkills: httpPost<Array<{ name: string; description: string; path: string }>, { folder_path: string }>(
    '/api/skills/scan'
  ),
  detectCommonSkillPaths: httpGet<Array<{ name: string; path: string }>, void>('/api/skills/detect-paths'),
  detectAndCountExternalSkills: httpGet<
    Array<{
      name: string;
      path: string;
      source: string;
      skill_count: number;
      skills: Array<{ name: string; description: string; path: string }>;
    }>,
    void
  >('/api/skills/detect-external'),
  importSkillWithSymlink: httpPost<{ skill_name: string; skill_names?: string[] }, { skill_path: string }>(
    '/api/skills/import-symlink'
  ),
  deleteSkill: httpDelete<void, { skill_name: string }>((p) => `/api/skills/${encodeURIComponent(p.skill_name)}`),
  // Assign tags to a skill (PUT /api/skills/{name}/tags). Tag keys reference the
  // shared assistant tag vocabulary; the backend stores them in a sidecar table.
  setSkillTags: httpPut<void, { skill_name: string; audience_tags: string[]; scenario_tags: string[] }>(
    (p) => `/api/skills/${encodeURIComponent(p.skill_name)}/tags`,
    (p) => ({ audience_tags: p.audience_tags, scenario_tags: p.scenario_tags })
  ),
  getSkillPaths: httpGet<{ user_skills_dir: string; builtin_skills_dir: string }, void>('/api/skills/paths'),
  getCustomExternalPaths: httpGet<Array<{ name: string; path: string }>, void>('/api/skills/external-paths'),
  addCustomExternalPath: httpPost<void, { name: string; path: string }>('/api/skills/external-paths'),
  removeCustomExternalPath: httpDelete<void, { path: string }>(
    (p) => `/api/skills/external-paths?path=${encodeURIComponent(p.path)}`
  ),
  enableSkillsMarket: httpPost<void, void>('/api/skills/market/enable'),
  disableSkillsMarket: httpPost<void, void>('/api/skills/market/disable'),
};

// ---------------------------------------------------------------------------
// Speech to Text — routed to backend
// ---------------------------------------------------------------------------

export const speechToText = {
  transcribe: httpPost<SpeechToTextResult, SpeechToTextRequest>('/api/stt'),
};

// ---------------------------------------------------------------------------
// File Watch — routed to /api/fs/watch/*
// ---------------------------------------------------------------------------

export const fileWatch = {
  startWatch: httpPost<void, { file_path: string }>('/api/fs/watch/start'),
  stopWatch: httpPost<void, { file_path: string }>('/api/fs/watch/stop'),
  stopAllWatches: httpPost<void, void>('/api/fs/watch/stop-all'),
  fileChanged: wsEmitter<{ file_path: string; event_type: string }>('fileWatch.fileChanged'),
};

// Workspace Office file watch
export const workspaceOfficeWatch = {
  start: httpPost<void, { workspace: string }>('/api/fs/office-watch/start'),
  stop: httpPost<void, { workspace: string }>('/api/fs/office-watch/stop'),
  fileAdded: wsEmitter<{ file_path: string; workspace: string }>('workspaceOfficeWatch.fileAdded'),
};

// File streaming updates (real-time content push when agent writes)
export const fileStream = {
  contentUpdate: wsEmitter<{
    file_path: string;
    content: string;
    workspace: string;
    relative_path: string;
    operation: 'write' | 'delete';
  }>('fileStream.contentUpdate'),
};

// File snapshot providers
export const fileSnapshot = {
  init: httpPost<import('@/common/types/platform/fileSnapshot').SnapshotInfo, { workspace: string }>(
    '/api/fs/snapshot/init'
  ),
  compare: withResponseMap(
    httpPost<RawCompareResult, { workspace: string }>('/api/fs/snapshot/compare'),
    fromBackendCompareResult
  ),
  getBaselineContent: httpPost<string | null, { workspace: string; file_path: string }>('/api/fs/snapshot/baseline'),
  getInfo: httpPost<import('@/common/types/platform/fileSnapshot').SnapshotInfo, { workspace: string }>(
    '/api/fs/snapshot/info'
  ),
  dispose: httpPost<void, { workspace: string }>('/api/fs/snapshot/dispose'),
  stageFile: httpPost<void, { workspace: string; file_path: string }>('/api/fs/snapshot/stage'),
  stageAll: httpPost<void, { workspace: string }>('/api/fs/snapshot/stage-all'),
  unstageFile: httpPost<void, { workspace: string; file_path: string }>('/api/fs/snapshot/unstage'),
  unstageAll: httpPost<void, { workspace: string }>('/api/fs/snapshot/unstage-all'),
  discardFile: httpPost<
    void,
    {
      workspace: string;
      file_path: string;
      operation: import('@/common/types/platform/fileSnapshot').FileChangeOperation;
    }
  >('/api/fs/snapshot/discard'),
  resetFile: httpPost<
    void,
    {
      workspace: string;
      file_path: string;
      operation: import('@/common/types/platform/fileSnapshot').FileChangeOperation;
    }
  >('/api/fs/snapshot/reset'),
  getBranches: httpPost<string[], { workspace: string }>('/api/fs/snapshot/branches'),
};

// ---------------------------------------------------------------------------
// Google Auth — stubbed (Electron-native OAuth flow)
// ---------------------------------------------------------------------------

export const googleAuth = {
  status: stubProvider<IBridgeResponse<{ account: string }>, { proxy?: string }>('googleAuth.status', {
    success: false,
    msg: 'Google Auth not available in backend mode',
  }),
};

// ---------------------------------------------------------------------------
// Google subscription status (Google OAuth provider path, used by nomi)
// ---------------------------------------------------------------------------

export const google = {
  subscriptionStatus: httpGet<
    { isSubscriber: boolean; tier?: string; lastChecked: number; message?: string },
    { proxy?: string }
  >('/api/google/subscription-status'),
};

// ---------------------------------------------------------------------------
// Bedrock connection test
// ---------------------------------------------------------------------------

export const bedrock = {
  testConnection: httpPost<
    { msg?: string },
    {
      bedrock_config: {
        auth_method: 'accessKey' | 'profile';
        region: string;
        access_key_id?: string;
        secret_access_key?: string;
        profile?: string;
      };
    }
  >('/api/bedrock/test-connection'),
};

// ---------------------------------------------------------------------------
// Mode (Provider management) — routed to /api/providers/*
// ---------------------------------------------------------------------------

export const mode = {
  listProviders: httpGet<IProvider[], void>('/api/providers'),
  createProvider: httpPost<IProvider, CreateProviderRequest>('/api/providers'),
  updateProvider: httpPut<IProvider, { id: string } & UpdateProviderRequest>(
    (p) => `/api/providers/${p.id}`,
    (p) => {
      const { id: _id, ...body } = p;
      return body;
    }
  ),
  deleteProvider: httpDelete<void, { id: string }>((p) => `/api/providers/${p.id}`),
  fetchProviderModels: httpPost<FetchModelsResponse, { id: string; try_fix?: boolean }>(
    (p) => `/api/providers/${p.id}/models`,
    (p) => ({ try_fix: p.try_fix })
  ),
  /**
   * Pre-create form preview — anonymous fetch-models (T1b).
   * Takes credentials in the body, no provider row required. Used by
   * AddPlatformModal / EditModeModal / ApiKeyEditorModal while the
   * dropdown is still being populated.
   */
  fetchModelList: httpPost<FetchModelsResponse, FetchModelsAnonymousRequest>('/api/providers/fetch-models'),
  detectProtocol: httpPost<ProtocolDetectionResponse, ProtocolDetectionRequest>('/api/providers/detect-protocol'),
};

// ---------------------------------------------------------------------------
// Model profiles (multimodal model hub) — routed to /api/model-profiles/*
// ---------------------------------------------------------------------------

export const modelProfile = {
  list: httpGet<ModelProfile[], void>('/api/model-profiles'),
  upsert: httpPost<ModelProfile, ModelProfileUpsertRequest>('/api/model-profiles'),
  remove: httpPost<void, ModelProfileKeyRequest>('/api/model-profiles/delete'),
  resolve: httpPost<ResolveModelsResponse, ResolveModelsRequest>('/api/model-profiles/resolve'),
};

// ---------------------------------------------------------------------------
// ACP Conversation — routed to /api/agents/* + conversation routes
// ---------------------------------------------------------------------------

export const acpConversation = {
  sendMessage: conversation.sendMessage,
  responseStream: conversation.responseStream,
  getAvailableAgents: httpGet<AgentMetadata[], void>('/api/agents'),
  refreshCustomAgents: httpPost<void, void>('/api/agents/refresh'),
  testCustomAgent: httpPost<
    { step: 'success' } | { step: 'fail_cli'; error: string } | { step: 'fail_acp'; error: string },
    { command: string; acp_args?: string[]; env?: Record<string, string> }
  >('/api/agents/custom/try-connect'),
  createCustomAgent: httpPost<
    AgentMetadata,
    {
      name: string;
      command: string;
      icon?: string;
      args?: string[];
      env?: Array<{ name: string; value: string; description?: string }>;
      advanced?: {
        yolo_id?: string;
        native_skills_dirs?: string[];
        behavior_policy?: { supports_side_question?: boolean };
        description?: string;
      };
    }
  >('/api/agents/custom'),
  updateCustomAgent: httpPut<
    AgentMetadata,
    {
      id: string;
      name: string;
      command: string;
      icon?: string;
      args?: string[];
      env?: Array<{ name: string; value: string; description?: string }>;
      advanced?: {
        yolo_id?: string;
        native_skills_dirs?: string[];
        behavior_policy?: { supports_side_question?: boolean };
        description?: string;
      };
    }
  >(
    (p) => `/api/agents/custom/${p.id}`,
    (p) => {
      const { id: _id, ...rest } = p;
      return rest;
    }
  ),
  deleteCustomAgent: httpDelete<{ deleted: boolean }, { id: string }>((p) => `/api/agents/custom/${p.id}`),
  setAgentEnabled: httpPatch<AgentMetadata, { id: string; enabled: boolean }>(
    (p) => `/api/agents/${p.id}/enabled`,
    (p) => ({ enabled: p.enabled })
  ),
  checkAgentHealth: httpPost<{ available: boolean; latency?: number; error?: string }, { backend: string }>(
    '/api/agents/health-check'
  ),
  checkProviderHealth: httpPost<ProviderHealthCheckResponse, ProviderHealthCheckRequest>(
    '/api/agents/provider-health-check'
  ),
  setMode: httpPut<void, { conversation_id: number; mode: string }>(
    (p) => `/api/conversations/${p.conversation_id}/mode`,
    (p) => ({ mode: p.mode })
  ),
  // 404 is the expected pre-warmup response from `/api/conversations/:id/mode`
  // and `/api/conversations/:id/model` — the agent has not attached yet, so
  // we have nothing to read. AcpModeSelector / AcpModelSelector both fall back
  // to handshake metadata in that case. Silence the bridge log so this
  // ordinary state doesn't pollute Sentry breadcrumbs (ELECTRON-1BT).
  getMode: httpGet<{ mode: string; initialized: boolean }, { conversation_id: number }>(
    (p) => `/api/conversations/${p.conversation_id}/mode`,
    { silentStatuses: [404] }
  ),
  getModel: httpGet<{ model_info: AcpModelInfo | null }, { conversation_id: number }>(
    (p) => `/api/conversations/${p.conversation_id}/model`,
    { silentStatuses: [404] }
  ),
  setModel: httpPut<void, { conversation_id: number; model_id: string }>(
    (p) => `/api/conversations/${p.conversation_id}/model`,
    (p) => ({ model_id: p.model_id })
  ),
};

// ---------------------------------------------------------------------------
// MCP Service — routed to /api/mcp/*
// ---------------------------------------------------------------------------

export const mcpService = {
  listServers: httpGet<IMcpServer[], void>('/api/mcp/servers'),
  createServer: httpPost<
    IMcpServer,
    Pick<IMcpServer, 'name' | 'description' | 'transport' | 'original_json' | 'builtin'>
  >('/api/mcp/servers'),
  importServers: httpPost<
    IMcpServer[],
    { servers: Array<Pick<IMcpServer, 'name' | 'description' | 'transport' | 'original_json' | 'builtin'>> }
  >('/api/mcp/servers/import'),
  updateServer: httpPut<
    IMcpServer,
    {
      id: number;
      data: Partial<Pick<IMcpServer, 'name' | 'description' | 'transport' | 'original_json' | 'builtin'>>;
    }
  >(
    (p) => `/api/mcp/servers/${p.id}`,
    (p) => p.data
  ),
  deleteServer: httpDelete<void, { id: number }>((p) => `/api/mcp/servers/${p.id}`),
  toggleServer: httpPost<IMcpServer, { id: number }>(
    (p) => `/api/mcp/servers/${p.id}/toggle`,
    () => undefined
  ),
  batchImportServers: httpPost<
    IMcpServer[],
    { servers: Array<Partial<IMcpServer> & Pick<IMcpServer, 'name' | 'transport'>> }
  >('/api/mcp/servers/import'),
  getAgentMcpConfigs: httpGet<
    Array<{
      source: string;
      servers: Array<
        IMcpServer & {
          importable: boolean;
          import_skip_reason?: string;
        }
      >;
    }>,
    Array<{ agent_type: string; backend?: string; name: string; cli_path?: string }>
  >('/api/mcp/agent-configs'),
  testMcpConnection: httpPost<
    {
      success: boolean;
      tools?: Array<{
        name: string;
        description?: string;
        input_schema?: unknown;
        _meta?: Record<string, unknown>;
      }>;
      error?: string;
      code?: string;
      details?: unknown;
      needsAuth?: boolean;
      needs_auth?: boolean;
      authMethod?: 'oauth' | 'basic';
      auth_method?: 'oauth' | 'basic';
      wwwAuthenticate?: string;
      www_authenticate?: string;
    },
    IMcpServer
  >('/api/mcp/test-connection'),
  checkOAuthStatus: httpPost<{ authenticated: boolean }, { server_url: string }>('/api/mcp/oauth/check-status'),
  loginMcpOAuth: httpPost<{ success: boolean; error?: string }, { server_url: string }>('/api/mcp/oauth/login'),
  logoutMcpOAuth: httpPost<void, { server_url: string }>('/api/mcp/oauth/logout'),
  getAuthenticatedServers: httpGet<string[], void>('/api/mcp/oauth/authenticated'),
};

export const openclawConversation = {
  sendMessage: conversation.sendMessage,
  responseStream: conversation.responseStream,
  getRuntime: httpGet<
    {
      conversation_id: number;
      runtime: {
        workspace?: string;
        backend?: string;
        agent_name?: string;
        cli_path?: string;
        model?: string;
        session_key?: string | null;
        is_connected?: boolean;
        has_active_session?: boolean;
        identity_hash?: string | null;
      };
      expected?: {
        expected_workspace?: string;
        expected_backend?: string;
        expected_agent_name?: string;
        expected_cli_path?: string;
        expected_model?: string;
        expected_identity_hash?: string | null;
        switched_at?: number;
      };
    },
    { conversation_id: number }
  >((p) => `/api/conversations/${p.conversation_id}/openclaw/runtime`),
};

// ---------------------------------------------------------------------------
// Remote Agent — routed to /api/remote-agents/*
// ---------------------------------------------------------------------------

export const remoteAgent = {
  list: httpGet<import('@/common/types/agent/remoteAgentTypes').RemoteAgentConfig[], void>('/api/remote-agents'),
  get: httpGet<import('@/common/types/agent/remoteAgentTypes').RemoteAgentConfig | null, { id: number }>(
    (p) => `/api/remote-agents/${p.id}`
  ),
  create: httpPost<
    import('@/common/types/agent/remoteAgentTypes').RemoteAgentConfig,
    import('@/common/types/agent/remoteAgentTypes').RemoteAgentInput
  >('/api/remote-agents'),
  update: httpPut<
    boolean,
    { id: number; updates: Partial<import('@/common/types/agent/remoteAgentTypes').RemoteAgentInput> }
  >(
    (p) => `/api/remote-agents/${p.id}`,
    (p) => p.updates
  ),
  delete: httpDelete<boolean, { id: number }>((p) => `/api/remote-agents/${p.id}`),
  testConnection: httpPost<
    { success: boolean; error?: string },
    { url: string; auth_type: string; auth_token?: string; allow_insecure?: boolean }
  >('/api/remote-agents/test-connection'),
  handshake: httpPost<{ status: 'ok' | 'pending_approval' | 'error'; error?: string }, { id: number }>(
    (p) => `/api/remote-agents/${p.id}/handshake`
  ),
};

// ---------------------------------------------------------------------------
// Database — routed to conversation/message endpoints
// ---------------------------------------------------------------------------

export type PaginatedResult<T> = {
  items: T[];
  total: number;
  has_more: boolean;
};

export const database = {
  getConversationMessages: httpGet<
    PaginatedResult<import('@/common/chat/chatLib').TMessage>,
    {
      conversation_id: number;
      page?: number;
      page_size?: number;
      order?: string;
      content_mode?: 'compact' | 'full';
      // Keyset cursor for incremental history loading: '' = newest window,
      // '<created_at>:<id>' = the page strictly older than that message. When
      // set (incl. ''), the backend ignores page/offset pagination.
      cursor?: string;
    }
  >((p) => {
    const params = new URLSearchParams();
    params.set('page', String(p.page ?? 1));
    params.set('page_size', String(p.page_size ?? 50));
    if (p.order) params.set('order', p.order);
    if (p.content_mode) params.set('content_mode', p.content_mode);
    // Send even an empty cursor (the "newest window" request) — distinct from
    // omitting it, which selects the legacy offset path.
    if (p.cursor !== undefined) params.set('cursor', p.cursor);
    return `/api/conversations/${p.conversation_id}/messages?${params.toString()}`;
  }),
  getConversationMessage: httpGet<
    import('@/common/chat/chatLib').TMessage,
    { conversation_id: number; message_id: string }
  >((p) => `/api/conversations/${p.conversation_id}/messages/${encodeURIComponent(p.message_id)}`),
  getUserConversations: withResponseMap(
    httpGet<PaginatedResult<import('@/common/config/storage').TChatConversation>, { cursor?: string; limit?: number }>(
      (p) => {
        const params = new URLSearchParams();
        if (p.cursor) params.set('cursor', p.cursor);
        if (p.limit) params.set('limit', String(p.limit));
        const qs = params.toString();
        return `/api/conversations${qs ? `?${qs}` : ''}`;
      }
    ),
    fromApiPaginatedConversations
  ),
  searchConversationMessages: withResponseMap(
    httpGet<PaginatedResult<ApiMessageSearchItem>, { keyword: string; page?: number; page_size?: number }>(
      (p) =>
        `/api/messages/search?keyword=${encodeURIComponent(p.keyword)}&page=${p.page ?? 1}&page_size=${p.page_size ?? 50}`
    ),
    fromApiSearchResult
  ),
};

// ---------------------------------------------------------------------------
// Preview History — routed to /api/preview-history/*
// ---------------------------------------------------------------------------

function mapPreviewTarget(target: PreviewHistoryTarget): Record<string, unknown> {
  return { ...target, content_type: target.contentType, contentType: undefined };
}

export const previewHistory = {
  list: httpPost<PreviewSnapshotInfo[], { target: PreviewHistoryTarget }>('/api/preview-history/list', (p) => ({
    target: mapPreviewTarget(p.target),
  })),
  save: httpPost<PreviewSnapshotInfo, { target: PreviewHistoryTarget; content: string }>(
    '/api/preview-history/save',
    (p) => ({ target: mapPreviewTarget(p.target), content: p.content })
  ),
  getContent: httpPost<
    { snapshot: PreviewSnapshotInfo; content: string } | null,
    { target: PreviewHistoryTarget; snapshot_id: string }
  >('/api/preview-history/get-content', (p) => ({ target: mapPreviewTarget(p.target), snapshot_id: p.snapshot_id })),
};

// Preview panel
export const preview = {
  open: wsEmitter<{
    content: string;
    content_type: import('../types/office/preview').PreviewContentType;
    metadata?: {
      title?: string;
      file_name?: string;
    };
  }>('preview.open'),
};

// ---------------------------------------------------------------------------
// Document conversion
// ---------------------------------------------------------------------------

export const document = {
  convert: httpPost<
    import('../types/office/conversion').DocumentConversionResponse,
    import('../types/office/conversion').DocumentConversionRequest
  >('/api/document/convert'),
};

// ---------------------------------------------------------------------------
// Office Previews — routed to /api/*-preview/*
// ---------------------------------------------------------------------------

export const pptPreview = {
  start: httpPost<{ url: string; error?: string }, { file_path: string; workspace?: string }>('/api/ppt-preview/start'),
  stop: httpPost<void, { file_path: string }>('/api/ppt-preview/stop'),
  status: wsEmitter<{ state: 'starting' | 'installing' | 'ready' | 'error'; message?: string }>('ppt-preview.status'),
};

export const wordPreview = {
  start: httpPost<{ url: string; error?: string }, { file_path: string; workspace?: string }>(
    '/api/word-preview/start'
  ),
  stop: httpPost<void, { file_path: string }>('/api/word-preview/stop'),
  status: wsEmitter<{ state: 'starting' | 'installing' | 'ready' | 'error'; message?: string }>('word-preview.status'),
};

export const excelPreview = {
  start: httpPost<{ url: string; error?: string }, { file_path: string; workspace?: string }>(
    '/api/excel-preview/start'
  ),
  stop: httpPost<void, { file_path: string }>('/api/excel-preview/stop'),
  status: wsEmitter<{ state: 'starting' | 'installing' | 'ready' | 'error'; message?: string }>('excel-preview.status'),
};

// ---------------------------------------------------------------------------
// Deep Link — stays IPC (Electron protocol handler)
// ---------------------------------------------------------------------------

export const deepLink = {
  received: shellEmitter<{ action: string; params: Record<string, string> }>((cb) => subscribeDeepLink(cb)),
};

// ---------------------------------------------------------------------------
// Window Controls — stays IPC (Electron-native)
// ---------------------------------------------------------------------------

export const windowControls = {
  minimize: shellProvider<void, void>(() => tauriWindowMinimize(), undefined),
  maximize: shellProvider<void, void>(() => tauriWindowMaximize(), undefined),
  unmaximize: shellProvider<void, void>(() => tauriWindowUnmaximize(), undefined),
  // Double-click-titlebar entry: a single native toggle (avoids a race between a
  // separate isMaximized read and maximize/unmaximize). Windows/Linux only — on
  // macOS the OS handles titlebar double-click on the native chrome.
  toggleMaximize: shellProvider<void, void>(() => tauriWindowToggleMaximize(), undefined),
  close: shellProvider<void, void>(() => tauriWindowClose(), undefined),
  isMaximized: shellProvider<boolean, void>(() => tauriWindowIsMaximized(), false),
  maximizedChanged: shellEmitter<{ is_maximized: boolean }>((cb) => subscribeWindowMaximized(cb)),
};

// ---------------------------------------------------------------------------
// System Settings — routed to /api/settings/* unless they need Electron-native side effects.
// ---------------------------------------------------------------------------

export const systemSettings = {
  getNotificationEnabled: httpGet<boolean, void>('/api/settings/client?key=notificationEnabled'),
  setNotificationEnabled: httpPut<void, { enabled: boolean }>('/api/settings/client', (p) => ({
    notificationEnabled: p.enabled,
  })),
  getCronNotificationEnabled: httpGet<boolean, void>('/api/settings/client?key=cronNotificationEnabled'),
  setCronNotificationEnabled: httpPut<void, { enabled: boolean }>('/api/settings/client', (p) => ({
    cronNotificationEnabled: p.enabled,
  })),
  getKeepAwake: httpGet<boolean, void>('/api/settings/client?key=keepAwake'),
  setKeepAwake: httpPut<void, { enabled: boolean }>('/api/settings/client', (p) => ({ keepAwake: p.enabled })),
  changeLanguage: httpPatch<void, { language: string }>('/api/settings', (p) => ({ language: p.language })),
  languageChanged: wsEmitter<{ language: string }>('system-settings:language-changed'),
  getSaveUploadToWorkspace: httpGet<boolean, void>('/api/settings/client?key=saveUploadToWorkspace'),
  setSaveUploadToWorkspace: httpPut<void, { enabled: boolean }>('/api/settings/client', (p) => ({
    saveUploadToWorkspace: p.enabled,
  })),
  getAutoPreviewOfficeFiles: httpGet<boolean, void>('/api/settings/client?key=autoPreviewOfficeFiles'),
  setAutoPreviewOfficeFiles: httpPut<void, { enabled: boolean }>('/api/settings/client', (p) => ({
    autoPreviewOfficeFiles: p.enabled,
  })),
};

// ---------------------------------------------------------------------------
// Computer-use OS permissions — macOS TCC (Accessibility / Screen Recording).
// Routed to the in-process backend, which probes/triggers the HOST process's
// OWN grants, so `get` is the authoritative answer to "did my grant take effect
// for the running app?" — a visibly-on System Settings toggle bound to a stale
// code identity reports `false` here. Off macOS the booleans are null.
// ---------------------------------------------------------------------------

export type ComputerPermissionKind = 'accessibility' | 'screen_recording';

export interface ComputerPermissionStatus {
  accessibility: boolean | null;
  screen_recording: boolean | null;
  platform: 'macos' | 'windows' | 'linux' | 'other';
  app_label: string;
}

export const computerPermissions = {
  /** Live grant state for the running host process (safe to poll). */
  get: httpGet<ComputerPermissionStatus, void>('/api/computer/permissions'),
  /** Trigger the macOS prompt + register the app in the list; returns post-call status. */
  request: httpPost<ComputerPermissionStatus, { kind: ComputerPermissionKind }>(
    '/api/computer/permissions/request'
  ),
  /** Deep-link to the exact System Settings privacy pane for `kind`. */
  openSettings: httpPost<void, { kind: ComputerPermissionKind }>('/api/computer/permissions/open-settings'),
};

// ---------------------------------------------------------------------------
// System events — global WS broadcasts owned by the backend
// ---------------------------------------------------------------------------

// (browser-automation runtime provisioning was removed with the native CDP
// engine — the self-contained engine acquires Chrome on demand, so there is no
// longer a `system.provisioning` broadcast to surface.)

// ---------------------------------------------------------------------------
// Notification — stays IPC (Electron-native Notification API)
// ---------------------------------------------------------------------------

export type INotificationOptions = {
  title: string;
  body: string;
  icon?: string;
  conversation_id?: number;
};

export const notification = {
  show: shellProvider<void, INotificationOptions>(
    (opts) => tauriSendNotification({ title: opts.title, body: opts.body, icon: opts.icon }),
    undefined
  ),
  // DEGRADE_STUB: click→navigate needs a Rust notification-action listener that
  // emits a Tauri event (see electron-removal-plan C2); inert until then.
  clicked: noopEmitter<{ conversation_id?: number }>(),
};

// ---------------------------------------------------------------------------
// Task management — stubbed (internal process management)
// ---------------------------------------------------------------------------

export const task = {
  stopAll: stubProvider<{ success: boolean; count: number }, void>('task.stopAll', { success: true, count: 0 }),
  getRunningCount: stubProvider<{ success: boolean; count: number }, void>('task.getRunningCount', {
    success: true,
    count: 0,
  }),
};

// ---------------------------------------------------------------------------
// WebUI — mix: start/stop/getStatus/statusChanged stay IPC (Electron-only
// lifecycle owned by the main process, can't run in backend); credential
// operations route to backend /api/webui/* under local-mode.
// ---------------------------------------------------------------------------

export interface IWebUIStatus {
  running: boolean;
  port: number;
  allowRemote: boolean;
  localUrl: string;
  networkUrl?: string;
  /** A quick-access URL per non-loopback NIC (routing-preferred first). */
  networkUrls?: string[];
  lanIP?: string;
  adminUsername: string;
  /** Whether a real admin password is stored (non-empty hash). Lets the UI
   *  distinguish "credential set (hidden)" from "never provisioned" even when
   *  the LAN server is stopped, so a persisted password is not read as lost. */
  passwordSet?: boolean;
  initialPassword?: string;
  /** Set when a start attempt failed (e.g. could not bind the port). */
  error?: string;
}

export const webui = {
  /**
   * Capability bit: can this runtime start/stop the LAN listener and report its
   * real status? True in the Tauri desktop shell (the embedded backend owns the
   * LAN-listener lifecycle via the `webui_*` commands). False in a WebUI
   * browser — that page IS served by the LAN listener, so it cannot control it.
   */
  lifecycleSupported: typeof window !== 'undefined' && Boolean((window as { __backendPort?: number }).__backendPort),
  getStatus: shellProvider<IWebUIStatus, void>(() => tauriWebuiGetStatus<IWebUIStatus>(), {
    running: false,
    port: 0,
    allowRemote: false,
    localUrl: '',
    adminUsername: '',
  }),
  // Enabling binds the LAN listener (0.0.0.0); the backend returns the full
  // status (running + error + lanIP + one-time initialPassword).
  start: shellProvider<IWebUIStatus, void>(() => tauriWebuiStart<IWebUIStatus>(), {
    running: false,
    port: 0,
    allowRemote: false,
    localUrl: '',
    adminUsername: '',
    error: 'desktop lifecycle unavailable',
  }),
  stop: shellProvider<void, void>(() => tauriWebuiStop<void>(), undefined),
  statusChanged: shellEmitter<{
    running: boolean;
    port?: number;
    localUrl?: string;
    networkUrl?: string;
    networkUrls?: string[];
    lanIP?: string;
    adminUsername?: string;
    passwordSet?: boolean;
    initialPassword?: string;
  }>((cb) => subscribeWebuiStatus(cb)),
  changePassword: httpPost<void, { newPassword: string }>('/api/webui/change-password', (p) => ({
    new_password: p.newPassword,
  })),
  changeUsername: httpPost<{ username: string }, { newUsername: string }>('/api/webui/change-username', (p) => ({
    new_username: p.newUsername,
  })),
  resetPassword: httpPost<{ new_password: string }, void>('/api/webui/reset-password'),
  generateQRToken: httpPost<{ token: string; expires_at_ms: number }, void>('/api/webui/generate-qr-token'),
  /**
   * Per-companion Remote access tokens (local-trust-gated; desktop shell only).
   * Mint returns the plaintext exactly ONCE (`token`) — it is never persisted nor
   * re-emitted; the backend stores only a hash. `warning` is present when the
   * companion has no resolvable model (the token still mints, but model-dependent
   * capabilities like `nomi_agent_run` will fail until a provider/model is set).
   */
  companionAccessToken: {
    status: httpGet<{ configured: boolean }, { companionId: string }>(
      (p) => `/api/webui/companions/${encodeURIComponent(p.companionId)}/access-token`
    ),
    mint: httpPost<{ token: string; companion_id: string; warning?: string }, { companionId: string }>(
      (p) => `/api/webui/companions/${encodeURIComponent(p.companionId)}/access-token`,
      () => undefined
    ),
    revoke: httpDelete<{ configured: boolean }, { companionId: string }>(
      (p) => `/api/webui/companions/${encodeURIComponent(p.companionId)}/access-token`
    ),
  },
};

// ---------------------------------------------------------------------------
// Cron — routed to /api/cron/*
// ---------------------------------------------------------------------------

export const cron = {
  listJobs: httpGet<ICronJob[], void>('/api/cron/jobs'),
  listJobsByConversation: httpGet<ICronJob[], { conversation_id: number }>(
    (p) => `/api/cron/jobs?conversation_id=${encodeURIComponent(p.conversation_id)}`
  ),
  getJob: httpGet<ICronJob | null, { job_id: string }>((p) => `/api/cron/jobs/${p.job_id}`),
  addJob: httpPost<ICronJob, ICreateCronJobParams>('/api/cron/jobs'),
  updateJob: httpPut<ICronJob, { job_id: string; updates: Partial<ICronJob> }>(
    (p) => `/api/cron/jobs/${p.job_id}`,
    (p) => ({
      name: p.updates.name,
      description: p.updates.description,
      enabled: p.updates.enabled,
      schedule: p.updates.schedule,
      message: p.updates.target?.payload.text,
      execution_mode: p.updates.target?.execution_mode,
      agent_config: p.updates.metadata?.agent_config,
      conversation_title: p.updates.metadata?.conversation_title,
      max_retries: p.updates.state?.max_retries,
      target_kind: p.updates.target?.target_kind,
    })
  ),
  removeJob: httpDelete<void, { job_id: string }>((p) => `/api/cron/jobs/${p.job_id}`),
  runNow: httpPost<{ conversation_id: number }, { job_id: string }>((p) => `/api/cron/jobs/${p.job_id}/run`),
  listRuns: httpGet<ICronJobRun[], { job_id: string }>((p) => `/api/cron/jobs/${p.job_id}/runs`),
  saveSkill: httpPost<void, { job_id: string; content: string }>(
    (p) => `/api/cron/jobs/${p.job_id}/skill`,
    (p) => ({ content: p.content })
  ),
  hasSkill: withResponseMap(
    httpGet<{ has_skill: boolean }, { job_id: string }>((p) => `/api/cron/jobs/${p.job_id}/skill`),
    (data) => Boolean(data?.has_skill)
  ),
  deleteSkill: httpDelete<void, { job_id: string }>((p) => `/api/cron/jobs/${p.job_id}/skill`),
  onJobCreated: wsEmitter<ICronJob>('cron.job-created'),
  onJobUpdated: wsEmitter<ICronJob>('cron.job-updated'),
  onJobRemoved: wsEmitter<{ job_id: string }>('cron.job-removed'),
  onJobExecuted: wsEmitter<{ job_id: string; status: 'ok' | 'error' | 'skipped' | 'missed'; error?: string }>(
    'cron.job-executed'
  ),
};

// ---------------------------------------------------------------------------
// Cron types (re-exported for consumers)
// ---------------------------------------------------------------------------

export type ICronSchedule =
  | { kind: 'at'; at_ms: number; description: string }
  | { kind: 'every'; every_ms: number; description: string }
  | { kind: 'cron'; expr: string; tz?: string; description: string };

export type ICronTargetKind = 'agent';

export type ICronJobRunStatus = 'ok' | 'error' | 'skipped' | 'missed';

export interface ICronJob {
  id: string;
  name: string;
  description?: string;
  enabled: boolean;
  schedule: ICronSchedule;
  target: {
    payload: { kind: 'message'; text: string };
    execution_mode?: 'existing' | 'new_conversation';
    target_kind?: ICronTargetKind;
  };
  metadata: {
    conversation_id: number;
    conversation_title?: string;
    agent_type: string;
    created_by: 'user' | 'agent';
    created_at: number;
    updated_at: number;
    agent_config?: ICronAgentConfig;
  };
  state: {
    next_run_at_ms?: number;
    last_run_at_ms?: number;
    last_status?: ICronJobRunStatus;
    last_error?: string;
    run_count: number;
    retry_count: number;
    max_retries: number;
  };
}

export interface ICronJobRun {
  id: string;
  job_id: string;
  executed_at_ms: number;
  status: ICronJobRunStatus;
}

export interface ICronAgentConfig {
  backend: string;
  name: string;
  cli_path?: string;
  is_preset?: boolean;
  custom_agent_id?: string;
  preset_agent_type?: string;
  mode?: string;
  model_id?: string;
  config_options?: Record<string, string>;
  workspace?: string;
  /** Clear the agent context before each scheduled run (existing-conversation jobs only). */
  clear_context_each_run?: boolean;
}

export interface ICreateCronJobParams {
  name: string;
  description?: string;
  schedule: ICronSchedule;
  prompt?: string;
  message?: string;
  conversation_id: number;
  conversation_title?: string;
  agent_type: string;
  created_by: 'user' | 'agent';
  execution_mode?: 'existing' | 'new_conversation';
  agent_config?: ICronAgentConfig;
  target_kind?: ICronTargetKind;
}

// ---------------------------------------------------------------------------
// Terminal — routed to /api/terminals/*
// ---------------------------------------------------------------------------

export interface ITerminalSession {
  /** Terminal session primary key — backend-minted INTEGER (numeric-id spec),
   *  rendered as `#N`. */
  id: number;
  name: string;
  cwd: string;
  /** 派生字段（不落库）：cwd 等于或位于默认工作路径之下 / Derived: cwd equals or sits under the backend default work dir. */
  is_default_workpath?: boolean;
  command: string;
  args: string[];
  backend?: string;
  mode?: string;
  cols: number;
  rows: number;
  created_at: number;
  updated_at: number;
  last_status: 'running' | 'exited' | 'error';
  exit_code?: number;
  pinned?: boolean;
  pinned_at?: number;
  /** Base64 scrollback snapshot — present only on single-session GET. */
  scrollback_b64?: string;
}

export interface ICreateTerminalParams {
  name?: string;
  cwd: string;
  command: string;
  args?: string[];
  env?: Record<string, string>;
  backend?: string;
  mode?: string;
  cols?: number;
  rows?: number;
  /** 推迟到首个 resize(携带真实尺寸)再 spawn PTY,使全屏 TUI(claude)首帧即按正确尺寸绘制,避免「进入即花屏、需手动调尺寸」 / Defer the PTY spawn until the first resize carries the real size. */
  defer_spawn?: boolean;
  /** 创建即绑定的知识库 id；启动时挂载到 {cwd}/.nomi/knowledge/ / Knowledge bases bound at creation, mounted before the PTY spawns. */
  knowledge_base_ids?: string[];
}

/** Response shape for GET /api/terminals/mcp-register-template. */
export interface IMcpRegisterTemplate {
  claude_cmd: string;
  claude_json: string;
  codex_toml: string;
  gemini_json: string;
}

/** Response shape for POST /api/terminals/register-knowledge. */
export interface IRegisterKnowledgeOutcome {
  written_path: string;
  scope: string;
  note?: string;
}

/** Request body for POST /api/terminals/register-knowledge. */
export interface IRegisterKnowledgeParams {
  cwd: string;
  family: string;
}

export const terminal = {
  list: httpGet<ITerminalSession[], void>('/api/terminals'),
  get: httpGet<ITerminalSession, { id: number }>((p) => `/api/terminals/${p.id}`),
  create: httpPost<ITerminalSession, ICreateTerminalParams>('/api/terminals'),
  mcpRegisterTemplate: httpGet<IMcpRegisterTemplate, void>('/api/terminals/mcp-register-template'),
  registerKnowledge: httpPost<IRegisterKnowledgeOutcome, IRegisterKnowledgeParams>('/api/terminals/register-knowledge'),
  input: httpPost<void, { id: number; data_b64: string }>(
    (p) => `/api/terminals/${p.id}/input`,
    (p) => ({ data_b64: p.data_b64 })
  ),
  resize: httpPost<void, { id: number; cols: number; rows: number }>(
    (p) => `/api/terminals/${p.id}/resize`,
    (p) => ({ cols: p.cols, rows: p.rows })
  ),
  kill: httpPost<void, { id: number }>((p) => `/api/terminals/${p.id}/kill`),
  relaunch: httpPost<ITerminalSession, { id: number }>((p) => `/api/terminals/${p.id}/relaunch`),
  /** 把会话原地回退为干净的登录 shell(杀掉卡死的 claude/codex 并以 $SHELL 重启同一会话) / Fall back to a clean login shell in place. */
  relaunchShell: httpPost<ITerminalSession, { id: number }>((p) => `/api/terminals/${p.id}/relaunch-shell`),
  update: httpPatch<ITerminalSession, { id: number; name?: string; pinned?: boolean }>(
    (p) => `/api/terminals/${p.id}`,
    (p) => ({ name: p.name, pinned: p.pinned })
  ),
  remove: httpDelete<void, { id: number }>((p) => `/api/terminals/${p.id}`),
  onOutput: wsEmitter<{ id: number; data_b64: string }>('terminal.output'),
  onExit: wsEmitter<{ id: number; exit_code?: number }>('terminal.exit'),
  onCreated: wsEmitter<ITerminalSession>('terminal.created'),
  onUpdated: wsEmitter<ITerminalSession>('terminal.updated'),
  onRemoved: wsEmitter<{ id: number }>('terminal.removed'),
  /** 在 WebSocket 断线重连成功后触发(本地合成事件,非服务端推送)。XtermView 借此 reset
   *  并重放 scrollback,修复断线期间丢失的重绘帧造成的乱码 / Fires after the WS reconnects
   *  (local synthetic event) so a view can reset + replay the scrollback it missed. */
  onReconnected: wsEmitter<undefined>('ws.reconnected'),
  // Uses httpRequest directly (instead of httpGet + withResponseMap) because the
  // response mapper needs `cwd` from params to build fullPath/relativePath, and
  // withResponseMap's map function does not receive the original params. Treats
  // `cwd` as the workspace root — same {name,type}[] wire shape as the
  // conversation workspace endpoint, so the workspace mapper is reused as-is.
  getWorkspace: {
    provider: () => {},
    invoke: (async (p: { terminal_id: number; cwd: string; path: string; search?: string }) => {
      const rel = absoluteToRelativePath(p.path, p.cwd);
      const url = `/api/terminals/${p.terminal_id}/workspace?path=${encodeURIComponent(rel)}${p.search ? `&search=${encodeURIComponent(p.search)}` : ''}`;
      const raw = await httpRequest<Array<{ name: string; type: string }>>('GET', url);
      return fromBackendWorkspaceList(raw, p.cwd, rel);
    }) as (p: { terminal_id: number; cwd: string; path: string; search?: string }) => Promise<IDirOrFile[]>,
  },
};

// ---------------------------------------------------------------------------
// Shared types (re-exported for consumers)
// ---------------------------------------------------------------------------

interface ISendMessageParams {
  input: string;
  conversation_id: number;
  files?: string[];
  loading_id?: string;
  inject_skills?: string[];
}

// Server-assigned identifier for the newly created user message. Clients must
// use this as the canonical msg_id when rendering an optimistic bubble so the
// local state aligns with DB rows and WebSocket stream events.
export interface ISendMessageResult {
  msg_id: string;
}

export interface IConfirmMessageParams {
  confirm_key: string;
  msg_id: string;
  conversation_id: number;
  call_id: string;
}

export interface ICreateConversationParams {
  type: 'acp' | 'codex' | 'openclaw-gateway' | 'nanobot' | 'remote' | 'nomi';
  name?: string;
  model: TProviderWithModel;
  extra: {
    workspace?: string;
    custom_workspace?: boolean;
    default_files?: string[];
    backend?: string;
    cli_path?: string;
    gateway?: {
      host?: string;
      port?: number;
      token?: string;
      password?: string;
      use_external_gateway?: boolean;
      cli_path?: string;
    };
    web_search_engine?: 'google' | 'default';
    agent_name?: string;
    agent_id?: string;
    custom_agent_id?: string;
    context?: string;
    context_file_name?: string;
    preset_rules?: string;
    /** Legacy field kept for backward-compat only (`#[serde(default)]`); no longer
     *  drives any lead prompt (the homepage 智能编排 entry was removed). The model
     *  range (主/协作) is now set per-conversation via the composer's collaborator
     *  selector into `extra.orchestrator_model_range`. */
    orchestrator_role?: string;
    /** Curated model range for the orchestration run this lead conversation
     *  spawns (homepage「主模型 + 协作模型」picker). `models[0]` = 主模型 (also the
     *  lead/planner); the rest = 协作模型. Read back by the `nomi_run_create`
     *  gateway handler from the conversation's extra (deterministic, not via the
     *  LLM). Absent ⇒ Auto (every enabled model). */
    orchestrator_model_range?: TModelRange;
    /** 「agent 集群」意图标记（需求1）：composer 顶部 toggle 选中后落此键。后端
     *  nomi 工厂据此在常驻 subagent 提示之上追加 CLUSTER_MODE_HINT（对每个任务
     *  刻意评估是否开集群、太简单先向用户说明原因）。 */
    agent_cluster_mode?: boolean;
    /** 节点级审批模式（需求5，迁移 030）：'manual' = 集群节点遇关键决策挂起向
     *  用户提问；'auto'/缺省 = 全授权。建 run 时由网关从会话 extra 读取生效。 */
    orchestrator_approval_mode?: string;
    /** Transient: preset opt-in skills. Consumed by backend create handler
     *  and stripped before persistence. */
    preset_enabled_skills?: string[];
    /** Transient: auto-inject skills the user opted out of on the Guid page.
     *  Consumed by backend create handler and stripped before persistence. */
    exclude_auto_inject_skills?: string[];
    /** Transient: MCP server ids selected on the Guid page. Consumed by the
     *  backend create handler and snapshotted into conversation.extra. */
    selected_mcp_server_ids?: number[];
    /** Transient: session-scoped MCP server configs that are not stored in the
     *  backend catalog (currently built-in MCP servers). */
    selected_session_mcp_servers?: ISessionMcpServer[];
    preset_context?: string;
    preset_assistant_id?: string;
    session_mode?: string;
    codex_model?: string;
    current_model_id?: string;
    cached_config_options?: import('../types/platform/acpTypes').AcpSessionConfigOption[];
    pending_config_options?: Record<string, string>;
    runtime_validation?: {
      expected_workspace?: string;
      expected_backend?: string;
      expected_agent_name?: string;
      expected_cli_path?: string;
      expected_model?: string;
      expected_identity_hash?: string | null;
      switched_at?: number;
    };
    /** Legacy marker for pre-provider-probe health-check conversations. */
    is_health_check?: boolean;
    remote_agent_id?: number;
    extra_skill_paths?: string[];
    team_id?: string;
  };
}

interface IResetConversationParams {
  id?: number;
}

export interface IDirOrFile {
  name: string;
  fullPath: string;
  relativePath: string;
  isDir: boolean;
  isFile: boolean;
  children?: Array<IDirOrFile>;
}

export interface IFileMetadata {
  name: string;
  path: string;
  size: number;
  type: string;
  lastModified: number;
  isDirectory?: boolean;
}

export type IWorkspaceFlatFile = {
  name: string;
  fullPath: string;
  relativePath: string;
};

export interface IResponseMessage {
  type: string;
  data: unknown;
  /** messages.id stays TEXT (`msg_…`). */
  msg_id: string;
  /** Owning conversation id — INTEGER (numeric-id spec). */
  conversation_id: number;
  created_at?: number;
  hidden?: boolean;
  /** Replace accumulated text for the same msg_id instead of appending. */
  replace?: boolean;
  /** Companion wire markers (backend StreamRelay stamps them on every
   *  fragment): true + owning companion id when the conversation is a companion
   *  companion / channel master session. */
  companion?: boolean;
  companion_id?: string | null;
  /** IM platform ("telegram" | "lark" | ...) when the conversation is a
   *  channel master session; null/absent for local conversations. */
  channel_platform?: string | null;
  /** Originating subsystem of the turn's user message (companion/cron/autowork/
   *  idmm); null/absent = typed by a real person. */
  origin?: string | null;
}

/** `message.userCreated` broadcast: a user message was persisted (covers IM
 *  channel inbound messages — the companion window renders those as incoming
 *  bubble headers). Same companion wire markers as IResponseMessage. */
export interface IUserMessageCreatedEvent {
  conversation_id: number;
  msg_id: string;
  content: string;
  position: 'right';
  status: string;
  hidden?: boolean;
  origin?: string | null;
  companion?: boolean;
  companion_id?: string | null;
  channel_platform?: string | null;
  created_at: number;
}

export type IConversationArtifactKind = 'cron_trigger' | 'skill_suggest';
export type IConversationArtifactStatus = 'active' | 'pending' | 'dismissed' | 'saved';

export interface IConversationArtifactBase<
  Kind extends IConversationArtifactKind,
  Payload extends Record<string, unknown>,
> {
  id: number;
  /** Owning conversation id — INTEGER (numeric-id spec). */
  conversation_id: number;
  /** cron_jobs.id stays TEXT (`cron_…`). */
  cron_job_id?: string;
  kind: Kind;
  status: IConversationArtifactStatus;
  payload: Payload;
  created_at: number;
  updated_at: number;
}

export type ICronTriggerArtifact = IConversationArtifactBase<
  'cron_trigger',
  {
    cron_job_id: string;
    cron_job_name: string;
    triggered_at: number;
  }
>;

export type ISkillSuggestArtifact = IConversationArtifactBase<
  'skill_suggest',
  {
    cron_job_id: string;
    name: string;
    description: string;
    skillContent?: string;
    skill_content?: string;
  }
>;

export type IConversationArtifact = ICronTriggerArtifact | ISkillSuggestArtifact;

export interface IConversationTurnStartedEvent {
  /** Conversation id — INTEGER (numeric-id spec). Named `session_id` on the wire. */
  session_id: number;
  conversation_id: number;
  turn_id?: string;
  status: 'pending' | 'running' | 'finished';
  phase?: 'starting' | 'thinking' | 'streaming' | 'tooling' | 'waiting_permission' | string;
  state:
    | 'ai_generating'
    | 'ai_waiting_input'
    | 'ai_waiting_confirmation'
    | 'initializing'
    | 'stopped'
    | 'error'
    | 'unknown'
    | string;
  detail: string;
  can_send_message: boolean;
  runtime: {
    state: 'idle' | 'starting' | 'running' | 'waiting_confirmation';
    can_send_message: boolean;
    has_task: boolean;
    task_status?: 'pending' | 'running' | 'finished';
    is_processing: boolean;
    pending_confirmations: number;
    processing_started_at?: number;
  };
  companion?: boolean;
  companion_id?: string | null;
  origin?: string | null;
  channel_platform?: string | null;
}

export interface IConversationTurnCompletedEvent {
  /** Conversation id — INTEGER (numeric-id spec). Named `session_id` on the wire. */
  session_id: number;
  status: 'pending' | 'running' | 'finished';
  state:
    | 'ai_generating'
    | 'ai_waiting_input'
    | 'ai_waiting_confirmation'
    | 'initializing'
    | 'stopped'
    | 'error'
    | 'unknown';
  detail: string;
  can_send_message: boolean;
  runtime: {
    state: 'idle' | 'starting' | 'running' | 'waiting_confirmation';
    can_send_message: boolean;
    has_task: boolean;
    task_status?: 'pending' | 'running' | 'finished';
    is_processing: boolean;
    pending_confirmations: number;
    processing_started_at?: number;
  };
  workspace: string;
  model: {
    platform: string;
    name: string;
    use_model: string;
  };
  last_message: {
    id?: string;
    type?: string;
    content: unknown;
    status?: string | null;
    created_at: number;
  };
}

export interface IConversationListChangedEvent {
  conversation_id: number;
  action: 'created' | 'updated' | 'deleted';
  source?: string;
}

export type ConversationSideQuestionResult =
  | { status: 'ok'; answer: string }
  | { status: 'noAnswer' }
  | { status: 'unsupported' }
  | { status: 'invalid'; reason: 'emptyQuestion' }
  | { status: 'toolsRequired' };

interface IBridgeResponse<D = {}> {
  success: boolean;
  data?: D;
  msg?: string;
}

// ---------------------------------------------------------------------------
// Extensions API
// ---------------------------------------------------------------------------

export interface IExtensionInfo {
  name: string;
  display_name: string;
  version: string;
  description?: string;
  source: string;
  enabled: boolean;
}

export interface IExtensionPermissionSummary {
  name: string;
  description: string;
  level: 'safe' | 'moderate' | 'dangerous';
  granted: boolean;
}

export interface IExtensionSettingsTab {
  id: string;
  label: string;
  icon?: string;
  url: string;
  position?: { relativeTo: string; placement: 'before' | 'after' };
  order: number;
  extensionName: string;
}

export interface IExtensionWebuiContribution {
  extensionName: string;
  apiRoutes: Array<{ path: string; auth: boolean }>;
  staticAssets: Array<{ urlPrefix: string; directory: string }>;
}

export type AgentActivityState = 'idle' | 'writing' | 'researching' | 'executing' | 'syncing' | 'error';

export interface IExtensionAgentActivityEvent {
  conversationId: number;
  at: number;
  kind: 'status' | 'tool' | 'message';
  text: string;
}

export interface IExtensionAgentActivityItem {
  id: string;
  backend: string;
  agentName: string;
  state: AgentActivityState;
  runtimeStatus: 'pending' | 'running' | 'finished' | 'unknown';
  conversations: number;
  activeConversations: number;
  lastActiveAt: number;
  lastStatus?: string;
  currentTask?: string;
  recentEvents: IExtensionAgentActivityEvent[];
}

export interface IExtensionAgentActivitySnapshot {
  generatedAt: number;
  totalConversations: number;
  runningConversations: number;
  agents: IExtensionAgentActivityItem[];
}

export const extensions = {
  getThemes: httpGet<ICssTheme[], void>('/api/extensions/themes'),
  getLoadedExtensions: httpGet<IExtensionInfo[], void>('/api/extensions'),
  getAssistants: httpGet<Record<string, unknown>[], void>('/api/extensions/assistants'),
  getAgents: httpGet<Record<string, unknown>[], void>('/api/extensions/agents'),
  getAcpAdapters: httpGet<Record<string, unknown>[], void>('/api/extensions/acp-adapters'),
  getMcpServers: httpGet<Record<string, unknown>[], void>('/api/extensions/mcp-servers'),
  getSkills: httpGet<Array<{ name: string; description: string; location: string }>, void>('/api/extensions/skills'),
  getSettingsTabs: httpGet<IExtensionSettingsTab[], void>('/api/extensions/settings-tabs'),
  getWebuiContributions: httpGet<IExtensionWebuiContribution[], void>('/api/extensions/webui'),
  getAgentActivitySnapshot: httpGet<IExtensionAgentActivitySnapshot, void>('/api/extensions/agent-activity'),
  getExtI18nForLocale: httpPost<Record<string, unknown>, { locale: string }>('/api/extensions/i18n'),
  enableExtension: httpPost<void, { name: string }>('/api/extensions/enable'),
  disableExtension: httpPost<void, { name: string; reason?: string }>('/api/extensions/disable'),
  getPermissions: httpPost<IExtensionPermissionSummary[], { name: string }>('/api/extensions/permissions'),
  getRiskLevel: httpPost<string, { name: string }>('/api/extensions/risk-level'),
  stateChanged: wsEmitter<{ name: string; enabled: boolean; reason?: string }>('extensions.state-changed'),
};

// ---------------------------------------------------------------------------
// Channel API — routed to /api/channel/*
// ---------------------------------------------------------------------------

import type {
  IChannelPairingRequest,
  IChannelPluginStatus,
  IChannelSession,
  IChannelUser,
} from '@/common/types/channel/channel';

type RawPluginStatus = Record<string, unknown>;
type RawPairing = Record<string, unknown>;
type RawUser = Record<string, unknown>;
type RawSession = Record<string, unknown>;

interface IChannelBridgeResponse {
  success: boolean;
  message?: string;
  error?: string;
}

function toPluginStatus(raw: RawPluginStatus): IChannelPluginStatus {
  return {
    id: (raw.plugin_id ?? raw.id) as string,
    type: (raw.type ?? raw.plugin_type) as string,
    name: raw.name as string,
    enabled: raw.enabled as boolean,
    connected: (raw.connected ?? false) as boolean,
    status: raw.status as string | undefined,
    last_connected: raw.last_connected as number | undefined,
    activeUsers: (raw.active_users ?? 0) as number,
    botUsername: raw.bot_username as string | undefined,
    hasToken: (raw.has_token ?? false) as boolean,
    companionId: raw.companion_id as string | undefined,
    publicAgentId: raw.public_agent_id as string | null | undefined,
    botKey: raw.bot_key as string | undefined,
    isExtension: raw.is_extension as boolean | undefined,
    extensionMeta: raw.extension_meta as IChannelPluginStatus['extensionMeta'],
  };
}

function toPairing(raw: RawPairing): IChannelPairingRequest {
  return {
    code: raw.code as string,
    platformUserId: raw.platform_user_id as string,
    platformType: raw.platform_type as string,
    display_name: raw.display_name as string | undefined,
    requestedAt: raw.requested_at as number,
    expiresAt: raw.expires_at as number,
    channelId: raw.channel_id as string | undefined,
  };
}

function toChannelUser(raw: RawUser): IChannelUser {
  return {
    id: raw.id as string,
    platformUserId: raw.platform_user_id as string,
    platformType: raw.platform_type as string,
    display_name: raw.display_name as string | undefined,
    authorizedAt: raw.authorized_at as number,
    lastActive: raw.last_active as number | undefined,
    session_id: raw.session_id as string | undefined,
    channelId: raw.channel_id as string | undefined,
  };
}

function toChannelSession(raw: RawSession): IChannelSession {
  return {
    id: raw.id as string,
    user_id: raw.user_id as string,
    agent_type: raw.agent_type as string,
    conversation_id: raw.conversation_id as string | undefined,
    workspace: raw.workspace as string | undefined,
    chatId: raw.chat_id as string | undefined,
    created_at: raw.created_at as number,
    lastActivity: raw.last_activity as number,
  };
}

export const channel = {
  getPluginStatus: withResponseMap(httpGet<RawPluginStatus[], void>('/api/channel/plugins'), (raw) =>
    raw.map(toPluginStatus)
  ),
  /**
   * 启用/更新机器人渠道。寻址契约（对应后端 EnableChannelSpec）：
   * - `plugin_id` 指向已有渠道行（legacy 调用传平台名）→ 更新该行；
   * - 省略 `plugin_id` 并给 `plugin_type` → 新建一行（每宠多机器人路径）；
   * - `companion_id` 把机器人绑到桌面伙伴，`public_agent_id` 把它绑到对外伙伴
   *   （二者互斥）；同一机器人(bot_key)已绑其他对象时后端 409。
   */
  enablePlugin: httpPost<
    IChannelBridgeResponse,
    { plugin_id?: string; plugin_type?: string; companion_id?: string; public_agent_id?: string; config: Record<string, unknown> }
  >('/api/channel/plugins/enable'),
  disablePlugin: httpPost<void, { plugin_id: string }>('/api/channel/plugins/disable'),
  /** 删除渠道行：停实例 + 清该渠道会话 + 删行（会话所产生的对话保留）。 */
  deletePlugin: httpPost<void, { plugin_id: string }>('/api/channel/plugins/delete'),
  testPlugin: httpPost<
    { success: boolean; bot_username?: string; error?: string },
    { plugin_id: string; token: string; extra_config?: { app_id?: string; app_secret?: string; app_token?: string; homeserver_url?: string; user_id?: string; server_url?: string; nostr_relays?: string } }
  >('/api/channel/plugins/test'),
  getPendingPairings: withResponseMap(httpGet<RawPairing[], void>('/api/channel/pairings'), (raw) =>
    raw.map(toPairing)
  ),
  approvePairing: httpPost<void, { code: string }>('/api/channel/pairings/approve'),
  rejectPairing: httpPost<void, { code: string }>('/api/channel/pairings/reject'),
  getAuthorizedUsers: withResponseMap(httpGet<RawUser[], void>('/api/channel/users'), (raw) => raw.map(toChannelUser)),
  revokeUser: httpPost<void, { user_id: string }>('/api/channel/users/revoke'),
  getActiveSessions: withResponseMap(httpGet<RawSession[], void>('/api/channel/sessions'), (raw) =>
    raw.map(toChannelSession)
  ),
  syncChannelSettings: httpPost<void, { platform: string }>('/api/channel/settings/sync'),
  /**
   * Bind one companion as the master-agent greeter for an IM platform (spec §4.4/§4.7).
   * Atomic on the backend: writes the `assistant.{platform}.companionId` client
   * preference and resets the platform's active sessions in one step.
   * Omitted/empty `companion_id` clears the binding (falls back to the default companion).
   * Binding a non-existent companion returns 400 — errors propagate to the caller
   * as `BackendHttpError`.
   */
  setMasterAgentCompanion: httpPost<void, { platform?: string; plugin_id?: string; companion_id?: string | null }>(
    '/api/channel/settings/companion'
  ),
  /**
   * Bind one public agent (对外伙伴) as the master-agent greeter for a channel row.
   * Symmetric to {@link setMasterAgentCompanion} but for public agents — a channel
   * bot serves EITHER a companion OR a public agent (mutually exclusive). Atomic on
   * the backend: persists the binding AND resets only this channel row's sessions.
   * A `null` `public_agent_id` clears the binding.
   */
  setMasterAgentPublicAgent: httpPost<void, { plugin_id: string; public_agent_id: string | null }>(
    '/api/channel/settings/public-agent'
  ),
  /**
   * 启动微信扫码登录流程。后端立即返回，二维码生命周期事件经 WebSocket 的
   * `weixinLogin` 推送。改用 WS（不再用 SSE）：`EventSource` 带不了桌面的
   * `x-nomi-local-trust` 头，旧 SSE 流被鉴权中间件 403 → 前端秒弹"微信登录失败"。
   */
  startWeixinLogin: httpPost<void, void>('/api/channel/weixin/login/start'),
  pairingRequested: wsMappedEmitter<IChannelPairingRequest>('channel.pairing-requested', (raw) =>
    toPairing(raw as RawPairing)
  ),
  pluginStatusChanged: wsMappedEmitter<{ plugin_id: string; status: IChannelPluginStatus }>(
    'channel.plugin-status-changed',
    (raw) => {
      const r = raw as Record<string, unknown>;
      return {
        plugin_id: r.plugin_id as string,
        status: toPluginStatus(r.status as RawPluginStatus),
      };
    }
  ),
  userAuthorized: wsMappedEmitter<IChannelUser>('channel.user-authorized', (raw) => toChannelUser(raw as RawUser)),
  /**
   * 微信扫码登录生命周期事件（替代旧 SSE 流）。`phase` 区分阶段：
   * `qr`(带 qrcodeData) → `scanned` → 终态 `done`(带 accountId/botToken) 或 `error`(带 message)。
   */
  weixinLogin: wsEmitter<{
    phase: 'qr' | 'scanned' | 'done' | 'error';
    qrcodeData?: string;
    accountId?: string;
    botToken?: string;
    baseUrl?: string;
    message?: string;
  }>('channel.weixin-login'),
};

// ---------------------------------------------------------------------------
// Agent Hub API — routed to /api/hub/*
// ---------------------------------------------------------------------------

import type { HubExtensionStatus, IHubAgentItem } from '@/common/types/agent/hub';
import type { AgentMetadata } from '@/renderer/utils/model/agentTypes';

export const hub = {
  getExtensionList: httpGet<IHubAgentItem[], void>('/api/hub/extensions'),
  install: httpPost<void, { name: string }>('/api/hub/install'),
  uninstall: httpPost<void, { name: string }>('/api/hub/uninstall'),
  retryInstall: httpPost<void, { name: string }>('/api/hub/retry-install'),
  checkUpdates: httpPost<{ name: string }[], void>('/api/hub/check-updates'),
  update: httpPost<void, { name: string }>('/api/hub/update'),
  onStateChanged: wsEmitter<{ name: string; status: HubExtensionStatus; error?: string }>('hub.state-changed'),
};

// ── Requirements Platform (需求平台) ─────────────────────────────────

export type RequirementStatus = 'pending' | 'in_progress' | 'done' | 'failed' | 'cancelled' | 'needs_review';

export interface IAttachment {
  id: string;
  file_name: string;
  mime: string;
  size_bytes: number;
  created_at: number;
  /** Absolute path resolved by the backend at read time, for image-base64 display. */
  abs_path: string;
}

export interface INewAttachmentRef {
  /** Absolute path returned by POST /api/fs/upload (must be inside the temp upload root). */
  source_path: string;
  file_name: string;
}

/** Owner session domain of a requirement (which entity claimed/owns it). */
export type RequirementOwnerKind = 'conversation' | 'terminal';

export interface IRequirement {
  /** Requirement primary key — backend-minted INTEGER (numeric-id spec),
   *  rendered as `#N`. */
  id: number;
  title: string;
  content: string;
  tag: string;
  order_key: string;
  status: RequirementStatus;
  completion_note?: string;
  /** Owning session id — a conversation id or terminal session id, both
   *  INTEGER (double-domain, no FK; disambiguated by owner_kind). */
  owner_session_id?: number;
  /** Discriminator for owner_session_id's domain. */
  owner_kind?: RequirementOwnerKind;
  started_at?: number;
  completed_at?: number;
  attempt_count: number;
  created_by: string;
  created_at: number;
  updated_at: number;
  /** Only present on get/create/update responses — list/board rows omit attachments to keep payloads small. */
  attachments?: IAttachment[];
}

/** Whitelisted sort columns for the requirements list (server validates too). */
export type RequirementOrderBy = 'id' | 'created_at' | 'updated_at' | 'status';

export interface IListRequirementsParams {
  tag?: string;
  status?: RequirementStatus;
  /** Filter by owning session id (conversation id or terminal session id, INTEGER). */
  owner_session_id?: number;
  q?: string;
  /** Sort column; omit for the default queue order (sort_seq, priority, created_at). */
  order_by?: RequirementOrderBy;
  /** Sort direction; server defaults to 'desc' when order_by is set. */
  order?: 'asc' | 'desc';
  page?: number;
  page_size?: number;
}

export interface ICreateRequirementParams {
  title: string;
  content?: string;
  tag: string;
  order_key?: string;
  status?: RequirementStatus;
  created_by?: string;
  attachments?: INewAttachmentRef[];
}

export interface IUpdateRequirementParams {
  title?: string;
  content?: string;
  tag?: string;
  order_key?: string;
  status?: RequirementStatus;
  completion_note?: string;
  add_attachments?: INewAttachmentRef[];
  remove_attachment_ids?: string[];
}

export interface ITagSummary {
  tag: string;
  pending: number;
  in_progress: number;
  done: number;
  failed: number;
  cancelled: number;
  needs_review: number;
  total: number;
  /** AutoWork is paused for this tag (a requirement exhausted its retries).
   * While true, the orchestrator does not claim this tag's requirements until
   * the tag is resumed. */
  paused: boolean;
  /** Why the tag was paused (`requirement_failed` | `manual` | …). */
  paused_reason?: string;
}

export interface IBoardResponse {
  tag: string;
  pending: IRequirement[];
  in_progress: IRequirement[];
  done: IRequirement[];
  failed: IRequirement[];
  cancelled: IRequirement[];
  needs_review: IRequirement[];
}

/** Broadcast (`autowork.tagPaused`) when AutoWork pauses a tag because one of
 * its requirements exhausted its retries. */
export interface ITagPausedPayload {
  tag: string;
  reason: string;
  requirement_id?: number;
}

export type AutoWorkTargetKind = 'conversation' | 'terminal';
export type AutoWorkRunState = 'off' | 'idle' | 'active';

export interface IAutoWorkConfigParams {
  kind: AutoWorkTargetKind;
  /** Session id (conversation or terminal, both INTEGER per numeric-id spec). */
  target_id: number;
  enabled: boolean;
  tag?: string;
  max_requirements?: number;
  /** Set by the AutoWork admin (标签会话管理). When true, the backend rejects
   * disabling an actively-executing session — the user must stop it from the
   * session page. Session-page toggles leave this unset. */
  from_admin?: boolean;
}

export interface IAutoWorkState {
  kind: AutoWorkTargetKind;
  /** Session id (conversation or terminal, both INTEGER). */
  target_id: number;
  enabled: boolean;
  tag?: string;
  running: boolean;
  run_state: AutoWorkRunState;
  current_requirement_id?: number;
  completed_count: number;
}

export const requirements = {
  list: httpGet<PaginatedResult<IRequirement>, IListRequirementsParams>((p) => {
    const q = new URLSearchParams();
    if (p?.tag) q.set('tag', p.tag);
    if (p?.status) q.set('status', p.status);
    if (p?.owner_session_id != null) q.set('owner_session_id', String(p.owner_session_id));
    if (p?.q) q.set('q', p.q);
    if (p?.order_by) q.set('order_by', p.order_by);
    if (p?.order) q.set('order', p.order);
    if (p?.page != null) q.set('page', String(p.page));
    if (p?.page_size != null) q.set('page_size', String(p.page_size));
    const qs = q.toString();
    return `/api/requirements${qs ? `?${qs}` : ''}`;
  }),
  get: httpGet<IRequirement, { id: number }>((p) => `/api/requirements/${p.id}`),
  create: httpPost<IRequirement, ICreateRequirementParams>('/api/requirements'),
  update: httpPut<IRequirement, { id: number; updates: IUpdateRequirementParams }>(
    (p) => `/api/requirements/${p.id}`,
    (p) => p.updates
  ),
  remove: httpDelete<void, { id: number }>((p) => `/api/requirements/${p.id}`),
  batchDelete: httpPost<{ deleted: number }, { ids: number[] }>('/api/requirements/batch-delete'),
  tags: httpGet<ITagSummary[], void>('/api/requirements/tags'),
  board: httpGet<IBoardResponse, { tag: string }>((p) => `/api/requirements/board?tag=${encodeURIComponent(p.tag)}`),
  setAutoWork: httpPost<IAutoWorkState, IAutoWorkConfigParams>('/api/requirements/autowork'),
  getAutoWork: httpGet<IAutoWorkState, { kind: AutoWorkTargetKind; target_id: number }>(
    (p) => `/api/requirements/autowork/${p.kind}/${p.target_id}`
  ),
  resumeTag: httpPost<ITagSummary, { tag: string; requeue_failed?: boolean; requeue_ids?: number[] }>(
    (p) => `/api/requirements/tags/${encodeURIComponent(p.tag)}/resume`,
    (p) => ({ requeue_failed: p.requeue_failed, requeue_ids: p.requeue_ids })
  ),
  onCreated: wsEmitter<IRequirement>('requirement.created'),
  onUpdated: wsEmitter<IRequirement>('requirement.updated'),
  onStatusChanged: wsEmitter<IRequirement>('requirement.statusChanged'),
  onDeleted: wsEmitter<{ id: number }>('requirement.deleted'),
  onAutoWork: wsEmitter<IAutoWorkState>('autowork.statusChanged'),
  onTagPaused: wsEmitter<ITagPausedPayload>('autowork.tagPaused'),
  tagBindings: httpGet<ITagBindings[], void>('/api/requirements/tag-bindings'),
};

// ─────────────────────────── IDMM (Intelligent Decision-Making Mode) ───────────────────────────

export type IdmmTargetKind = 'conversation' | 'terminal';
export type IdmmRunState = 'off' | 'armed' | 'intervening';

// ── Phase-2 dual-watch config (mirrors `nomifun-api-types/src/idmm.rs` D1/D2). ──
// IDMM is reorganized into two independently-toggleable, default-off watches that
// share one engine: 故障值守 (fault watch) and 决策值守 (decision watch). The
// backend flattens `WatchBase` into each watch (serde `#[flatten]`), so the base
// knobs live at the top level of each watch object on the wire.

/** Rule-only (no model) vs rule + backup model. */
export type IdmmWatchTier = 'rule_only' | 'rule_plus_model';

/** How much context the watch scans / feeds the backup model. */
export type IdmmScanScope = 'last_turn' | 'last_messages' | 'full_session';

/** Backup ("bypass") model the watch escalates to (empty → global default → session model). */
export interface IIdmmBypassModelRef {
  provider_id?: string | null;
  model?: string | null;
}

/** Rate limits to keep a watch from thrashing a session. */
export interface IIdmmBudgetConfig {
  max_interventions_per_hour: number;
  min_interval_secs: number;
}

/** Shared base knobs flattened into each watch config. */
export interface IIdmmWatchBase {
  enabled: boolean;
  tier: IdmmWatchTier;
  /** 监测间隔 (was idle_threshold_secs). */
  scan_interval_secs: number;
  /** 最大重试. */
  max_retries: number;
  /** 扫描范围. */
  scan_scope: IdmmScanScope;
  /** Context-char ceiling fed to the bypass model (carried over default 8000). */
  max_context_chars: number;
  /** 旁路模型. */
  bypass_model: IIdmmBypassModelRef;
  budget: IIdmmBudgetConfig;
}

/** P3 fault failover strategy; P2 only Retry is live. */
export type IdmmWakeStrategy = 'retry' | 'failover' | 'failover_then_retry';

/** 故障值守 — base flattened to top level + fault-specific fields. */
export interface IIdmmFaultWatchConfig extends IIdmmWatchBase {
  wake_action: IdmmWakeStrategy;
  use_failover_queue: boolean;
}

// ── Decision strategy (D2) ──

export type IdmmTendency = 'conservative' | 'balanced' | 'aggressive';
export type IdmmBlockedBehavior = 'prefer_continue' | 'prefer_pause' | 'must_ask';
export type IdmmCategoryMode = 'auto' | 'ask_first' | 'off';

export interface IIdmmOptionRule {
  mode: IdmmCategoryMode;
  prefer_recommended: boolean;
  allow_unmarked_pick: boolean;
  never_destructive: boolean;
}
export interface IIdmmOpenQuestionRule {
  mode: IdmmCategoryMode;
  max_answer_chars: number;
}
export interface IIdmmPermissionRule {
  mode: IdmmCategoryMode;
  only_safe_value: boolean;
  escalate_risky: boolean;
}
export interface IIdmmCategoryRules {
  option_decision: IIdmmOptionRule;
  open_question: IIdmmOpenQuestionRule;
  permission: IIdmmPermissionRule;
}
export interface IIdmmDecisionStrategy {
  tendency: IdmmTendency;
  on_blocked: IdmmBlockedBehavior;
  categories: IIdmmCategoryRules;
  /** 自由文本策略 — appended to the bypass-model prompt (model tier only). */
  freeform_policy?: string | null;
}

/** 决策值守 — base flattened to top level + decision-specific fields. */
export interface IIdmmDecisionWatchConfig extends IIdmmWatchBase {
  strategy: IIdmmDecisionStrategy;
  /** 纯问答开关 — answer open-ended questions (only effective at rule_plus_model). */
  answer_open_questions: boolean;
}

export interface IIdmmConfig {
  fault_watch: IIdmmFaultWatchConfig;
  decision_watch: IIdmmDecisionWatchConfig;
}

/** POST /api/idmm body: kind + target_id + a (flattened) IdmmConfig. */
export interface IIdmmSetParams extends IIdmmConfig {
  kind: IdmmTargetKind;
  /** Session id (conversation or terminal, both INTEGER per numeric-id spec). */
  target_id: number;
}

export interface IIdmmState {
  kind: IdmmTargetKind;
  /** Session id (conversation or terminal, both INTEGER). */
  target_id: number;
  /** True when either watch is enabled. */
  enabled: boolean;
  run_state: IdmmRunState;
  interventions_count: number;
  last_signal?: string;
  last_intervention_at?: number;
  /** Whether a backup provider is resolvable (per-session or global default). */
  sidecar_provider_resolved: boolean;
  /**
   * Persisted per-session IdmmConfig — the form's source of truth on remount.
   * Absent for targets that have never been saved. Without this round-trip,
   * user edits would silently disappear after navigation.
   */
  config?: IIdmmConfig;
}

/** One persisted IDMM decision (the "思路"/audit trail row). Field names mirror
 * the backend `InterventionRecord` JSON exactly. `target_id` is polymorphic on
 * the wire (conversation/terminal id serialized as a string). */
export interface IIdmmIntervention {
  /** `idmmrec_{uuidv7}` — absent on legacy in-memory payloads. */
  id: string;
  target_kind: IdmmTargetKind;
  target_id: string;
  /** Which watch fired: 'fault' | 'decision'. */
  watch: string;
  at: number;
  stall_class: string;
  tier_used: string;
  /** option / open_question / permission / fault. */
  category?: string;
  action: string;
  /** What was picked/answered (truncated server-side). */
  detail?: string;
  outcome: string;
  /** The reasoning ("思路") — model reason or a rule explanation. */
  reason?: string;
  /** Model confidence (null for the rule tier). */
  confidence?: number;
  /** provider/model used (null for the rule tier). */
  bypass_model?: string;
}

export interface IIdmmSettings {
  backup_provider_id?: string;
  backup_model?: string;
  default_steering_prompt: string;
}

export const idmm = {
  set: httpPost<IIdmmState, IIdmmSetParams>('/api/idmm'),
  getStatus: httpGet<IIdmmState, { kind: IdmmTargetKind; target_id: number }>(
    (p) => `/api/idmm/${p.kind}/${p.target_id}`
  ),
  intervene: httpPost<IIdmmState, { kind: IdmmTargetKind; target_id: number }>(
    (p) => `/api/idmm/${p.kind}/${p.target_id}/intervene`,
    () => ({})
  ),
  getLog: httpGet<IIdmmIntervention[], { kind: IdmmTargetKind; target_id: number; limit?: number }>(
    (p) => `/api/idmm/${p.kind}/${p.target_id}/log${p.limit ? `?limit=${p.limit}` : ''}`
  ),
  clearLog: httpDelete<void, { kind: IdmmTargetKind; target_id: number }>(
    (p) => `/api/idmm/${p.kind}/${p.target_id}/log`
  ),
  /** Cross-session recent interventions for the global activity overview
   * (most-recent-first, across all targets; honours the same aggressive
   * eviction the per-target records do). */
  getActivity: httpGet<IIdmmIntervention[], { limit?: number }>(
    (p) => `/api/idmm/activity${p.limit ? `?limit=${p.limit}` : ''}`
  ),
  clearActivity: httpDelete<void, void>('/api/idmm/activity'),
  getSettings: httpGet<IIdmmSettings, void>('/api/idmm/settings'),
  updateSettings: httpPut<IIdmmSettings, IIdmmSettings>('/api/idmm/settings'),
  onStatus: wsEmitter<IIdmmState>('idmm.statusChanged'),
  onIntervention: wsEmitter<IIdmmIntervention>('idmm.intervention'),
};

// ── Phase-3 model failover queue (mirrors `ModelFailoverConfig`, plan D1/D8). ──
// A global, ordered list of provider+model candidates the conversation send-loop
// falls back through when a NOMI session hits a pre-response provider fault. Read
// & written through the `agent.model_failover` client preference (one JSON blob),
// the same idmm-settings-style channel as `idmm.getSettings`/`updateSettings`.

/** One ordered candidate in the failover queue. */
export interface IModelFailoverCandidate {
  provider_id: string;
  model: string;
}

/** Global model-failover config persisted under `agent.model_failover`. */
export interface IModelFailoverConfig {
  /** Master switch; default false. */
  enabled: boolean;
  /** Ordered candidates tried head-to-tail on a pre-response provider fault. */
  queue: IModelFailoverCandidate[];
  /** Per-turn cap on switches (also bounded by `queue.length`); default 4. */
  max_switches: number;
  /** Stamp the failed model `Unhealthy` on switch; default true. */
  stamp_unhealthy: boolean;
}

export const agentModelFailover = {
  getSettings: httpGet<IModelFailoverConfig, void>('/api/agent/model-failover'),
  updateSettings: httpPut<IModelFailoverConfig, IModelFailoverConfig>('/api/agent/model-failover'),
};

// ─────────────────────────── Webhook + AutoWork admin ───────────────────────────

/** AutoWork tag→session binding (a session whose AutoWork is enabled on a tag). */
export interface ITagBinding {
  kind: AutoWorkTargetKind;
  /** Session id (conversation or terminal, both INTEGER). */
  target_id: number;
  name: string;
  run_state: AutoWorkRunState;
}

/** All AutoWork bindings for one tag (used by 标签会话管理). */
export interface ITagBindings {
  tag: string;
  bindings: ITagBinding[];
}

export type IWebhookPlatform = 'lark' | 'http' | 'slack';

/** A webhook endpoint. The signing `secret` is never returned — `has_secret`
 * signals whether one is stored. */
export interface IWebhook {
  id: number;
  name: string;
  platform: IWebhookPlatform;
  url: string;
  description: string;
  has_secret: boolean;
  enabled: boolean;
  created_at: number;
  updated_at: number;
}

export interface ICreateWebhookParams {
  name: string;
  url: string;
  platform?: IWebhookPlatform;
  description?: string;
  /** Optional Lark signing secret (加签). */
  secret?: string | null;
  enabled?: boolean;
}

/** Partial update. `secret`: omit = keep, `null` = clear, string = set. */
export interface IUpdateWebhookParams {
  name?: string;
  url?: string;
  platform?: IWebhookPlatform;
  description?: string;
  secret?: string | null;
  enabled?: boolean;
}

/** Per-tag settings (bound webhook + description) over the implicit tags. */
export interface ITagSetting {
  tag: string;
  webhook_id?: number | null;
  description: string;
  /** Event kinds that trigger a notification for this tag. */
  notify_events: string[];
}

export interface IUpsertTagSettingParams {
  /** omit = keep, `null` = clear, number = bind. */
  webhook_id?: number | null;
  description?: string;
  /** omit = keep, array = replace the notify-event set. */
  notify_events?: string[];
}

export const webhook = {
  list: httpGet<IWebhook[], void>('/api/webhooks'),
  get: httpGet<IWebhook, { id: number }>((p) => `/api/webhooks/${p.id}`),
  create: httpPost<IWebhook, ICreateWebhookParams>('/api/webhooks'),
  update: httpPut<IWebhook, { id: number; updates: IUpdateWebhookParams }>(
    (p) => `/api/webhooks/${p.id}`,
    (p) => p.updates
  ),
  remove: httpDelete<void, { id: number }>((p) => `/api/webhooks/${p.id}`),
  test: httpPost<void, { id: number }>(
    (p) => `/api/webhooks/${p.id}/test`,
    () => ({})
  ),
  getTagSetting: httpGet<ITagSetting, { tag: string }>((p) => `/api/tags/${encodeURIComponent(p.tag)}/settings`),
  setTagSetting: httpPut<ITagSetting, { tag: string; updates: IUpsertTagSettingParams }>(
    (p) => `/api/tags/${encodeURIComponent(p.tag)}/settings`,
    (p) => p.updates
  ),
};

// ─────────────────────────── 智能编排 (Orchestration) ───────────────────────────
// REST client for fleets + orchestration workspaces, routed to
// /api/orchestrator/*. Mirrors the plain-REST webhook block above; IDs are
// strings (`fleet_…` / `ows_…`).

export const orchestrator = {
  runs: {
    // Every run owned by the current user (all workspaces + ad-hoc/workspace-less
    // runs), newest first — the read path for the read-only Run-history library.
    listMine: httpGet<TRun[], void>('/api/orchestrator/runs'),
    // Create an ad-hoc run straight from the「智能编排」Tab's structured form
    // (no workspace / pre-built fleet — the fleet is synthesized from
    // `model_range`). Backend defaults autonomy to `interactive`, so the run
    // parks at `awaiting_plan_approval` until approved. Body matches the wire
    // shape verbatim, so no mapBody is needed.
    createAdhoc: httpPost<TRun, TCreateAdhocRun>('/api/orchestrator/runs/adhoc'),
    get: httpGet<TRunDetail, { id: string }>((p) => `/api/orchestrator/runs/${p.id}`),
    // Delete a run (owner-scoped). The backend stops any live engine loop first,
    // then deletes the row — the schema's ON DELETE CASCADE FKs sweep out the
    // run's tasks/deps/assignments. 403 if the run is owned by another user.
    remove: httpDelete<void, { id: string }>((p) => `/api/orchestrator/runs/${p.id}`),
    // Rename a run = change its goal (owner-scoped, PATCH). Body is { goal }.
    rename: httpPatch<void, { id: string; goal: string }>(
      (p) => `/api/orchestrator/runs/${p.id}`,
      (p) => ({ goal: p.goal })
    ),
    // Re-plan a run IN PLACE (owner-scoped, POST). Clears the run's old task graph
    // and re-decomposes against the (optionally) edited goal / model_range /
    // autonomy / pinned_roles — every edit field is optional (omitted = keep
    // current). Returns the re-planned run. The backend stops any live engine
    // loop, applies the edits, clears the old plan, then re-plans + re-arms the
    // engine for non-`interactive` runs (`interactive` re-parks at approval).
    replan: httpPost<TRun, { id: string } & TReplanRequest>(
      (p) => `/api/orchestrator/runs/${p.id}/replan`,
      ({ id: _id, ...body }) => body
    ),
    // Conversation-driven intelligent re-adjust IN PLACE (owner-scoped, POST):
    // the lead model judges, per task, whether to KEEP the completed work or
    // re-decompose, and the backend RECONCILEs the run to the result — preserving
    // the kept tasks + their output (unlike `replan`, which wipes the whole plan),
    // dropping the un-kept ones, inserting + routing the new tasks, and rebuilding
    // the deps. Returns the (re-activated) run. Rejected (400) for a blank intent
    // or a run with any running task (pause first). The backend runs the whole
    // reconcile + terminal re-activation under the run's per-run lock and re-arms
    // the engine for a re-activated run, mirroring `rerunTask`.
    adjustRun: httpPost<TRun, { run_id: string } & TAdjustRunRequest>(
      (p) => `/api/orchestrator/runs/${p.run_id}/adjust`,
      ({ run_id: _run_id, ...body }) => body
    ),
    cancel: httpPost<void, { id: string }>(
      (p) => `/api/orchestrator/runs/${p.id}/cancel`,
      () => undefined
    ),
    approve: httpPost<void, { id: string }>(
      (p) => `/api/orchestrator/runs/${p.id}/approve`,
      () => undefined
    ),
    pause: httpPost<void, { id: string }>(
      (p) => `/api/orchestrator/runs/${p.id}/pause`,
      () => undefined
    ),
    resume: httpPost<void, { id: string }>(
      (p) => `/api/orchestrator/runs/${p.id}/resume`,
      () => undefined
    ),
    reassign: httpPut<void, { run_id: string; task_id: string; updates: TReassign }>(
      (p) => `/api/orchestrator/runs/${p.run_id}/tasks/${p.task_id}/assignment`,
      (p) => p.updates
    ),
    steer: httpPost<void, { run_id: string; task_id: string; updates: TSteer }>(
      (p) => `/api/orchestrator/runs/${p.run_id}/tasks/${p.task_id}/steer`,
      (p) => p.updates
    ),
    // Re-execute a single node (UC-2a): resets the task + its settled downstream
    // dependents to pending and re-activates a terminal run, so the engine re-drives
    // it. Rejected (400) if the task is currently running (pause/stop first).
    rerunTask: httpPost<void, { run_id: string; task_id: string }>(
      (p) => `/api/orchestrator/runs/${p.run_id}/tasks/${p.task_id}/rerun`,
      () => undefined
    ),
    // Adopt the node's worker conversation's CURRENT output as the node's product
    // (UC-2c「采用为该节点产出」): reads the worker's latest assistant text into the
    // node, marks it done, and re-activates a terminal run so downstream unblocks.
    // For a failed/stuck node the user kept chatting in the content area. Rejected
    // (400) if the run is running, or the node has no worker conversation / no
    // output yet.
    adoptTaskResult: httpPost<void, { run_id: string; task_id: string }>(
      (p) => `/api/orchestrator/runs/${p.run_id}/tasks/${p.task_id}/adopt`,
      () => undefined
    ),
    // Fine-tune a node's intent/prompt (UC-2a): replace the task's spec. Rejected
    // (400) for a blank spec or a running task; a later rerun uses the new spec.
    updateTaskSpec: httpPatch<void, { run_id: string; task_id: string; updates: TTaskSpecUpdate }>(
      (p) => `/api/orchestrator/runs/${p.run_id}/tasks/${p.task_id}/spec`,
      (p) => p.updates
    ),
    // 启动前配置台 (迁移 026): set/clear a node's per-task model override + 预置要求.
    // FULL replace of the three fields (null/blank clears); rejected (400) for a
    // running task. A pending node picks these up at dispatch; a settled node on
    // the next rerun.
    setTaskConfig: httpPatch<void, { run_id: string; task_id: string; updates: TTaskConfigUpdate }>(
      (p) => `/api/orchestrator/runs/${p.run_id}/tasks/${p.task_id}/config`,
      (p) => p.updates
    ),
    // List one directory level under a run's working directory (read-only). Root
    // is the run's `work_dir` (server resolves the actual dir); the client passes
    // an ABSOLUTE path (root or a node's fullPath) which is mapped to a
    // workspace-relative `path`. Mirrors terminal/conversation `getWorkspace` so
    // the shared WorkspaceRailBody tree source consumes it unchanged (IDirOrFile[]).
    getWorkspace: {
      provider: () => {},
      invoke: (async (p: { id: string; work_dir: string; path: string; search?: string }) => {
        const rel = absoluteToRelativePath(p.path, p.work_dir);
        const url = `/api/orchestrator/runs/${p.id}/workspace?path=${encodeURIComponent(rel)}${p.search ? `&search=${encodeURIComponent(p.search)}` : ''}`;
        const raw = await httpRequest<Array<{ name: string; type: string }>>('GET', url);
        return fromBackendWorkspaceList(raw, p.work_dir, rel);
      }) as (p: { id: string; work_dir: string; path: string; search?: string }) => Promise<IDirOrFile[]>,
    },
  },
  // Realtime WS events from the run engine (OrchestratorRunEventEmitter). Wire
  // names verbatim from orchestratorEvents.ts.
  runEvents: {
    statusChanged: wsEmitter<TOrchRunStatusEvent>('orchestrator.run.statusChanged'),
    planUpdated: wsEmitter<TOrchRunPlanUpdatedEvent>('orchestrator.run.planUpdated'),
    completed: wsEmitter<TOrchRunCompletedEvent>('orchestrator.run.completed'),
    taskStatusChanged: wsEmitter<TOrchTaskStatusEvent>('orchestrator.task.statusChanged'),
    taskAssigned: wsEmitter<TOrchTaskAssignedEvent>('orchestrator.task.assigned'),
    leadThinking: wsEmitter<TOrchRunLeadThinkingEvent>('orchestrator.run.leadThinking'),
  },
};

// ─────────────────────────── Companion (nomi 桌面伙伴) ───────────────────────────

export interface ICompanionCollectConfig {
  chat_user_messages: boolean;
  chat_assistant_replies: boolean;
  requirements: boolean;
  cron_runs: boolean;
  conversation_lifecycle: boolean;
  terminal_sessions: boolean;
  tool_calls: boolean;
}

/** One sanitized collected event ({ts,source,name,data}) for the transparency viewer. */
export interface ICompanionCollectedEvent {
  ts: number;
  source: string;
  name: string;
  data: unknown;
}

export type ICompanionMemoryKind = 'profile' | 'preference' | 'knowledge' | 'episode' | 'task' | 'affective';

export interface ICompanionMemory {
  id: string;
  kind: ICompanionMemoryKind;
  content: string;
  tags: string[];
  importance: number;
  strength: number;
  pinned: boolean;
  source: string;
  status: 'active' | 'archived';
  created_at: number;
  updated_at: number;
  last_reinforced_at: number;
  /** `'user'` = shared (all companions) / `'companion'` = private to one. */
  scope_kind: 'user' | 'companion';
  /** Owning companion id when private; `''` when shared. */
  scope_companion_id: string;
}

export interface ICompanionSuggestion {
  id: string;
  kind: string;
  title: string;
  body: string;
  action?: { type: string; to?: string } | null;
  status: 'new' | 'accepted' | 'dismissed';
  created_at: number;
  decided_at?: number | null;
}

/** A companion's self-evolved skill (registry row + SKILL.md description). snake_case = Rust JSON 1:1. */
export interface ICompanionSkill {
  skill_name: string;
  scope_kind: string;
  scope_companion_id: string; // '' = shared
  status: 'draft' | 'active' | 'archived';
  source: string;
  confidence: number;
  provenance: string[]; // session/event ids the skill grew from
  strength: number;
  version: number;
  superseded_by: string | null;
  usage_count: number;
  last_used_at: number | null;
  created_at: number;
  updated_at: number;
  description: string; // from SKILL.md frontmatter (CompanionSkillView)
}

export interface ICompanionSkillContent {
  skill: ICompanionSkill;
  content: string;
}

/** WS payload for companion.skill-drafted / companion.skill-learned. */
export interface ICompanionSkillEvent {
  companion_id: string;
  skill_name: string;
}

export interface ICompanionLearnRun {
  id: string;
  started_at: number;
  finished_at?: number | null;
  status: string;
  events_processed: number;
  memories_added: number;
  suggestions_added: number;
  error?: string | null;
  summary?: string | null;
}

export interface ICompanionStatus {
  /** Owning companion id (multi-companion; spec 2026-06-11 §4.3). */
  companion_id: string;
  xp: number;
  level: number;
  mood: string;
  memories_active: number;
  memories_archived: number;
  suggestions_new: number;
  skills_active: number;
  model_configured: boolean;
  collect_any_enabled: boolean;
  last_learn?: ICompanionLearnRun | null;
}

/** "What I learned this week" digest (skills per-companion; memories/learn-runs global). */
export interface ICompanionWeeklyDigest {
  since_ms: number;
  skills_learned: number;
  skills_active_new: number;
  memories_added: number;
  learn_runs: number;
  new_skill_names: string[];
  recent_summaries: string[];
}

export interface ICompanionSourceStats {
  source: string;
  today: number;
  total: number;
}

/** One archived session-window day-digest (伙伴会话归档回看). */
export interface ICompanionDayDigest {
  id: string;
  companion_id: string;
  conversation_id: string;
  /** Local start day, `YYYYMMDD`. */
  session_day: string;
  started_at: number;
  last_activity_at: number;
  closed_at: number | null;
  status: string;
  message_count: number;
  boundary_ts: number;
  /** The compressed narrative summary (markdown). */
  digest: string | null;
  /** JSON string: `{topics,decisions,todos,mood}`. */
  highlights: string | null;
  token_estimate: number;
}

/** 伙伴的唯一专属会话 — 一条真实的 `type='nomi'` 会话。每个伙伴生命周期内恒一条。 */
export interface ICompanionThread {
  conversation_id: number;
  companion_id: string;
  title: string;
  created_at: number;
  updated_at: number;
}

// ── Multi-companion (spec docs/superpowers/specs/2026-06-11-unified-memory-knowledge-design.md §4.3/§4.7/§4.8) ──

/** Persona of one companion (same shape as the legacy single-companion config persona). */
export interface ICompanionPersona {
  preset: string;
  custom: string;
}

/** Model reference (provider + model id) as stored in companion configs. */
export interface ICompanionModelRef {
  provider_id: string;
  model: string;
}

/** Desktop-companion window settings of one companion (`character` lives on ICompanionProfile). */
export interface ICompanionWindowConfig {
  companion_enabled: boolean;
  companion_x?: number | null;
  companion_y?: number | null;
  quiet_start: string;
  quiet_end: string;
  /** DIY single-image figure metadata (`character === 'custom'`); absent for roster characters.
   *  `null` in a patch clears it (RFC 7396) — used when switching back to a built-in character. */
  custom_figure?: {
    aspect: number;
    head_box: { x: number; y: number; w: number; h?: number };
    size_tier: 's' | 'm' | 'l';
    /** Per-companion continuous figure-height override (logical px); supersedes `size_tier`
     *  for this companion's desktop window. Absent ⇒ fall back to the tier. `null` in a patch
     *  clears it (RFC 7396) — used by the 总览 size slider's reset. */
    size_px?: number | null;
    /** Library figure id (`figure_…`) backing this companion; absent for legacy per-companion figures. */
    figure_id?: string;
  } | null;
}

/** A reusable figure in the shared custom-figure library (decoupled from companions). */
export interface IFigureMeta {
  id: string;
  name: string;
  aspect: number;
  head_box: { x: number; y: number; w: number; h?: number };
  size_tier: 's' | 'm' | 'l';
  created_at: number;
}

export type IFigureUpdatePatch = {
  figure_id: string;
  name?: string;
  head_box?: { x: number; y: number; w: number; h: number };
  size_tier?: 's' | 'm' | 'l';
};

/** One companion's profile — `companions/{companion_id}/config.json`. */
export interface ICompanionProfile {
  id: string;
  name: string;
  /** Character id (mochi/ink/roux/pixel/bolt/boo); unknown → default. */
  character: string;
  persona: ICompanionPersona;
  model: ICompanionModelRef;
  appearance: ICompanionWindowConfig;
  created_at: number;
}

/** Shared skill-evolution settings (P1/P2 backend; P3 surfaces in UI). */
export interface ICompanionEvolveConfig {
  enabled: boolean;
  interval_minutes: number;
  model: ICompanionModelRef;
  min_pattern_count: number;
  min_distinct_sessions: number;
  reflect_enabled: boolean;
  auto_activate: boolean;
  auto_threshold: number;
}

/** Shared session-window archiving settings (伙伴会话窗口归档). Default OFF (opt-in). */
export interface ICompanionArchiveConfig {
  enabled: boolean;
  idle_minutes: number;
  min_chars: number;
  inject_recent_days: number;
}

/** Shared (cross-companion) config — `shared/config.json`, served by /api/companion/config. */
export interface ICompanionSharedConfig {
  collect: ICompanionCollectConfig;
  learn: {
    enabled: boolean;
    interval_minutes: number;
    model: ICompanionModelRef;
  };
  evolve: ICompanionEvolveConfig;
  /** Session-window archiving (伙伴会话归档). */
  archive: ICompanionArchiveConfig;
  /** 智能编排：开启后本地伙伴会话可用 nomi_run_create 把复杂大任务拆给子 agent。 */
  smart_orchestration: boolean;
  /** Empty when no companion exists yet (zero-companion state is allowed). */
  default_companion_id: string | null;
}

export type ICompanionWithStatus = ICompanionProfile & { status: ICompanionStatus };

/// RFC 7396 merge patch over ICompanionProfile — nested partial objects merge.
export type ICompanionProfilePatch = {
  name?: string;
  character?: string;
  persona?: Partial<ICompanionPersona>;
  model?: Partial<ICompanionModelRef>;
  appearance?: Partial<ICompanionWindowConfig>;
};

/// RFC 7396 merge patch over ICompanionSharedConfig — nested partial objects merge.
export type ICompanionSharedConfigPatch = {
  collect?: Partial<ICompanionCollectConfig>;
  learn?: Partial<{
    enabled: boolean;
    interval_minutes: number;
    model: ICompanionModelRef;
  }>;
  evolve?: Partial<ICompanionEvolveConfig>;
  archive?: Partial<ICompanionArchiveConfig>;
  smart_orchestration?: boolean;
};

/** Export endpoint result — backend echoes the resolved destination path
 *  (plus backend-reported stats; contract still settling). */
export interface ICompanionExportResult {
  dest_path: string;
  [extra: string]: unknown;
}

// WS event payloads — per-companion events carry `companion_id` (spec §4.3).

/** `companion.config-updated` — `scope` distinguishes a shared-config change
 *  (`'shared'`) from a per-companion profile change (`scope === companion_id`, payload =
 *  the full companion profile); `companion_id` is set for per-companion scope. The payload
 *  remainder is scope-dependent, hence the open index signature. */
export interface ICompanionConfigUpdatedEvent {
  scope?: string;
  companion_id?: string;
  /** Scope-dependent payload remainder (shared config or full companion profile). */
  [extra: string]: unknown;
}

/** `companion.created` */
export interface ICompanionCreatedEvent {
  companion_id: string;
  profile: ICompanionProfile;
}

/** `companion.deleted` */
export interface ICompanionDeletedEvent {
  companion_id: string;
}

export const companion = {
  listMemories: httpGet<
    ICompanionMemory[],
    { kind?: string; q?: string; status?: string; scope_companion_id?: string; limit?: number; offset?: number }
  >((p) => {
    const params = new URLSearchParams();
    if (p?.kind) params.set('kind', p.kind);
    if (p?.q) params.set('q', p.q);
    if (p?.status) params.set('status', p.status);
    if (p?.scope_companion_id) params.set('scope_companion_id', p.scope_companion_id);
    if (p?.limit) params.set('limit', String(p.limit));
    if (p?.offset) params.set('offset', String(p.offset));
    const qs = params.toString();
    return `/api/companion/memories${qs ? `?${qs}` : ''}`;
  }),
  addMemory: httpPost<ICompanionMemory, { kind: string; content: string; tags?: string[]; scope_companion_id?: string }>(
    '/api/companion/memories'
  ),
  updateMemory: httpPut<
    void,
    { id: string; content?: string; pinned?: boolean; status?: string; scope_kind?: string; scope_companion_id?: string }
  >(
    (p) => `/api/companion/memories/${p.id}`,
    (p) => ({
      content: p.content,
      pinned: p.pinned,
      status: p.status,
      scope_kind: p.scope_kind,
      scope_companion_id: p.scope_companion_id,
    })
  ),
  deleteMemory: httpDelete<void, { id: string }>((p) => `/api/companion/memories/${p.id}`),
  listSuggestions: httpGet<ICompanionSuggestion[], { status?: string; limit?: number }>((p) => {
    const params = new URLSearchParams();
    if (p?.status) params.set('status', p.status);
    if (p?.limit) params.set('limit', String(p.limit));
    const qs = params.toString();
    return `/api/companion/suggestions${qs ? `?${qs}` : ''}`;
  }),
  decideSuggestion: httpPost<ICompanionSuggestion, { id: string; accept: boolean }>(
    (p) => `/api/companion/suggestions/${p.id}/decide`,
    (p) => ({ accept: p.accept })
  ),
  // ── Self-evolved skills (P2: see + edit). Keyed by companion_id + skill_name (no standalone id). ──
  listSkills: httpGet<ICompanionSkill[], { companion_id: string; include_shared?: boolean }>(
    (p) =>
      `/api/companion/companions/${p.companion_id}/skills${p.include_shared === false ? '?include_shared=false' : ''}`
  ),
  getSkillContent: httpGet<ICompanionSkillContent, { companion_id: string; name: string }>(
    (p) => `/api/companion/companions/${p.companion_id}/skills/${encodeURIComponent(p.name)}`,
    { silentStatuses: [404] }
  ),
  writeSkillContent: httpPut<void, { companion_id: string; name: string; content: string }>(
    (p) => `/api/companion/companions/${p.companion_id}/skills/${encodeURIComponent(p.name)}`,
    (p) => ({ content: p.content })
  ),
  decideSkill: httpPost<ICompanionSkill, { companion_id: string; name: string; accept: boolean; reason?: string }>(
    (p) => `/api/companion/companions/${p.companion_id}/skills/${encodeURIComponent(p.name)}/decide`,
    (p) => ({ accept: p.accept, reason: p.reason })
  ),
  weeklyDigest: httpGet<ICompanionWeeklyDigest, { companion_id: string; days?: number }>(
    (p) => `/api/companion/companions/${p.companion_id}/weekly-digest${p.days ? `?days=${p.days}` : ''}`
  ),
  /** Archived session-window day-digests (伙伴会话归档回看时间线 / 去年今日). */
  listDayDigests: httpGet<
    ICompanionDayDigest[],
    { companion_id: string; since?: string; until?: string; on_day?: string; limit?: number }
  >((p) => {
    const q = new URLSearchParams();
    if (p.since) q.set('since', p.since);
    if (p.until) q.set('until', p.until);
    if (p.on_day) q.set('on_day', p.on_day);
    if (p.limit) q.set('limit', String(p.limit));
    const qs = q.toString();
    return `/api/companion/companions/${p.companion_id}/digests${qs ? `?${qs}` : ''}`;
  }),
  /** Learn-by-demonstration: draft a skill from a work session's tool sequence. Returns the name. */
  draftFromSession: httpPost<string | null, { companion_id: string; conversation_id: string }>(
    (p) => `/api/companion/companions/${p.companion_id}/skills/from-session`,
    (p) => ({ conversation_id: p.conversation_id })
  ),
  /** Gift a skill to another companion (互教). */
  giftSkill: httpPost<ICompanionSkill, { companion_id: string; name: string; to_companion_id: string }>(
    (p) => `/api/companion/companions/${p.companion_id}/skills/${encodeURIComponent(p.name)}/gift`,
    (p) => ({ to_companion_id: p.to_companion_id })
  ),
  runLearn: httpPost<ICompanionLearnRun, void>('/api/companion/learn/run'),
  listLearnRuns: httpGet<ICompanionLearnRun[], { limit?: number }>(
    (p) => `/api/companion/learn/runs${p?.limit ? `?limit=${p.limit}` : ''}`
  ),
  eventStats: httpGet<ICompanionSourceStats[], void>('/api/companion/events/stats'),
  recentEvents: httpGet<ICompanionCollectedEvent[], { limit?: number }>(
    (p) => `/api/companion/events/recent${p?.limit ? `?limit=${p.limit}` : ''}`
  ),
  clearEvents: httpDelete<void, void>('/api/companion/events'),
  /** First-launch consent: apply self-evolution default-ON once (server KV-gated). */
  applyConsent: httpPost<ICompanionSharedConfig, void>('/api/companion/consent'),
  /** Master kill switch: stop all collection + learning + evolution. */
  disableAll: httpPost<ICompanionSharedConfig, void>('/api/companion/disable-all'),
  // ── Multi-companion CRUD (spec §4.3) ──
  listCompanions: httpGet<ICompanionWithStatus[], void>('/api/companion/companions'),
  createCompanion: httpPost<ICompanionProfile, { name: string; character: string }>('/api/companion/companions'),
  getCompanion: httpGet<ICompanionWithStatus, { companion_id: string }>((p) => `/api/companion/companions/${p.companion_id}`),
  /** RFC 7396 merge patch over one companion's profile (name/character/persona/model/appearance). */
  patchCompanion: httpPatch<ICompanionProfile, { companion_id: string; patch: ICompanionProfilePatch }>(
    (p) => `/api/companion/companions/${p.companion_id}`,
    (p) => p.patch
  ),
  deleteCompanion: httpDelete<void, { companion_id: string }>((p) => `/api/companion/companions/${p.companion_id}`),
  getCompanionStatus: httpGet<ICompanionStatus, { companion_id: string }>((p) => `/api/companion/companions/${p.companion_id}/status`),
  /** Ingest a DIY figure image previously landed in the temp upload root via `/api/fs/upload` (two-phase upload). */
  uploadFigure: httpPost<void, { companion_id: string; source_path: string }>(
    (p) => `/api/companion/companions/${p.companion_id}/figure`,
    (p) => ({ source_path: p.source_path })
  ),
  // ── Custom-figure library (reusable, decoupled from companions) ──
  listFigures: httpGet<IFigureMeta[], void>('/api/companion/figures'),
  /** Create a reusable library figure from a temp upload (two-phase upload). */
  createFigure: httpPost<
    IFigureMeta,
    { source_path: string; name: string; aspect: number; head_box: { x: number; y: number; w: number; h: number }; size_tier: 's' | 'm' | 'l' }
  >('/api/companion/figures'),
  updateFigure: httpPatch<IFigureMeta, IFigureUpdatePatch>(
    (p) => `/api/companion/figures/${p.figure_id}`,
    (p) => ({ name: p.name, head_box: p.head_box, size_tier: p.size_tier })
  ),
  renameFigure: httpPatch<IFigureMeta, { figure_id: string; name: string }>(
    (p) => `/api/companion/figures/${p.figure_id}`,
    (p) => ({ name: p.name })
  ),
  deleteFigure: httpDelete<void, { figure_id: string }>((p) => `/api/companion/figures/${p.figure_id}`),
  // ── 伙伴单会话（companion single session）──
  // 每个伙伴生命周期内恒一条专属会话；多线程列表/新建/重命名/单删/设活已废除。
  /** 返回该伙伴唯一会话的 id（无则 null）。
   *  ⚠ companion 后端域仍以字符串存储 conversation_id（companion_threads.conversation_id
   *  为 TEXT 主键、CompanionThread.conversation_id: String），而实时 `message.stream` 的
   *  conversation_id 是数字 i64（stream_relay conv_id()→i64）。下游订阅按 `!==` 严格过滤
   *  会话：数字 wire 与字符串 id 永不相等 → 实时事件（含 finish）全被丢弃，桌面伙伴气泡/伙伴
   *  会话卡在「处理中」、仅切换会话重拉 DB 才出结果。这里在 API 边界统一强转为 number，
   *  与声明类型 `number` 及 wire 对齐，修复 CompanionChat 与桌面伙伴两处流式过滤。 */
  getCompanionSession: withResponseMap(
    httpGet<{ conversation_id: number | string | null }, { companion_id: string }>(
      (p) => `/api/companion/companions/${p.companion_id}/companion/active`
    ),
    (raw): { conversation_id: number | null } => ({
      conversation_id: raw.conversation_id == null ? null : Number(raw.conversation_id),
    })
  ),
  /** 幂等 ensure：已存在则返回现有唯一会话，不存在则创建（要求 profile.model 已配置，否则 400）。
   *  同 getCompanionSession：把后端字符串 conversation_id 在边界强转为 number。 */
  ensureCompanionSession: withResponseMap(
    httpPost<Omit<ICompanionThread, 'conversation_id'> & { conversation_id: number | string }, { companion_id: string }>(
      (p) => `/api/companion/companions/${p.companion_id}/companion/threads`,
      () => ({})
    ),
    (raw): ICompanionThread => ({ ...raw, conversation_id: Number(raw.conversation_id) })
  ),
  // ── Shared (cross-companion) config — same /api/companion/config route, multi-companion shape ──
  getSharedConfig: httpGet<ICompanionSharedConfig, void>('/api/companion/config'),
  patchSharedConfig: httpPatch<ICompanionSharedConfig, ICompanionSharedConfigPatch>('/api/companion/config'),
  // ── Import / export (spec §4.8) ──
  exportMemory: httpPost<ICompanionExportResult, { dest_path: string; include_events: boolean }>('/api/companion/export/memory'),
  exportCompanion: httpPost<ICompanionExportResult, { companion_id: string; dest_path: string; knowledge_names?: string[] }>(
    (p) => `/api/companion/export/companions/${p.companion_id}`,
    (p) => ({ dest_path: p.dest_path, knowledge_names: p.knowledge_names ?? [] })
  ),
  /** Import a memory/companion bundle; the backend dispatches on manifest.kind. */
  importCompanionBundle: httpPost<Record<string, unknown>, { src_path: string }>('/api/companion/import'),
  onSuggestionCreated: wsEmitter<ICompanionSuggestion & { companion_id?: string }>('companion.suggestion-created'),
  /** A suggestion was accepted/dismissed (any surface) — drop the now-decided
   *  card live so other open surfaces don't keep a stale `new` snapshot that
   *  404s on the next decide. Payload carries the decided suggestion. */
  onSuggestionDecided: wsEmitter<ICompanionSuggestion>('companion.suggestion-decided'),
  onLearnStarted: wsEmitter<{ companion_id?: string }>('companion.learn-started'),
  onLearnFinished: wsEmitter<ICompanionLearnRun & { companion_id?: string }>('companion.learn-finished'),
  onMoodChanged: wsEmitter<{ mood: string; companion_id?: string }>('companion.mood-changed'),
  onConfigUpdated: wsEmitter<ICompanionConfigUpdatedEvent>('companion.config-updated'),
  onMemoryCreated: wsEmitter<ICompanionMemory>('companion.memory-created'),
  onMemoryUpdated: wsEmitter<ICompanionMemory>('companion.memory-updated'),
  onMemoryDeleted: wsEmitter<{ id: string }>('companion.memory-deleted'),
  onSkillDrafted: wsEmitter<ICompanionSkillEvent>('companion.skill-drafted'),
  onSkillLearned: wsEmitter<ICompanionSkillEvent>('companion.skill-learned'),
  onSkillArchived: wsEmitter<ICompanionSkillEvent>('companion.skill-archived'),
  onCompanionCreated: wsEmitter<ICompanionCreatedEvent>('companion.created'),
  onCompanionDeleted: wsEmitter<ICompanionDeletedEvent>('companion.deleted'),
};

// ==================== Browser-use credential secrets (P3-X2) ====================

/** A registered browser-use secret as returned to the client — metadata ONLY.
 *  The plaintext `value` is write-only (register) and is NEVER returned by any
 *  endpoint (it is encrypted into a per-pet, machine-bound vault). */
export interface ISecretListItem {
  /** The reference name used as `secret:NAME` in a browser type/set_value action. */
  name: string;
  /** The registrable domains (eTLD+1) this secret is bound to. These also feed the
   *  browser egress domain allowlist (shared per-pet config). */
  allowed_origins: string[];
}

/** Per-pet browser-use credential secret CRUD. `pet_id` is the companion id (the
 *  secret is scoped to that companion's browser). The value is write-only. */
export const browserSecret = {
  /** List a pet's registered secrets (name + bound origins; NEVER the value). */
  list: httpGet<ISecretListItem[], { pet_id: string }>((p) => `/api/browser-secrets/${encodeURIComponent(p.pet_id)}`),
  /** Register (or overwrite) a secret. `value` is encrypted into the vault and never echoed. */
  register: httpPost<void, { pet_id: string; name: string; value: string; allowed_origins: string[] }>(
    (p) => `/api/browser-secrets/${encodeURIComponent(p.pet_id)}`,
    (p) => ({ name: p.name, value: p.value, allowed_origins: p.allowed_origins })
  ),
  /** Remove a secret by name. */
  remove: httpDelete<void, { pet_id: string; name: string }>(
    (p) => `/api/browser-secrets/${encodeURIComponent(p.pet_id)}/${encodeURIComponent(p.name)}`
  ),
};

/** Phase 2b「登录我的浏览器」status returned by open/close/status. */
export interface IBrowserLoginStatus {
  /** Whether a visible login browser is currently open. */
  active: boolean;
  /** Outcome code: 'opened' | 'already_open' | 'closed' | 'not_open' | 'launch_failed:<err>'. */
  message?: string;
  /** Whether close() captured the login state into the encrypted vault backup. */
  saved: boolean;
}

/** 「登录我的浏览器」— open a visible browser bound to the shared profile so the user logs
 *  into their sites once; silent agent sessions then reuse the login. `source` mirrors
 *  agent.browserUse.source so the login browser uses the same binary. */
export const browserLogin = {
  /** Open the visible login window (idempotent while already open). */
  open: httpPost<IBrowserLoginStatus, { source: 'managed' | 'system' }>('/api/browser/login/open'),
  /** Close it (best-effort backs the login state up to the vault), returns final status. */
  close: httpPost<IBrowserLoginStatus, void>('/api/browser/login/close'),
  /** Poll whether a login window is currently open. */
  status: httpGet<IBrowserLoginStatus, void>('/api/browser/login/status'),
};

// ==================== Knowledge Base Platform (knowledge) ====================

/** One URL entry of a knowledge-base URL source. */
export interface IKnowledgeSourceEntry {
  url: string;
  title?: string;
  /**
   * P3-K3: fetch this URL through the rendering backend (a real headless
   * browser) instead of a plain HTTP GET — for JS-heavy SPAs whose content a
   * static fetch cannot see. Omitted/false ⇒ HTTP (backward compatible). When
   * no browser backend is wired the fetch gracefully falls back to HTTP.
   */
  rendered?: boolean;
}

/** 'snapshot' = fetched into snapshots/*.md at create/refresh time; 'live' = surfaced to agents as realtime sources. */
export type KnowledgeSourceMode = 'live' | 'snapshot';

/** URL source config of a base (wire shape: camelCase, `lastFetchedAt` epoch-ms). */
export interface IKnowledgeSource {
  /** Source kind discriminator; "url" for URL sources, "feishu"/… for connectors. */
  kind: string;
  mode: KnowledgeSourceMode;
  entries: IKnowledgeSourceEntry[];
  /** Last successful snapshot fetch (epoch ms); absent until the first fetch. */
  lastFetchedAt?: number;
  /** Connector-backed sources: reference to a stored connector credential. */
  credentialRef?: string;
  /** Connector-specific scope (e.g. Feishu `{ spaceId | space_id }`). Opaque to the core. */
  scope?: Record<string, unknown>;
  /** Connector sync state (cursor + interval + last outcome). */
  sync?: {
    intervalMinutes?: number;
    lastSyncAt?: number;
    cursor?: unknown;
    lastError?: string;
  };
}

/** Per-batch outcome of a URL-source fetch (create with snapshot source / refresh-source). */
export interface IKnowledgeSourceFetchSummary {
  fetched: number;
  failed: number;
  /** One "{url}: {error}" line per failed entry. */
  errors: string[];
  /** `extra.source.last_fetched_at` after the run; absent when nothing was ever fetched. */
  last_fetched_at?: number;
}

/** Result of POST /api/knowledge/bases/{id}/autogen (AI overview generation). */
export interface IKnowledgeAutogenOutcome {
  /** The (possibly clamped) description after the run. */
  description: string;
  description_updated: boolean;
  /** Whether this run wrote {root}/README.md. */
  readme_written: boolean;
  base: IKnowledgeBase;
}

/** A registered knowledge base — a directory of markdown documents. */
export interface IKnowledgeBase {
  id: string;
  name: string;
  description: string;
  root_path: string;
  /** true = directory provisioned under the backend data dir (purge allowed); false = user-referenced external dir. */
  managed: boolean;
  created_at: number;
  updated_at: number;
  file_count: number;
  total_size: number;
  /** false when the registered root directory no longer exists on disk. */
  root_exists: boolean;
  /** Create-response-only: per-entry fetch summary when the create carried a snapshot-mode URL source. */
  source_fetch?: IKnowledgeSourceFetchSummary;
  /** URL source config when the base has one (top-level on the wire). */
  source?: IKnowledgeSource;
  /** Tag keys attached to this base. */
  tags: string[];
  /** Source kind discriminator. */
  kind: 'blank' | 'local' | 'web' | 'feishu';
  /** Number of unreviewed staged inbox proposals. */
  pending_inbox: number;
}

/** A knowledge-base tag (for categorization / filtering). */
export interface IKnowledgeTag {
  key: string;
  label: string;
  color?: string;
  sortOrder: number;
}

/** A single search hit from cross-base semantic/keyword search. */
export interface IKnowledgeSearchHit {
  kb_id: string;
  kb_name: string;
  rel_path: string;
  heading: string;
  snippet: string;
  score: number;
}

export interface IKnowledgeFileEntry {
  rel_path: string;
  size: number;
  modified_at: number | null;
}

export interface IKnowledgeTreeEntry {
  name: string;
  rel_path: string;
  is_dir: boolean;
  is_file: boolean;
  size?: number;
  modified_at: number | null;
  children?: IKnowledgeTreeEntry[];
}

export interface IKnowledgeFileContent {
  rel_path: string;
  content: string;
  size: number;
  modified_at: number | null;
}

/** Per-target mount binding: which bases a session mounts + the write-back switch. */
export interface IKnowledgeBinding {
  enabled: boolean;
  writeback: boolean;
  /** 'staged' = writes confined to _inbox/{conversation_id}/ (conflict-free, default); 'direct' = agent may edit the base body. */
  writeback_mode: KnowledgeWritebackMode;
  /** Write-back disposition ("回写意识"), orthogonal to writeback_mode: 'conservative' (restrained, default) only writes clearly-useful knowledge; 'aggressive' captures anything plausibly relevant. */
  writeback_eagerness: KnowledgeWritebackEagerness;
  /**
   * Opt-in switch letting an unattended IM-channel (bot) session write back to
   * the base. Off by default; channel writes are ALWAYS staged into the review
   * inbox even when on. Set by the gateway/MCP path (the bot), not the in-app
   * control — but it MUST round-trip through `setBinding` so an in-app edit
   * (toggling bases / write-back) never silently clears it.
   */
  channel_write_enabled: boolean;
  kb_ids: string[];
}

export type KnowledgeWritebackMode = 'staged' | 'direct';

export type KnowledgeWritebackEagerness = 'conservative' | 'aggressive';

export type KnowledgeBindingKind = 'conversation' | 'terminal' | 'companion' | 'workpath';

/** One staged write-back proposal under `_inbox/{scope}/{rel_path}`. */
export interface IKnowledgeInboxEntry {
  /** First path segment under `_inbox/` — the session/conversation id that staged it. */
  scope: string;
  /** Base-relative path the proposal mirrors. */
  rel_path: string;
  size: number;
  modified_at: number | null;
}

/** A staged proposal vs. its current base version (for the review panel). */
export interface IKnowledgeInboxDiff {
  scope: string;
  rel_path: string;
  inbox_content: string;
  /** Current base document; absent when the proposal would create a new file. */
  base_content?: string | null;
  /** Server-computed unified diff, ready for the diff renderer. */
  unified_diff: string;
  /** true when there's no existing base document (a brand-new doc). */
  is_new: boolean;
}

/** A consumer (binding) of a base — a workspace/conversation/etc. that mounts it. */
export interface IKnowledgeConsumer {
  target_kind: KnowledgeBindingKind | string;
  target_id?: string | null;
  enabled: boolean;
}

/** Wire-safe connector credential summary (never carries the secret payload). */
export interface IConnectorCredentialSummary {
  id: string;
  /** Connector discriminator: "feishu", … */
  kind: string;
  name: string;
  createdAt: number;
}

/** Identity returned by a successful connector credential validation. */
export interface IConnectorIdentity {
  tenant_name?: string;
  scopes_available: string[];
}

// ---------------------------------------------------------------------------
// Public Companion (对外伙伴) — an enterprise-grade agent that safely serves
// STRANGERS (customer service): narrow-but-deep, Q&A + knowledge retrieval only,
// all dangerous capabilities off. A SEPARATE first-class domain from the desktop
// 伙伴 (companion): its own data, config, console, and audit trail — never mixed
// into the desktop-companion roster or the conversation sidebar.
//
// Routed to /api/public-agents (hand-defined against the pinned backend contract).
// ---------------------------------------------------------------------------

/** Which model a public companion answers strangers with (independent of desktop 伙伴). */
export interface IPublicAgentModel {
  provider_id: string;
  model: string;
  /** Optional display/override model id the backend may resolve; unset = use `model`. */
  use_model?: string;
}

/** One public companion — an enterprise customer-service agent. */
export interface IPublicAgent {
  id: string;
  /** Local auto-increment ordinal; null on backends that don't assign one. */
  seq: number | null;
  name: string;
  /** 开场白 / 欢迎语 shown when a stranger opens a conversation. */
  greeting: string;
  /** 语气规范 — tone/voice guidance the agent must follow. */
  tone: string;
  model: IPublicAgentModel;
  /** Platform knowledge-base ids this agent may retrieve from. */
  knowledge_base_ids: string[];
  /** 严格模式：only answer from bound knowledge bases (no free-form/general answers). */
  grounded_mode: boolean;
  /** 服务守则 — business scope / off-limits topics / compliance phrasing. */
  service_policy: string;
  /** How many days of audit entries to retain before auto-pruning. */
  audit_retention_days: number;
  /** Whether this agent is live (serving strangers) or paused. */
  enabled: boolean;
  /** Epoch milliseconds. */
  created_at: number;
}

/** Where a public-companion audit entry originated. */
export type PublicAgentAuditSurface = 'channel' | 'desktop' | 'remote';
/** What an audit-log row records: a served conversation turn, or an exposure/config change. */
export type PublicAgentAuditKind = 'turn' | 'exposure_change';

/** One reverse-chronological audit-log row for a public companion. */
export interface IPublicAgentAuditEntry {
  id: string;
  /** Epoch milliseconds. */
  at: number;
  surface: PublicAgentAuditSurface;
  /** IM platform when `surface === 'channel'` (e.g. "telegram"); null otherwise. */
  channel_platform: string | null;
  kind: PublicAgentAuditKind;
  detail: string;
}

/** A page of audit entries (newest-first) plus the cursor for the next page. */
export interface IPublicAgentAuditPage {
  entries: IPublicAgentAuditEntry[];
  /** `at` (epoch ms) to pass as `cursor` for the next page, or null when exhausted. */
  next_cursor: number | null;
}

/** Editable fields on a public companion (all optional — PATCH is a partial merge). */
export type IPublicAgentPatch = Partial<{
  name: string;
  greeting: string;
  tone: string;
  model: IPublicAgentModel;
  knowledge_base_ids: string[];
  grounded_mode: boolean;
  service_policy: string;
  audit_retention_days: number;
  enabled: boolean;
}>;

export const publicAgent = {
  /** Roster of public companions. */
  list: httpGet<IPublicAgent[], void>('/api/public-agents'),
  /** Create a new public companion (name only; everything else defaults server-side). */
  create: httpPost<IPublicAgent, { name: string }>('/api/public-agents'),
  /** One public companion by id. */
  get: httpGet<IPublicAgent, { id: string }>((p) => `/api/public-agents/${p.id}`),
  /** RFC 7396-style partial merge over the editable fields. Returns the updated agent. */
  patch: httpPatch<IPublicAgent, { id: string; patch: IPublicAgentPatch }>(
    (p) => `/api/public-agents/${p.id}`,
    (p) => p.patch
  ),
  /** Delete a public companion (204). */
  remove: httpDelete<void, { id: string }>((p) => `/api/public-agents/${p.id}`),
  /**
   * Reverse-chronological (newest-first) audit page. Cursor-paginated by `at` (epoch ms):
   * pass the previous page's `next_cursor` as `cursor` to load older entries. Degrades to
   * an empty page when the backend hasn't shipped the endpoint yet (404 silenced).
   */
  listAudit: httpGet<
    IPublicAgentAuditPage,
    { id: string; limit?: number; cursor?: number | null; q?: string; kind?: PublicAgentAuditKind; days?: number }
  >((p) => {
    const params = new URLSearchParams();
    params.set('limit', String(p.limit ?? 50));
    if (p.cursor != null) params.set('cursor', String(p.cursor));
    if (p.q) params.set('q', p.q);
    if (p.kind) params.set('kind', p.kind);
    if (p.days != null) params.set('days', String(p.days));
    return `/api/public-agents/${p.id}/audit?${params.toString()}`;
  }, { silentStatuses: [404] }),
  /** Purge audit entries older than N days. Returns how many days were cleared. */
  clearAudit: httpDelete<{ deleted_days: number }, { id: string; older_than_days: number }>(
    (p) => `/api/public-agents/${p.id}/audit?older_than_days=${p.older_than_days}`
  ),
};

/**
 * Client-side deadline for knowledge-base READ endpoints. The backend now
 * bounds each base's directory walk (≈6s) and parallelizes the list, so these
 * return quickly in normal operation; this is only a safety net so a wedged
 * NAS/offline root surfaces a legible timeout error instead of hanging the UI.
 * NOT applied to knowledge mutations (autogen / snapshot fetch / import) — those
 * legitimately take minutes.
 */
const KB_READ_TIMEOUT_MS = 30_000;

export const knowledge = {
  listBases: httpGet<IKnowledgeBase[], void>('/api/knowledge/bases', { timeoutMs: KB_READ_TIMEOUT_MS }),
  createBase: httpPost<
    IKnowledgeBase,
    {
      name: string;
      description?: string;
      root_path?: string;
      /** Optional URL source; mode 'snapshot' fetches every entry before the response returns (slow — see source_fetch). */
      source?: { kind: string; mode: KnowledgeSourceMode; entries?: IKnowledgeSourceEntry[]; credential_ref?: string; scope?: Record<string, unknown>; sync?: { interval_minutes?: number } };
      /** Tag keys to assign at creation time. */
      tags?: string[];
    }
  >('/api/knowledge/bases'),
  getBase: httpGet<IKnowledgeBase, { id: string }>((p) => `/api/knowledge/bases/${p.id}`, { timeoutMs: KB_READ_TIMEOUT_MS }),
  updateBase: httpPut<IKnowledgeBase, { id: string; name?: string; description?: string; tags?: string[] }>(
    (p) => `/api/knowledge/bases/${p.id}`,
    (p) => ({ name: p.name, description: p.description, tags: p.tags })
  ),
  /** AI overview generation (description + README.md). Slow (LLM round-trip, 30s+); 409 when no AI provider is configured. */
  autogenBase: httpPost<IKnowledgeAutogenOutcome, { id: string; overwrite_readme?: boolean; provider_id?: string; model?: string }>(
    (p) => `/api/knowledge/bases/${p.id}/autogen`,
    (p) => ({ overwrite_readme: p.overwrite_readme ?? false, provider_id: p.provider_id, model: p.model })
  ),
  /**
   * Stateless AI description draft from a local directory (no base required — used by the create form).
   * Slow (LLM round-trip); 409 when no AI completer is configured, 400 when the path is invalid.
   */
  generateDescription: httpPost<{ description: string }, { name?: string; root_path: string; provider_id?: string; model?: string }>(
    '/api/knowledge/description/generate',
    (p) => ({ name: p.name, root_path: p.root_path, provider_id: p.provider_id, model: p.model })
  ),
  /** Stateless AI polish of a hand-written description draft. Slow (LLM round-trip); 409 when no AI completer is configured. */
  polishDescription: httpPost<{ description: string }, { name?: string; draft: string; provider_id?: string; model?: string }>(
    '/api/knowledge/description/polish',
    (p) => ({ name: p.name, draft: p.draft, provider_id: p.provider_id, model: p.model })
  ),
  /** Re-fetch every URL-source entry into snapshots/ (works for live-mode sources too); 400 when the base has no source. */
  refreshSource: httpPost<IKnowledgeSourceFetchSummary, { id: string }>(
    (p) => `/api/knowledge/bases/${p.id}/refresh-source`,
    () => undefined
  ),
  /** Attach / replace / clear a base's source config (e.g. wire a Feishu connector onto an existing base). */
  setSource: httpPut<IKnowledgeBase, { id: string; source: IKnowledgeSource | null }>(
    (p) => `/api/knowledge/bases/${p.id}/source`,
    (p) => ({ source: p.source })
  ),
  deleteBase: httpDelete<void, { id: string; purge?: boolean }>(
    (p) => `/api/knowledge/bases/${p.id}${p.purge ? '?purge=true' : ''}`
  ),
  listFiles: httpGet<IKnowledgeFileEntry[], { id: string }>((p) => `/api/knowledge/bases/${p.id}/files`, { timeoutMs: KB_READ_TIMEOUT_MS }),
  listTree: httpGet<IKnowledgeTreeEntry[], { id: string; path?: string }>(
    (p) => `/api/knowledge/bases/${p.id}/tree${p.path ? `?path=${encodeURIComponent(p.path)}` : ''}`,
    { timeoutMs: KB_READ_TIMEOUT_MS }
  ),
  createFolder: httpPost<IKnowledgeTreeEntry, { id: string; path: string }>(
    (p) => `/api/knowledge/bases/${p.id}/folder`,
    (p) => ({ path: p.path })
  ),
  deleteFolder: httpDelete<void, { id: string; path: string }>(
    (p) => `/api/knowledge/bases/${p.id}/folder?path=${encodeURIComponent(p.path)}`
  ),
  renameTreeEntry: httpPost<IKnowledgeTreeEntry, { id: string; path: string; newName: string }>(
    (p) => `/api/knowledge/bases/${p.id}/tree/rename`,
    (p) => ({ path: p.path, new_name: p.newName })
  ),
  readFile: httpGet<IKnowledgeFileContent, { id: string; path: string }>(
    (p) => `/api/knowledge/bases/${p.id}/file?path=${encodeURIComponent(p.path)}`,
    { timeoutMs: KB_READ_TIMEOUT_MS }
  ),
  writeFile: httpPut<void, { id: string; path: string; content: string }>(
    (p) => `/api/knowledge/bases/${p.id}/file`,
    (p) => ({ path: p.path, content: p.content })
  ),
  deleteFile: httpDelete<void, { id: string; path: string }>(
    (p) => `/api/knowledge/bases/${p.id}/file?path=${encodeURIComponent(p.path)}`
  ),
  getBinding: httpGet<IKnowledgeBinding, { kind: KnowledgeBindingKind; target_id: string }>(
    // workpath target_id is a filesystem path containing `/`; encode so it
    // stays a single path segment (`/`→`%2F`). conversation/terminal ids have
    // no `/`, so their encoded form is byte-identical — no regression.
    (p) => `/api/knowledge/binding/${p.kind}/${encodeURIComponent(p.target_id)}`
  ),
  setBinding: httpPost<IKnowledgeBinding, { kind: KnowledgeBindingKind; target_id: string } & IKnowledgeBinding>(
    (p) => `/api/knowledge/binding/${p.kind}/${encodeURIComponent(p.target_id)}`,
    // Forward EVERY binding field by destructuring off the routing params only.
    // A hand-maintained whitelist here silently dropped writeback_mode,
    // writeback_eagerness and channel_write_enabled in turn (the backend POST
    // is a full replace), so any new IKnowledgeBinding field stays in the body
    // automatically.
    (p) => {
      const { kind: _kind, target_id: _target_id, ...body } = p;
      return body;
    }
  ),
  // ── Import / export (spec 2026-06-11 §4.8: zip with manifest.kind="knowledge-base") ──
  exportBase: httpPost<{ dest_path: string }, { id: string; dest_path: string }>(
    (p) => `/api/knowledge/bases/${p.id}/export`,
    (p) => ({ dest_path: p.dest_path })
  ),
  /** Import a knowledge-base bundle — a new managed base is provisioned (name conflicts get a "(2)" suffix). */
  importBase: httpPost<IKnowledgeBase, { src_path: string }>('/api/knowledge/bases/import'),
  // ── P4 inbox review (staged write-back proposals) ──
  /** List staged write-back proposals under `_inbox/` (group by `scope` client-side). */
  listInbox: httpGet<IKnowledgeInboxEntry[], { id: string }>((p) => `/api/knowledge/bases/${p.id}/inbox`, { timeoutMs: KB_READ_TIMEOUT_MS }),
  /** Server-computed unified diff of one proposal vs. the current base document. */
  getInboxDiff: httpGet<IKnowledgeInboxDiff, { id: string; scope: string; path: string }>(
    (p) =>
      `/api/knowledge/bases/${p.id}/inbox/diff?scope=${encodeURIComponent(p.scope)}&path=${encodeURIComponent(p.path)}`
  ),
  /** Accept a proposal: overwrite the base document and drop the staged copy. */
  mergeInbox: httpPost<{ merged_path: string }, { id: string; scope: string; path: string }>(
    (p) => `/api/knowledge/bases/${p.id}/inbox/merge`,
    (p) => ({ scope: p.scope, path: p.path })
  ),
  /** Discard a proposal (delete the staged copy, base untouched). */
  discardInbox: httpPost<void, { id: string; scope: string; path: string }>(
    (p) => `/api/knowledge/bases/${p.id}/inbox/discard`,
    (p) => ({ scope: p.scope, path: p.path })
  ),
  /** Bindings currently mounting this base (enabled AND disabled). */
  listConsumers: httpGet<IKnowledgeConsumer[], { id: string }>((p) => `/api/knowledge/bases/${p.id}/consumers`, { timeoutMs: KB_READ_TIMEOUT_MS }),
  /** Total unreviewed staged proposals across all bases (sidebar red-dot signal). */
  pendingInboxCount: httpGet<number, void>('/api/knowledge/inbox/pending-count', { timeoutMs: KB_READ_TIMEOUT_MS }),
  // ── P3 source connectors (Feishu, …) ──
  /** Pull a connector-backed base's remote docs into snapshots/ (distinct from refresh-source). */
  syncSource: httpPost<IKnowledgeSourceFetchSummary, { id: string }>(
    (p) => `/api/knowledge/bases/${p.id}/sync`,
    () => undefined
  ),
  listCredentials: httpGet<IConnectorCredentialSummary[], void>('/api/knowledge/connectors/credentials'),
  /** Validate then store a connector credential (probed before encryption; returns a secret-free summary). */
  createCredential: httpPost<IConnectorCredentialSummary, { kind: string; name: string; payload: Record<string, unknown> }>(
    '/api/knowledge/connectors/credentials',
    (p) => ({ kind: p.kind, name: p.name, payload: p.payload })
  ),
  deleteCredential: httpDelete<void, { id: string }>((p) => `/api/knowledge/connectors/credentials/${p.id}`),
  /** Re-probe a stored credential against its remote (the "test connection" action). */
  testCredential: httpPost<IConnectorIdentity, { id: string }>(
    (p) => `/api/knowledge/connectors/credentials/${p.id}/test`,
    () => undefined
  ),
  onBaseCreated: wsEmitter<IKnowledgeBase>('knowledge.base-created'),
  onBaseUpdated: wsEmitter<IKnowledgeBase>('knowledge.base-updated'),
  onBaseDeleted: wsEmitter<{ id: string }>('knowledge.base-deleted'),
  onBindingChanged: wsEmitter<{ target_kind: string; target_id: string } & IKnowledgeBinding>(
    'knowledge.binding-changed'
  ),
  /** A tag was created/renamed/recolored/reordered/deleted — re-list tags. */
  onTagChanged: wsEmitter<Record<string, never>>('knowledge.tag-changed'),
  // ── Tags (categorization / filtering) ──
  listTags: httpGet<IKnowledgeTag[], void>('/api/knowledge/tags'),
  createTag: httpPost<IKnowledgeTag, { label: string; color?: string }>(
    '/api/knowledge/tags',
    (p) => ({ label: p.label, color: p.color })
  ),
  updateTag: httpPut<void, { key: string; label?: string; color?: string; sortOrder?: number }>(
    (p) => `/api/knowledge/tags/${p.key}`,
    (p) => ({ label: p.label, color: p.color, sortOrder: p.sortOrder })
  ),
  deleteTag: httpDelete<void, { key: string }>((p) => `/api/knowledge/tags/${p.key}`),
  // ── Cross-base search ──
  search: httpPost<IKnowledgeSearchHit[], { kbIds: string[]; query: string; limit?: number }>(
    '/api/knowledge/search',
    (p) => ({ kbIds: p.kbIds, query: p.query, limit: p.limit })
  ),
  // ── Batch inbox operations ──
  mergeAllInbox: httpPost<void, { kbId: string; scope?: string }>(
    '/api/knowledge/inbox/merge-all',
    (p) => ({ kbId: p.kbId, scope: p.scope })
  ),
  discardAllInbox: httpPost<void, { kbId: string; scope?: string }>(
    '/api/knowledge/inbox/discard-all',
    (p) => ({ kbId: p.kbId, scope: p.scope })
  ),
};
