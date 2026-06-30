/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import React, { useEffect, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Button, Input, Spin } from '@arco-design/web-react';
import { Comment, Left, Redo, Send } from '@icon-park/react';
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
 * ProjectedWorkerView — the read-only projection of one DAG worker node into the
 * conversation content area (「会话原生编排」F7; left chat column of the 左右分屏).
 * Rendered by {@link ConversationContentSwitcher} ON TOP of the (display:none)
 * main NomiChat whenever a node is projected, so the user can inspect a worker's
 * live record and adjust / rerun it in place, then return to the main agent.
 *
 * Layout:
 *  - a thin banner (left「查看:<title>」; right [重跑] / [← 返回 main]);
 *  - the worker transcript body (replicates {@link WorkerTranscriptPanel}'s
 *    resolution: `task.conversation_id → conversation.get → ReadOnlyConversationView`,
 *    with「尚未开始」/ loader states);
 *  - a docked「局部调整」composer at the BOTTOM (only once the worker has started)
 *    — a labeled input + 提交 that injects a steering message into the worker's
 *    live conversation (`runs.steer`). This used to be a 「转向…」popover in the
 *    banner; moved to the bottom so the user types the adjustment where the eye
 *    already is (用户反馈).
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

  // 局部调整 (mid-turn steer) draft + in-flight state (now a docked bottom bar).
  const [adjustText, setAdjustText] = useState('');
  const [adjusting, setAdjusting] = useState(false);
  // Guards the rerun trigger against a double-click while the request is in flight.
  const [rerunning, setRerunning] = useState(false);

  // Reset the adjust draft whenever the projected task changes.
  useEffect(() => {
    setAdjustText('');
  }, [task.id]);

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

  // Submit a 局部调整: inject a steering message into the worker's live
  // conversation. On success clear the draft; no refetch needed (the transcript
  // stream surfaces the injected turn on its own).
  const submitAdjust = async () => {
    const text = adjustText.trim();
    if (!text || adjusting) return;
    setAdjusting(true);
    try {
      await ipcBridge.orchestrator.runs.steer.invoke({
        run_id: runId,
        task_id: task.id,
        updates: { text },
      });
      message.success(t('orchestrator.run.steer.sent', { defaultValue: '已提交局部调整' }));
      setAdjustText('');
    } catch (e) {
      message.error(t('orchestrator.run.steer.error', { defaultValue: '局部调整失败:{{error}}', error: String(e) }));
    } finally {
      setAdjusting(false);
    }
  };

  const adjustDisabled = adjusting || adjustText.trim().length === 0;

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

      {/* ── Body: worker transcript / not-started / loading ───────────────── */}
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
          <ReadOnlyConversationView conversation={conversation} hideSendBox agent_name={task.title} />
        ) : null}
      </div>

      {/* ── 局部调整 composer (docked bottom) — only once the worker has started.
          The user types an adjustment for this node and submits it (runs.steer)
          right where they are reading the transcript. Enter submits;
          Shift+Enter / IME composition inserts a newline. ───────────────────── */}
      {conversationId !== undefined && (
        <div className={styles.steerBar}>
          <span className={styles.steerLabel}>
            <Send theme='outline' size='13' strokeWidth={3} />
            {t('orchestrator.run.steer.label', { defaultValue: '局部调整' })}
          </span>
          <Input.TextArea
            value={adjustText}
            disabled={adjusting}
            placeholder={t('orchestrator.run.steer.placeholder', { defaultValue: '输入对当前节点的局部调整方向…' })}
            autoSize={{ minRows: 1, maxRows: 4 }}
            className={styles.steerInput}
            onChange={(v: string) => setAdjustText(v)}
            onKeyDown={(e: React.KeyboardEvent<HTMLTextAreaElement>) => {
              if (e.key === 'Enter' && !e.shiftKey && !e.nativeEvent.isComposing) {
                e.preventDefault();
                if (!adjustDisabled) void submitAdjust();
              }
            }}
          />
          <Button
            type='primary'
            size='small'
            loading={adjusting}
            disabled={adjustText.trim().length === 0}
            onClick={() => void submitAdjust()}
          >
            {t('orchestrator.run.steer.submit', { defaultValue: '提交' })}
          </Button>
        </div>
      )}
    </div>
  );
};

export default ProjectedWorkerView;
