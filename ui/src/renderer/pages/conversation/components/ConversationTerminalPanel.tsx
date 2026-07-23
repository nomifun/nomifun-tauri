/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import { ipcBridge } from '@/common';
import type { ITerminalSession } from '@/common/adapter/ipcBridge';
import type { ConversationId, TerminalId } from '@/common/types/ids';
import { decodeBase64ToString, createStreamingDecoder } from '@/renderer/pages/terminal/terminalEncoding';
import { Button, Empty, Popconfirm, Spin } from '@arco-design/web-react';
import { DeleteOne, Down, Power, Right, Terminal } from '@icon-park/react';
import React, { useCallback, useEffect, useMemo, useRef, useState } from 'react';
import { useTranslation } from 'react-i18next';

const OUTPUT_LIMIT = 32_768;

const stripTerminalControls = (value: string): string =>
  value
    // CSI cursor/style/control sequences.
    .replace(/\u001b\[[0-?]*[ -/]*[@-~]/g, '')
    // OSC title/hyperlink sequences terminated by BEL or ST.
    .replace(/\u001b\][^\u0007]*(?:\u0007|\u001b\\)/g, '')
    .replace(/\r(?!\n)/g, '\n');

const trimOutput = (value: string): string => {
  const stripped = stripTerminalControls(value);
  return stripped.length > OUTPUT_LIMIT
    ? stripped.slice(stripped.length - OUTPUT_LIMIT)
    : stripped;
};

/**
 * Render a passive terminal tail. It deliberately never mounts XtermView:
 * viewing an agent-owned terminal in the narrow side panel must not resize its
 * PTY, send input, or steal focus from the conversation.
 */
const TerminalOutputTail: React.FC<{ session: ITerminalSession }> = ({ session }) => {
  const { t } = useTranslation();
  const [output, setOutput] = useState('');
  const [loading, setLoading] = useState(true);
  const decoderRef = useRef(createStreamingDecoder());
  const snapshotGenerationRef = useRef(0);
  const snapshotLoadingRef = useRef(false);
  const pendingChunksRef = useRef<string[]>([]);

  const loadSnapshot = useCallback(async () => {
    const generation = ++snapshotGenerationRef.current;
    snapshotLoadingRef.current = true;
    pendingChunksRef.current = [];
    decoderRef.current = createStreamingDecoder();
    setLoading(true);
    try {
      const detail = await ipcBridge.terminal.get.invoke({ terminal_id: session.terminal_id });
      if (generation !== snapshotGenerationRef.current) return;
      const snapshot = detail.scrollback_b64
        ? decodeBase64ToString(detail.scrollback_b64)
        : '';
      // Output events are subscribed while the snapshot is in flight. Buffer
      // them so a late HTTP response can never overwrite newer live output.
      setOutput(trimOutput(snapshot + pendingChunksRef.current.join('')));
    } catch {
      if (generation !== snapshotGenerationRef.current) return;
      setOutput(trimOutput(pendingChunksRef.current.join('')));
    } finally {
      if (generation !== snapshotGenerationRef.current) return;
      snapshotLoadingRef.current = false;
      pendingChunksRef.current = [];
      setLoading(false);
    }
  }, [session.terminal_id]);

  useEffect(() => {
    void loadSnapshot();

    const offOutput = ipcBridge.terminal.onOutput.on((event) => {
      if (event.terminal_id !== session.terminal_id) return;
      const chunk = decoderRef.current(event.data_b64);
      if (snapshotLoadingRef.current) {
        pendingChunksRef.current.push(chunk);
        return;
      }
      setOutput((previous) => trimOutput(previous + chunk));
    });
    const offReconnected = ipcBridge.terminal.onReconnected.on(() => {
      void loadSnapshot();
    });
    return () => {
      snapshotGenerationRef.current += 1;
      snapshotLoadingRef.current = false;
      pendingChunksRef.current = [];
      offOutput();
      offReconnected();
    };
  }, [loadSnapshot, session.terminal_id]);

  if (loading) {
    return (
      <div className='flex min-h-80px items-center justify-center'>
        <Spin size={18} />
      </div>
    );
  }

  return (
    <pre
      className='m-0 max-h-240px overflow-auto whitespace-pre-wrap break-all rounded-8px bg-[var(--color-fill-1)] p-10px text-11px leading-17px text-t-secondary'
      aria-label={t('terminal.conversationPanel.output')}
    >
      {output || t('terminal.conversationPanel.noOutput')}
    </pre>
  );
};

