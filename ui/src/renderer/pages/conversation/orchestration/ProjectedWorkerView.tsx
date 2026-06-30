/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import React, { useEffect, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Spin } from '@arco-design/web-react';
import { Comment, Left, Redo, CheckOne } from '@icon-park/react';
import { ipcBridge } from '@/common';
import type { TChatConversation } from '@/common/config/storage';
import type { OpenTaskPayload } from '@/renderer/pages/orchestrator/RunDetail/DagCanvas';
import { useArcoMessage } from '@/renderer/utils/ui/useArcoMessage';
import { useOrchestration } from './OrchestrationContext';
import ReadOnlyConversationView from '@/renderer/pages/orchestrator/RunDetail/ReadOnlyConversationView';
import styles from './projectedWorkerView.module.css';

type ProjectedWorkerViewProps = {
  /** The clicked DAG node's payload (task + assignment + run id + refetch). */
  payload: OpenTaskPayload;
};

/**
 * ProjectedWorkerView — projects one DAG worker node into the conversation content
 * area (「会话原生编排」F7; left chat column of the 左右分屏). Rendered by
 * {@link ConversationContentSwitcher} ON TOP of the (display:none) main NomiChat
 * whenever a node is projected, so the user can inspect a worker's record, talk to
 * it, and rerun it — then return to the main agent.
 *
 * Layout:
 *  - a thin banner (left「查看:<title>」; right [采用为该节点产出] / [重跑] / [← 返回 main]);
 *  - the worker conversation, rendered via {@link ReadOnlyConversationView}
 *    WITHOUT `hideSendBox` — so the worker's OWN full composer (NomiChat →
 *    NomiSendBox) is reused: current-model pill, `+` attachments, @-file mentions,
 *    slash commands, autonomy pill, multi-line auto-grow, circular send. The user
 *    types a 局部调整 by talking to the worker directly (a normal turn in the
 *    worker's conversation) — the fullest, most familiar input surface, instead of
 *    a bespoke steer box. 「尚未开始」/ loader states cover the not-started case.
 *
 * Because that continued chat is a plain worker turn the engine does NOT observe,
 * [采用为该节点产出] is the explicit hand-off back into the DAG: it asks the backend
 * to re-read the worker's latest output, mark this node done, and re-activate the
 * run so downstream unblocks (UC-2c). [重跑] resets + re-runs the node from scratch.
 *
 * `TRunTask.conversation_id` is already the backend INTEGER id — passed straight
 * through with no conversion.
 */
