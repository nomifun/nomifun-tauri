/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import type { IConversationArtifact } from '@/common/adapter/ipcBridge';
import type { IMessageAcpToolCall, IMessageToolCall, IMessageToolGroup, TMessage } from '@/common/chat/chatLib';
import { normalizeToolMessages } from '@/common/chat/normalizeToolCall';
import { useConversationContextSafe } from '@/renderer/hooks/context/ConversationContext';
import { usePreviewLauncher } from '@/renderer/hooks/file/usePreviewLauncher';
import { extractContentFromDiff } from '@/renderer/utils/file/diffUtils';
import { getFileTypeInfo } from '@/renderer/utils/file/fileType';
import MessageAcpPermission from '@renderer/pages/conversation/Messages/acp/MessageAcpPermission';
import { Right } from '@icon-park/react';
import classNames from 'classnames';
import React, { useCallback, useMemo, useState } from 'react';
import { useTranslation } from 'react-i18next';
import type { FileChangeInfo } from '../MessageFileChanges';
import { isContextCompressionTip } from '../processTipModel';
import { formatFileTargetPreview, formatWorkspaceFileTarget } from '../processFileTargetLabel';
import {
  isFileReceiptRow,
  shouldShowFileListDetail,
  shouldShowToolRowDetail,
} from '../processTraceDisplayModel';
import type { TurnDisclosureProcessState } from '../turnDisclosureModel';
import { getProcessItemState, mergeProcessStates } from '../turnProcessState';
import MessageThinking from './MessageThinking';
import MessagePermission from './MessagePermission';
import { buildToolReceiptDetailRows, type ToolReceiptDetailRow } from './toolGroupSummaryModel';

type ToolProcessMessage = IMessageToolGroup | IMessageAcpToolCall | IMessageToolCall;

export type ProcessTraceRenderableItem =
  | TMessage
  | {
      type: 'file_summary';
      id: string;
      msg_id?: string;
      diffs: FileChangeInfo[];
      sourceMessageIds: string[];
      created_at: number;
    }
  | {
      type: 'tool_summary';
      id: string;
      msg_id?: string;
      messages: ToolProcessMessage[];
      sourceMessageIds: string[];
      created_at: number;
    }
  | {
      type: 'artifact';
      id: string;
      artifact: IConversationArtifact;
      created_at: number;
    };

type TranslationFn = ReturnType<typeof useTranslation>['t'];

type ProcessTraceVariant = 'list' | 'receipt';

export type ProcessTraceItemExpansionControls = {
  expanded?: boolean;
  onExpandedChange?: (expanded: boolean) => void;
};

type ProcessTraceRow = {
  key: string;
  label: string;
  title?: string;
  state: TurnDisclosureProcessState;
  onClick?: () => void;
};

const defaultToolSummaryByState: Record<TurnDisclosureProcessState, string> = {
  completed: 'Ran {{target}}',
  running: 'Running {{target}}',
  waiting: 'Waiting to confirm {{target}}',
  failed: 'Failed {{target}}',
  canceled: 'Canceled {{target}}',
};

const compactReceiptText = (value: unknown, fallback: string): string => {
  if (typeof value !== 'string') return fallback;
  const compacted = value.replace(/\s+/g, ' ').trim();
  return compacted || fallback;
};

const joinCompactText = (parts: Array<string | undefined>): string => parts.filter(Boolean).join(' ');

const getToolReceiptDetailDisplayTarget = (row: ToolReceiptDetailRow, workspaceRoots: string[]): string | undefined => {
  if (!row.target) return undefined;
  if (row.action !== 'read_files' && row.action !== 'edit_files') return row.target;
  return formatWorkspaceFileTarget(row.target, { workspaceRoots }).label;
};