const statusClass = (status: ITerminalSession['last_status']): string => {
  if (status === 'running') return 'bg-[rgb(var(--success-6))]';
  if (status === 'error') return 'bg-[rgb(var(--danger-6))]';
  return 'bg-[var(--color-text-4)]';
};

interface TerminalPanelItemProps {
  session: ITerminalSession;
  expanded: boolean;
  busy: boolean;
  closing: boolean;
  onToggle: () => void;
  onClose: () => Promise<void>;
  onRemove: () => Promise<void>;
}

const TerminalPanelItem: React.FC<TerminalPanelItemProps> = ({
  session,
  expanded,
  busy,
  closing,
  onToggle,
  onClose,
  onRemove,
}) => {
  const { t } = useTranslation();
  const command = [session.command, ...session.args].join(' ');

  return (
    <section className='overflow-hidden rounded-10px border border-solid border-[var(--color-border-2)] bg-[var(--color-bg-2)]'>
      <button
        type='button'
        className='flex w-full cursor-pointer items-center gap-8px border-0 bg-transparent px-10px py-9px text-left text-t-primary'
        aria-expanded={expanded}
        onClick={onToggle}
      >
        {expanded ? <Down size={14} /> : <Right size={14} />}
        <Terminal size={16} />
        <span className='min-w-0 flex-1'>
          <span className='block truncate text-13px font-600'>{session.name}</span>
          <span className='block truncate text-11px text-t-tertiary'>{command}</span>
        </span>
        <span
          className={`h-7px w-7px shrink-0 rounded-full ${
            closing ? 'bg-[rgb(var(--warning-6))]' : statusClass(session.last_status)
          }`}
        />
      </button>

      {expanded && (
        <div className='flex flex-col gap-9px border-0 border-t border-solid border-[var(--color-border-2)] p-10px'>
          <dl className='m-0 grid grid-cols-[64px_minmax(0,1fr)] gap-x-8px gap-y-5px text-11px'>
            <dt className='text-t-tertiary'>{t('terminal.conversationPanel.status')}</dt>
            <dd className='m-0 text-t-secondary'>
              {closing ? t('terminal.conversationPanel.stopping') : session.last_status}
            </dd>
            <dt className='text-t-tertiary'>{t('terminal.conversationPanel.cwd')}</dt>
            <dd className='m-0 break-all text-t-secondary'>{session.cwd}</dd>
            <dt className='text-t-tertiary'>{t('terminal.conversationPanel.command')}</dt>
            <dd className='m-0 break-all text-t-secondary'>{command}</dd>
            <dt className='text-t-tertiary'>ID</dt>
            <dd className='m-0 break-all font-mono text-t-tertiary'>{session.terminal_id}</dd>
            {session.exit_code != null && (
              <>
                <dt className='text-t-tertiary'>{t('terminal.conversationPanel.exitCode')}</dt>
                <dd className='m-0 text-t-secondary'>{session.exit_code}</dd>
              </>
            )}
          </dl>

          <TerminalOutputTail session={session} />

          <div className='flex justify-end'>
            {closing ? (
              <Button
                size='mini'
                status='warning'
                type='text'
                loading
                disabled
                icon={<Power size={14} />}
              >
                {t('terminal.conversationPanel.stopping')}
              </Button>
            ) : session.last_status === 'running' ? (
              <Popconfirm
                title={t('terminal.conversationPanel.closeConfirm', { name: session.name })}
                onOk={onClose}
              >
                <Button
                  size='mini'
                  status='danger'
                  type='text'
                  loading={busy}
                  icon={<Power size={14} />}
                >
                  {t('terminal.action.close')}
                </Button>
              </Popconfirm>
            ) : (
              <Popconfirm
                title={t('terminal.conversationPanel.removeConfirm', { name: session.name })}
                onOk={onRemove}
              >
                <Button
                  size='mini'
                  status='danger'
                  type='text'
                  loading={busy}
                  icon={<DeleteOne size={14} />}
                >
                  {t('terminal.conversationPanel.remove')}
                </Button>
              </Popconfirm>
            )}
          </div>
        </div>
      )}
    </section>
  );
};

