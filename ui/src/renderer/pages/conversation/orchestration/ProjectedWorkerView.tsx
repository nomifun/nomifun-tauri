/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import React, { useEffect, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Input, Popover, Spin } from '@arco-design/web-react';
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
 * conversation content area (「会话原生编排 v2」F7). Rendered by
 * {@link ConversationContentSwitcher} ON TOP of the (display:none) main NomiChat
 * whenever a node is projected, so the user can inspect a worker's live record
 * and steer / rerun it in place, then return to the main agent.
 *
 * Layout: a thin banner (left「查看:<title>」, right [重跑] / [转向…] / [← 返回 main])
 * over a transcript body. The body replicates {@link WorkerTranscriptPanel}'s
 * resolution exactly: `task.conversation_id → ipcBridge.conversation.get →
 * <ReadOnlyConversationView hideSendBox agent_name={task.title} />`, with a
 * 「尚未开始」empty state when `conversation_id` is undefined and a loader while
 * the conversation is being fetched.
 *
 * `TRunTask.conversation_id` is already the backend INTEGER id — passed straight
 * through with no conversion (unlike the string TeamAgent.conversation_id).
 */
const ProjectedWorkerView: React.FC<ProjectedWorkerViewProps> = ({ payload }) => {
  const { t } = useTranslation();
  const { returnToMain } = useOrchestration();
  const [message, msgCtx] = useArcoMessage();

  const { task, runId } = payload;
  const conversationId = task.conversation_id;

  const [conversation, setConversation] = useState<TChatConversation | null>(null);
  const [loading, setLoading] = useState(false);

  // Steer (mid-turn inject) draft + in-flight state, behind the 「转向…」popover.
  const [steerOpen, setSteerOpen] = useState(false);
  const [steerText, setSteerText] = useState('');
  const [steering, setSteering] = useState(false);
  // Guards the rerun trigger against a double-click while the request is in flight.
  const [rerunning, setRerunning] = useState(false);

  // Reset the steer draft whenever the projected task changes.
  useEffect(() => {
    setSteerText('');
    setSteerOpen(false);
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
      message.error(
        t('orchestrator.run.rerun.error', { defaultValue: '重跑失败:{{error}}', error: String(e) })
      );
    } finally {
      setRerunning(false);
    }
  };

  // Inject a steering message into the worker's live conversation. On success we
  // clear the draft + close the popover; no refetch needed (the transcript stream
  // surfaces the injected turn on its own).
  const sendSteer = async () => {
    const text = steerText.trim();
    if (!text || steering) return;
    setSteering(true);
    try {
      await ipcBridge.orchestrator.runs.steer.invoke({
        run_id: runId,
        task_id: task.id,
        updates: { text },
      });
      message.success(t('orchestrator.run.steer.sent', { defaultValue: '已发送转向消息' }));
      setSteerText('');
      setSteerOpen(false);
    } catch (e) {
      message.error(
        t('orchestrator.run.steer.error', { defaultValue: '转向失败:{{error}}', error: String(e) })
      );
    } finally {
      setSteering(false);
    }
  };

  const steerDisabled = steering || steerText.trim().length === 0;

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

          {/* 转向… (mid-turn inject), behind a small Arco Popover */}
          <Popover
            trigger='click'
            position='br'
            popupVisible={steerOpen}
            onVisibleChange={setSteerOpen}
            content={
              <div className={styles.steerPop}>
                <div className={styles.steerPopTitle}>{t('orchestrator.run.steer.title', { defaultValue: '转向' })}</div>
                <div className={styles.steerPopHint}>
                  {t('orchestrator.run.steer.hint', { defaultValue: '向正在运行的 agent 中途注入一条消息' })}
                </div>
                <Input.TextArea
                  autoFocus
                  value={steerText}
                  disabled={steering}
                  placeholder={t('orchestrator.run.steer.placeholder', { defaultValue: '输入要注入的消息…' })}
                  autoSize={{ minRows: 2, maxRows: 6 }}
                  onChange={(v: string) => setSteerText(v)}
                />
                <div className={styles.steerPopRow}>
                  <div
                    role='button'
                    tabIndex={0}
                    aria-label={t('orchestrator.run.steer.send', { defaultValue: '发送' })}
                    aria-disabled={steerDisabled}
                    className={`${styles.action} ${styles.actionPrimary}`}
                    onClick={steerDisabled ? undefined : () => void sendSteer()}
                    onKeyDown={(e) => {
                      if ((e.key === 'Enter' || e.key === ' ') && !steerDisabled) {
                        e.preventDefault();
                        void sendSteer();
                      }
                    }}
                  >
                    <Send theme='outline' size='13' strokeWidth={3} />
                    <span>{t('orchestrator.run.steer.send', { defaultValue: '发送' })}</span>
                  </div>
                </div>
              </div>
            }
          >
            <div
              role='button'
              tabIndex={0}
              aria-label={t('orchestrator.run.steer.open', { defaultValue: '转向…' })}
              className={styles.action}
            >
              <Send theme='outline' size='13' strokeWidth={3} />
              <span>{t('orchestrator.run.steer.open', { defaultValue: '转向…' })}</span>
            </div>
          </Popover>

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
    </div>
  );
};

export default ProjectedWorkerView;