const formatToolReceiptDetailLabel = (
  row: ToolReceiptDetailRow,
  t: TranslationFn,
  workspaceRoots: string[]
): string => {
  const displayTarget = getToolReceiptDetailDisplayTarget(row, workspaceRoots);

  if ((row.state === 'failed' || row.state === 'canceled') && displayTarget) {
    return t(`messages.toolSummary.${row.state}`, {
      target: displayTarget,
      defaultValue: defaultToolSummaryByState[row.state],
    });
  }

  if (row.action === 'run_commands' && row.target) {
    return t(`messages.toolSummary.${row.state}`, {
      target: row.target,
      defaultValue: defaultToolSummaryByState[row.state],
    });
  }

  if (row.action === 'search_code') {
    return row.target
      ? t('messages.processReceipt.searchedTarget', {
          target: row.target,
          defaultValue: 'Searched {{target}}',
        })
      : t('messages.processReceipt.searchedCode', { defaultValue: 'Searched code' });
  }

  if (row.action === 'list_files') {
    return row.target
      ? t('messages.processReceipt.listedTarget', {
          target: row.target,
          defaultValue: 'Listed {{target}}',
        })
      : t('messages.processReceipt.listedFiles', { defaultValue: 'Listed files' });
  }

  if (row.action === 'load_tools') {
    return row.target
      ? t('messages.processReceipt.loadedTarget', {
          target: row.target,
          defaultValue: 'Loaded {{target}}',
        })
      : t('messages.processReceipt.loadedTools', {
          count: 1,
          defaultValue: 'Loaded {{count}} tools',
        });
  }

  if (row.action === 'read_files' && displayTarget) {
    return compactReceiptText(
      t('messages.processReceipt.fileRead', {
        target: displayTarget,
        defaultValue: 'Read {{target}}',
      }),
      displayTarget
    );
  }

  if (row.action === 'edit_files' && displayTarget) {
    return compactReceiptText(
      t('messages.processReceipt.fileChanged', {
        target: displayTarget,
        stats: '',
        defaultValue: 'Edited {{target}}',
      }),
      displayTarget
    );
  }

  return joinCompactText([row.title, displayTarget]);
};

const formatFileChangeStats = (file: FileChangeInfo): string =>
  joinCompactText([
    file.insertions > 0 ? `+${file.insertions}` : undefined,
    file.deletions > 0 ? `-${file.deletions}` : undefined,
  ]);

const formatTargetPreview = (targets: string[], workspaceRoots: string[]): string =>
  formatFileTargetPreview(targets, { workspaceRoots });

const getToolFileListTargets = (rows: ToolReceiptDetailRow[]): string[] =>
  Array.from(new Set(rows.map((row) => row.target).filter((target): target is string => Boolean(target))));

const formatToolFileListLabel = (
  rows: ToolReceiptDetailRow[],
  t: TranslationFn,
  workspaceRoots: string[]
): string => {
  const targets = getToolFileListTargets(rows);
  const targetPreview = formatTargetPreview(targets, workspaceRoots);
  const hasReadRows = rows.some((row) => row.action === 'read_files');
  const hasEditRows = rows.some((row) => row.action === 'edit_files');

  if (hasEditRows && !hasReadRows) {
    return t('messages.processReceipt.fileEditTargets', {
      count: targets.length,
      target: targetPreview,
      defaultValue: 'Edited {{count}} files: {{target}}',
    });
  }

  if (hasReadRows && !hasEditRows) {
    return t('messages.processReceipt.readTargets', {
      count: targets.length,
      target: targetPreview,
      defaultValue: 'Read {{count}} files: {{target}}',
    });
  }

  return t('messages.processReceipt.fileTargets', {
    count: targets.length,
    target: targetPreview,
    defaultValue: 'Handled {{count}} files: {{target}}',
  });
};

const ToolFileListDetail: React.FC<{
  rows: ToolReceiptDetailRow[];
  workspaceRoots: string[];
  showLabel?: boolean;
}> = ({
  rows,
  workspaceRoots,
  showLabel = true,
}) => {
  const { t } = useTranslation();
  const targets = getToolFileListTargets(rows);
  if (!targets.length) return null;

  const label = formatToolFileListLabel(rows, t, workspaceRoots);

  return (
    <div className='turn-process-trace-detail'>
      {showLabel && <div className='turn-process-trace-detail__label'>{label}</div>}
      <ul className='turn-process-trace-file-list'>
        {targets.map((target) => {
          const display = formatWorkspaceFileTarget(target, { workspaceRoots });
          return (
            <li key={target} className='turn-process-trace-file-list__item' title={display.title}>
              {display.label}
            </li>
          );
        })}
      </ul>
    </div>
  );
};