interface ConversationTerminalPanelProps {
  conversationId: ConversationId;
}

const ConversationTerminalPanel: React.FC<ConversationTerminalPanelProps> = ({ conversationId }) => {
  const { t } = useTranslation();
  const [sessions, setSessions] = useState<ITerminalSession[]>([]);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [expandedId, setExpandedId] = useState<TerminalId | null>(null);
  const [busyIds, setBusyIds] = useState<Set<TerminalId>>(new Set());
  const [closingIds, setClosingIds] = useState<Set<TerminalId>>(new Set());
  const activeConversationIdRef = useRef(conversationId);
  const refreshRequestRef = useRef(0);
  const lifecycleRevisionRef = useRef(0);
  activeConversationIdRef.current = conversationId;

  const refresh = useCallback(async () => {
    const requestedConversationId = conversationId;
    const requestId = ++refreshRequestRef.current;
    const lifecycleRevision = lifecycleRevisionRef.current;
    try {
      const list = await ipcBridge.terminal.listConversation.invoke({
        conversation_id: requestedConversationId,
      });
      if (
        activeConversationIdRef.current !== requestedConversationId ||
        refreshRequestRef.current !== requestId
      ) {
        return;
      }
      if (lifecycleRevisionRef.current !== lifecycleRevision) {
        // A lifecycle event arrived while this snapshot was in flight. Fetch
        // again instead of letting the stale response overwrite that event.
        void refresh();
        return;
      }
      const nextSessions = Array.isArray(list) ? list : [];
      setSessions(nextSessions);
      setClosingIds((previous) => {
        const stillRunning = new Set(
          nextSessions
            .filter((session) => session.last_status === 'running')
            .map((session) => session.terminal_id),
        );
        const next = new Set([...previous].filter((id) => stillRunning.has(id)));
        return next.size === previous.size ? previous : next;
      });
      setError(null);
    } catch (caught: unknown) {
      if (
        activeConversationIdRef.current !== requestedConversationId ||
        refreshRequestRef.current !== requestId
      ) {
        return;
      }
      setError(caught instanceof Error ? caught.message : String(caught));
    } finally {
      if (
        activeConversationIdRef.current === requestedConversationId &&
        refreshRequestRef.current === requestId
      ) {
        setLoading(false);
      }
    }
  }, [conversationId]);

  useEffect(() => {
    refreshRequestRef.current += 1;
    lifecycleRevisionRef.current += 1;
    setLoading(true);
    setSessions([]);
    setExpandedId(null);
    setBusyIds(new Set());
    setClosingIds(new Set());
    void refresh();

    const offCreated = ipcBridge.terminal.onCreated.on((session) => {
      if (
        activeConversationIdRef.current !== conversationId ||
        session.owner_conversation_id !== conversationId
      ) {
        return;
      }
      lifecycleRevisionRef.current += 1;
      setSessions((previous) =>
        previous.some((item) => item.terminal_id === session.terminal_id)
          ? previous.map((item) => (item.terminal_id === session.terminal_id ? session : item))
          : [session, ...previous],
      );
    });
    const offUpdated = ipcBridge.terminal.onUpdated.on((session) => {
      if (
        activeConversationIdRef.current !== conversationId ||
        session.owner_conversation_id !== conversationId
      ) {
        return;
      }
      lifecycleRevisionRef.current += 1;
      if (session.last_status !== 'running') {
        setClosingIds((previous) => {
          const next = new Set(previous);
          next.delete(session.terminal_id);
          return next;
        });
      }
      setSessions((previous) => {
        return previous.some((item) => item.terminal_id === session.terminal_id)
          ? previous.map((item) => (item.terminal_id === session.terminal_id ? session : item))
          : [session, ...previous];
      });
    });
    const offExit = ipcBridge.terminal.onExit.on((event) => {
      if (activeConversationIdRef.current !== conversationId) return;
      lifecycleRevisionRef.current += 1;
      setClosingIds((previous) => {
        const next = new Set(previous);
        next.delete(event.terminal_id);
        return next;
      });
      setSessions((previous) =>
        previous.map((item) =>
          item.terminal_id === event.terminal_id
            ? { ...item, last_status: 'exited', exit_code: event.exit_code }
            : item,
        ),
      );
    });
    const offRemoved = ipcBridge.terminal.onRemoved.on((event) => {
      if (activeConversationIdRef.current !== conversationId) return;
      lifecycleRevisionRef.current += 1;
      setSessions((previous) => previous.filter((item) => item.terminal_id !== event.terminal_id));
      setExpandedId((current) => (current === event.terminal_id ? null : current));
      setClosingIds((previous) => {
        const next = new Set(previous);
        next.delete(event.terminal_id);
        return next;
      });
    });
    const offReconnected = ipcBridge.terminal.onReconnected.on(() => {
      if (activeConversationIdRef.current !== conversationId) return;
      void refresh();
    });

    return () => {
      refreshRequestRef.current += 1;
      offCreated();
      offUpdated();
      offExit();
      offRemoved();
      offReconnected();
    };
  }, [conversationId, refresh]);

  const runningCount = useMemo(
    () =>
      sessions.filter(
        (session) =>
          session.last_status === 'running' && !closingIds.has(session.terminal_id),
      ).length,
    [closingIds, sessions],
  );

  const runAction = useCallback(
    async (terminalId: TerminalId, action: () => Promise<unknown>) => {
      setBusyIds((previous) => new Set(previous).add(terminalId));
      try {
        await action();
        setError(null);
        await refresh();
      } catch (caught: unknown) {
        setError(caught instanceof Error ? caught.message : String(caught));
      } finally {
        setBusyIds((previous) => {
          const next = new Set(previous);
          next.delete(terminalId);
          return next;
        });
      }
    },
    [refresh],
  );

  const closeTerminal = useCallback(
    async (terminalId: TerminalId) => {
      setBusyIds((previous) => new Set(previous).add(terminalId));
      setClosingIds((previous) => new Set(previous).add(terminalId));
      try {
        await ipcBridge.terminal.kill.invoke({ terminal_id: terminalId });
        setError(null);
        // Keep the optimistic closing marker while the durable row may still
        // report running. An exit/update event or reconciliation clears it.
        await refresh();
      } catch (caught: unknown) {
        setClosingIds((previous) => {
          const next = new Set(previous);
          next.delete(terminalId);
          return next;
        });
        setError(caught instanceof Error ? caught.message : String(caught));
      } finally {
        setBusyIds((previous) => {
          const next = new Set(previous);
          next.delete(terminalId);
          return next;
        });
      }
    },
    [refresh],
  );

  if (loading) {
    return (
      <div className='flex size-full items-center justify-center'>
        <Spin />
      </div>
    );
  }

  return (
    <div className='flex size-full flex-col gap-10px overflow-y-auto p-10px box-border'>
      <div className='flex items-center justify-between text-12px text-t-secondary'>
        <span>
          {t('terminal.conversationPanel.summary', {
            total: sessions.length,
            running: runningCount,
          })}
        </span>
        <Button size='mini' type='text' onClick={() => void refresh()}>
          {t('common.refresh')}
        </Button>
      </div>

      {error && (
        <div className='rounded-8px bg-[rgb(var(--danger-1))] px-9px py-7px text-11px text-[rgb(var(--danger-6))]'>
          {error}
        </div>
      )}

      {sessions.length === 0 ? (
        <div className='flex min-h-180px items-center justify-center'>
          <Empty description={t('terminal.conversationPanel.empty')} />
        </div>
      ) : (
        sessions.map((session) => (
          <TerminalPanelItem
            key={session.terminal_id}
            session={session}
            expanded={expandedId === session.terminal_id}
            busy={busyIds.has(session.terminal_id)}
            closing={closingIds.has(session.terminal_id)}
            onToggle={() =>
              setExpandedId((current) =>
                current === session.terminal_id ? null : session.terminal_id,
              )
            }
            onClose={() => closeTerminal(session.terminal_id)}
            onRemove={() =>
              runAction(session.terminal_id, () =>
                ipcBridge.terminal.remove.invoke({ terminal_id: session.terminal_id }),
              )
            }
          />
        ))
      )}
    </div>
  );
};

export default ConversationTerminalPanel;