const ProjectedWorkerView: React.FC<ProjectedWorkerViewProps> = ({ payload }) => {
  const { t } = useTranslation();
  const { returnToMain } = useOrchestration();
  const [message, msgCtx] = useArcoMessage();

  const { task, runId } = payload;
  const conversationId = task.conversation_id;

  const [conversation, setConversation] = useState<TChatConversation | null>(null);
  const [loading, setLoading] = useState(false);
  // Guards the rerun trigger against a double-click while the request is in flight.
  const [rerunning, setRerunning] = useState(false);
  // Guards the「采用为该节点产出」trigger against a double-click while in flight.
  const [adopting, setAdopting] = useState(false);

  // Resolve the worker conversation off `task.conversation_id` (mirrors
  // WorkerTranscriptPanel). Undefined → no conversation yet (「尚未开始」state).
  useEffect(() => {
    if (conversationId === undefined) {
      setConversation(null);
      return;
    }
    let cancelled = false;
    setLoading(true);
    void ipcBridge.conversation.get
      .invoke({ id: conversationId })
      .then((conv) => {
        if (!cancelled) setConversation((conv as TChatConversation | null) ?? null);
      })
      .catch((e) => {
        console.error('[ProjectedWorkerView] load conversation failed:', e);
        if (!cancelled) setConversation(null);
      })
      .finally(() => {
        if (!cancelled) setLoading(false);
      });
    return () => {
      cancelled = true;
    };
  }, [conversationId]);

  // Re-execute this node (and cascade-reset its settled downstream). On success
  // we refetch so the canvas reflects the reset + re-drive immediately.
  const doRerun = async () => {
    if (rerunning) return;
    setRerunning(true);
    try {
      await ipcBridge.orchestrator.runs.rerunTask.invoke({ run_id: runId, task_id: task.id });
      message.success(t('orchestrator.run.rerun.ok', { defaultValue: '已重跑该节点' }));
      await payload.refetch();
    } catch (e) {
      message.error(t('orchestrator.run.rerun.error', { defaultValue: '重跑失败:{{error}}', error: String(e) }));
    } finally {
      setRerunning(false);
    }
  };

  // Adopt the worker conversation's CURRENT output as this node's product
  // (UC-2c「采用为该节点产出」). After the user kept chatting with a failed/stuck
  // worker (a normal turn in its conversation, NOT observed by the engine), this is
  // the explicit hand-off: the engine re-reads the worker's latest output, marks the
  // node done, and re-activates the run so downstream unblocks. On success we refetch
  // so the canvas reflects the now-completed node + re-drive.
  const doAdopt = async () => {
    if (adopting) return;
    setAdopting(true);
    try {
      await ipcBridge.orchestrator.runs.adoptTaskResult.invoke({ run_id: runId, task_id: task.id });
      message.success(t('orchestrator.run.adopt.ok', { defaultValue: '已采用为该节点产出' }));
      await payload.refetch();
    } catch (e) {
      message.error(t('orchestrator.run.adopt.error', { defaultValue: '采用失败:{{error}}', error: String(e) }));
    } finally {
      setAdopting(false);
    }
  };

  return (
    <div className={styles.root}>
      {msgCtx}

      {/* ── Banner: context (left) + node actions (right) ─────────────────── */}
      <div className={styles.banner}>
        <div className={styles.bannerLead}>
          <span className={styles.bannerBadge}>
            <Comment theme='outline' size='13' strokeWidth={3} />
          </span>
          <span className={styles.bannerEyebrow}>{t('orchestrator.run.project.viewing', { defaultValue: '查看' })}</span>
          <span className={styles.bannerTitle} title={task.title}>
            {task.title}
          </span>
        </div>

        <div className={styles.bannerActions}>
          {/* 采用为该节点产出 — only when a worker conversation exists to read from. */}
          {conversationId !== undefined ? (
            <div
              role='button'
              tabIndex={0}
              aria-label={t('orchestrator.run.adopt.button', { defaultValue: '采用为该节点产出' })}
              aria-disabled={adopting}
              className={`${styles.action} ${styles.actionAdopt}`}
              onClick={adopting ? undefined : () => void doAdopt()}
              onKeyDown={(e) => {
                if ((e.key === 'Enter' || e.key === ' ') && !adopting) {
                  e.preventDefault();
                  void doAdopt();
                }
              }}
            >
              <CheckOne theme='outline' size='13' strokeWidth={3} />
              <span>{t('orchestrator.run.adopt.button', { defaultValue: '采用为该节点产出' })}</span>
            </div>
          ) : null}

          {/* 重跑 */}
          <div
            role='button'
            tabIndex={0}
            aria-label={t('orchestrator.run.rerun.button', { defaultValue: '重跑' })}
            aria-disabled={rerunning}
            className={styles.action}
            onClick={rerunning ? undefined : () => void doRerun()}
            onKeyDown={(e) => {
              if ((e.key === 'Enter' || e.key === ' ') && !rerunning) {
                e.preventDefault();
                void doRerun();
              }
            }}
          >
            <Redo theme='outline' size='13' strokeWidth={3} />
            <span>{t('orchestrator.run.rerun.button', { defaultValue: '重跑' })}</span>
          </div>

          {/* ← 返回 main */}
          <div
            role='button'
            tabIndex={0}
            aria-label={t('orchestrator.run.project.returnMain', { defaultValue: '返回 main' })}
            className={`${styles.action} ${styles.actionPrimary}`}
            onClick={() => returnToMain()}
            onKeyDown={(e) => {
              if (e.key === 'Enter' || e.key === ' ') {
                e.preventDefault();
                returnToMain();
              }
            }}
          >
            <Left theme='outline' size='13' strokeWidth={3} />
            <span>{t('orchestrator.run.project.returnMain', { defaultValue: '返回 main' })}</span>
          </div>
        </div>
      </div>

      {/* ── Body: the worker conversation, EDITABLE (full NomiSendBox reused) ──
          Not-started / loading covered; otherwise the worker's own conversation
          with its full composer (model pill, attachments, @, slash, send). */}
      <div className={styles.body}>
        {conversationId === undefined ? (
          <div className={styles.center}>
            <span className={styles.emptyIcon}>
              <Comment theme='outline' size='26' strokeWidth={3} />
            </span>
            <div className={styles.emptyTitle}>
              {t('orchestrator.run.transcript.notStarted', { defaultValue: '该 agent 尚未开始' })}
            </div>
            <div className={styles.emptyHint}>
              {t('orchestrator.run.transcript.noConversation', {
                defaultValue: '该节点还没有被 worker 接手,暂无可查看的会话记录。',
              })}
            </div>
          </div>
        ) : loading ? (
          <Spin loading className='flex flex-1 items-center justify-center' />
        ) : conversation ? (
          <ReadOnlyConversationView conversation={conversation} agent_name={task.title} />
        ) : null}
      </div>
    </div>
  );
};

export default ProjectedWorkerView;