const ToolFileGroupTraceRow: React.FC<{ rows: ToolReceiptDetailRow[]; workspaceRoots: string[] }> = ({
  rows,
  workspaceRoots,
}) => {
  const { t } = useTranslation();
  const [expanded, setExpanded] = useState(false);
  const targets = getToolFileListTargets(rows);
  if (!targets.length) return null;

  const label = formatToolFileListLabel(rows, t, workspaceRoots);
  const state = mergeProcessStates(rows.map((row) => row.state));

  return (
    <div className='turn-process-trace-tool'>
      <button
        type='button'
        className={classNames(
          'turn-process-trace__row',
          'turn-process-trace-tool__toggle',
          `turn-process-trace__row--${state}`
        )}
        onClick={() => setExpanded((value) => !value)}
        aria-expanded={expanded}
      >
        <span className='turn-process-trace__text' title={targets.join('\n')}>
          {label}
        </span>
        <Right
          theme='outline'
          size='12'
          className={classNames('turn-process-trace-tool__arrow', expanded && 'turn-process-trace-tool__arrow--open')}
        />
      </button>
      {expanded && <ToolFileListDetail rows={rows} workspaceRoots={workspaceRoots} showLabel={false} />}
    </div>
  );
};

const ToolTraceDetailSection: React.FC<{ label: string; value?: string }> = ({ label, value }) => {
  if (!value) return null;
  return (
    <div className='turn-process-trace-detail__section'>
      <div className='turn-process-trace-detail__label'>{label}</div>
      <pre className='turn-process-trace-detail__content'>{value}</pre>
    </div>
  );
};

const ToolTraceDetail: React.FC<{ row: ToolReceiptDetailRow; workspaceRoots: string[] }> = ({ row, workspaceRoots }) => {
  const { t } = useTranslation();
  if (isFileReceiptRow(row) && row.state !== 'failed' && row.state !== 'canceled') {
    return <ToolFileListDetail rows={[row]} workspaceRoots={workspaceRoots} />;
  }

  const command = row.action === 'run_commands' ? row.target : undefined;
  const input = row.input && row.input !== command ? row.input : undefined;

  return (
    <div className='turn-process-trace-detail'>
      <ToolTraceDetailSection
        label={t('messages.command', { defaultValue: 'Command:' })}
        value={command}
      />
      <ToolTraceDetailSection
        label={t('messages.toolDetailInput', { defaultValue: 'Input' })}
        value={input}
      />
      <ToolTraceDetailSection
        label={t('messages.toolDetailOutput', { defaultValue: 'Output' })}
        value={row.output}
      />
      {row.truncated && (
        <div className='turn-process-trace-detail__label'>
          {t('messages.toolDetailLoadFailed', { defaultValue: 'Full output was truncated' })}
        </div>
      )}
    </div>
  );
};

const ToolTraceRow: React.FC<{
  row: ToolReceiptDetailRow;
  label: string;
  workspaceRoots: string[];
  fileRowCount?: number;
}> = ({
  row,
  label,
  workspaceRoots,
  fileRowCount,
}) => {
  const [expanded, setExpanded] = useState(false);
  const hasDetail = shouldShowToolRowDetail(row, { fileRowCount });
  const rowClassName = classNames(
    'turn-process-trace__row',
    'turn-process-trace-tool__toggle',
    `turn-process-trace__row--${row.state}`
  );

  if (!hasDetail) {
    return (
      <div className='turn-process-trace-tool'>
        <div className={classNames('turn-process-trace__row', `turn-process-trace__row--${row.state}`)}>
          <span className='turn-process-trace__text' title={row.target ?? label}>
            {label}
          </span>
        </div>
      </div>
    );
  }

  return (
    <div className='turn-process-trace-tool'>
      <button
        type='button'
        className={rowClassName}
        onClick={() => setExpanded((value) => !value)}
        aria-expanded={expanded}
      >
        <span className='turn-process-trace__text' title={row.target ?? label}>
          {label}
        </span>
        <Right
          theme='outline'
          size='12'
          className={classNames('turn-process-trace-tool__arrow', expanded && 'turn-process-trace-tool__arrow--open')}
        />
      </button>
      {expanded && <ToolTraceDetail row={row} workspaceRoots={workspaceRoots} />}
    </div>
  );
};

const ProcessTraceRows: React.FC<{ rows: ProcessTraceRow[] }> = ({ rows }) => {
  if (!rows.length) return null;

  return (
    <div className='turn-process-trace'>
      {rows.map((row) => {
        const className = classNames('turn-process-trace__row', `turn-process-trace__row--${row.state}`);
        const text = (
          <span className='turn-process-trace__text' title={row.title ?? row.label}>
            {row.label}
          </span>
        );

        if (row.onClick) {
          return (
            <button key={row.key} type='button' className={className} onClick={row.onClick}>
              {text}
            </button>
          );
        }

        return (
          <div key={row.key} className={className}>
            {text}
          </div>
        );
      })}
    </div>
  );
};

const ToolProcessTraceRows: React.FC<{
  messages: ToolProcessMessage[];
  variant?: ProcessTraceVariant;
  workspaceRoots: string[];
  stateOverride?: TurnDisclosureProcessState;
}> = ({
  messages,
  variant = 'list',
  workspaceRoots,
  stateOverride,
}) => {
  const { t } = useTranslation();
  const tools = useMemo(() => normalizeToolMessages(messages), [messages]);
  const rows = useMemo(
    () =>
      buildToolReceiptDetailRows(tools).map((row) => {
        const effectiveRow = stateOverride ? { ...row, state: stateOverride } : row;
        return {
          row: effectiveRow,
          label: formatToolReceiptDetailLabel(effectiveRow, t, workspaceRoots),
        };
      }),
    [stateOverride, t, tools, workspaceRoots]
  );

  const fileRows = rows.filter(({ row }) => isFileReceiptRow(row)).map(({ row }) => row);
  const nonFileRows = rows.filter(({ row }) => !isFileReceiptRow(row));

  if (shouldShowFileListDetail(fileRows)) {
    return (
      <div className='turn-process-trace'>
        <ToolFileGroupTraceRow rows={fileRows} workspaceRoots={workspaceRoots} />
        {nonFileRows.map(({ row, label }) => (
          <ToolTraceRow key={row.key} row={row} label={label} workspaceRoots={workspaceRoots} />
        ))}
      </div>
    );
  }

  if (variant === 'receipt' && rows.length === 1 && shouldShowToolRowDetail(rows[0].row, { fileRowCount: fileRows.length })) {
    return <ToolTraceDetail row={rows[0].row} workspaceRoots={workspaceRoots} />;
  }

  return (
    <div className='turn-process-trace'>
      {rows.map(({ row, label }) => (
        <ToolTraceRow
          key={row.key}
          row={row}
          label={label}
          workspaceRoots={workspaceRoots}
          fileRowCount={fileRows.length}
        />
      ))}
    </div>
  );
};

const FileProcessTraceRows: React.FC<{ diffs: FileChangeInfo[]; workspaceRoots: string[] }> = ({
  diffs,
  workspaceRoots,
}) => {
  const { t } = useTranslation();
  const { launchPreview } = usePreviewLauncher();
  const files = useMemo(() => Array.from(new Map(diffs.map((file) => [file.fullPath, file])).values()), [diffs]);

  const openFile = useCallback(
    (file: FileChangeInfo) => {
      const { contentType, editable, language } = getFileTypeInfo(file.file_name);
      void launchPreview({
        relativePath: file.fullPath,
        file_name: file.file_name,
        contentType,
        editable,
        language,
        fallbackContent: editable ? extractContentFromDiff(file.diff) : undefined,
        diffContent: file.diff,
      });
    },
    [launchPreview]
  );

  const rows = useMemo<ProcessTraceRow[]>(
    () =>
      files.map((file) => {
        const stats = formatFileChangeStats(file);
        const target = formatWorkspaceFileTarget(file.fullPath, { workspaceRoots });
        return {
          key: file.fullPath,
          state: 'completed',
          title: file.fullPath,
          label: compactReceiptText(
            t('messages.processReceipt.fileChanged', {
              target: target.label,
              stats,
              defaultValue: 'Edited {{target}} {{stats}}',
            }),
            target.label
          ),
          onClick: () => openFile(file),
        };
      }),
    [files, openFile, t, workspaceRoots]
  );

  return <ProcessTraceRows rows={rows} />;
};

const getUnhandledMessageType = (_message: never): string => 'unknown';

const ProcessTraceItem: React.FC<{
  item: ProcessTraceRenderableItem;
  variant?: ProcessTraceVariant;
  workspaceRoots?: string[];
  stateOverride?: TurnDisclosureProcessState;
  thinkingExpansion?: ProcessTraceItemExpansionControls;
}> = ({
  item,
  variant = 'list',
  workspaceRoots,
  stateOverride,
  thinkingExpansion,
}) => {
  const { t } = useTranslation();
  const conversationContext = useConversationContextSafe();
  const state = stateOverride ?? getProcessItemState(item);
  const resolvedWorkspaceRoots = useMemo(
    () =>
      workspaceRoots && workspaceRoots.length
        ? workspaceRoots
        : conversationContext?.workspace
          ? [conversationContext.workspace]
          : [],
    [conversationContext?.workspace, workspaceRoots]
  );

  if ('type' in item && item.type === 'artifact') {
    const target =
      item.artifact.kind === 'cron_trigger' ? item.artifact.payload.cron_job_name : item.artifact.payload.name;
    return (
      <ProcessTraceRows
        rows={[
          {
            key: item.id,
            state,
            label: t('messages.processReceipt.status', { target, defaultValue: '{{target}}' }),
          },
        ]}
      />
    );
  }

  if ('type' in item && item.type === 'file_summary') {
    return <FileProcessTraceRows diffs={item.diffs} workspaceRoots={resolvedWorkspaceRoots} />;
  }

  if ('type' in item && item.type === 'tool_summary') {
    return (
      <ToolProcessTraceRows
        messages={item.messages}
        variant={variant}
        workspaceRoots={resolvedWorkspaceRoots}
        stateOverride={stateOverride}
      />
    );
  }

  switch (item.type) {
    case 'text':
      return (
        <div className='turn-process-trace'>
          <div className='turn-process-trace__paragraph'>{item.content.content}</div>
        </div>
      );
    case 'thinking':
      return (
        <MessageThinking
          message={item}
          variant='process'
          expanded={thinkingExpansion?.expanded}
          onExpandedChange={thinkingExpansion?.onExpandedChange}
        />
      );
    case 'tips':
      if (isContextCompressionTip(item)) {
        return (
          <ProcessTraceRows
            rows={[
              {
                key: item.id,
                state,
                label: t('messages.processReceipt.contextCompressed', { defaultValue: 'Context compressed' }),
              },
            ]}
          />
        );
      }
      return (
        <ProcessTraceRows
          rows={[
            {
              key: item.id,
              state,
              label: compactReceiptText(
                item.content.content,
                t('messages.processReceipt.status', {
                  target: t('messages.processing'),
                  defaultValue: '{{target}}',
                })
              ),
            },
          ]}
        />
      );
    case 'tool_call':
    case 'tool_group':
    case 'acp_tool_call':
      return (
        <ToolProcessTraceRows
          messages={[item]}
          variant={variant}
          workspaceRoots={resolvedWorkspaceRoots}
          stateOverride={stateOverride}
        />
      );
    case 'agent_status':
      return (
        <ProcessTraceRows
          rows={[
            {
              key: item.id,
              state,
              label:
                item.content.status === 'preparing'
                  ? t('messages.processReceipt.preparingAction', {
                      defaultValue: 'Preparing next action',
                    })
                  : item.content.status === 'prepared'
                    ? t('messages.processReceipt.preparedAction', {
                        defaultValue: 'Prepared next action',
                      })
                  : state === 'failed'
                  ? t('messages.processReceipt.agentFailed', {
                      target: item.content.agent_name || item.content.backend,
                      defaultValue: '{{target}} failed',
                    })
                  : t('messages.processReceipt.agentConnecting', {
                      target: item.content.agent_name || item.content.backend,
                      defaultValue: 'Connecting {{target}}',
                    }),
            },
          ]}
        />
      );
    case 'permission':
      if (state === 'waiting') return <MessagePermission message={item} />;
      return (
        <ProcessTraceRows
          rows={[
            {
              key: item.id,
              state,
              label: t('messages.processReceipt.waitingPermission', {
                target: compactReceiptText(
                  item.content.title || item.content.description,
                  t('messages.permissionRequest')
                ),
                defaultValue: 'Waiting to confirm {{target}}',
              }),
            },
          ]}
        />
      );
    case 'acp_permission':
      if (state === 'waiting') return <MessageAcpPermission message={item} />;
      return (
        <ProcessTraceRows
          rows={[
            {
              key: item.id,
              state,
              label: t('messages.processReceipt.waitingPermission', {
                target: compactReceiptText(
                  item.content.tool_call?.title ||
                    item.content.tool_call?.raw_input?.command ||
                    item.content.tool_call?.raw_input?.description,
                  t('messages.permissionRequest')
                ),
                defaultValue: 'Waiting to confirm {{target}}',
              }),
            },
          ]}
        />
      );
    case 'plan':
    case 'available_commands':
      return null;
    default:
      return <div>{t('messages.unknownMessageType', { type: getUnhandledMessageType(item) })}</div>;
  }
};

export default ProcessTraceItem;
