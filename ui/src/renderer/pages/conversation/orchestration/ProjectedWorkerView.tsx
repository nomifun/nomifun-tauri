/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import React, { useEffect, useMemo, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Dropdown, Menu, Spin } from '@arco-design/web-react';
import { Brain, Comment, Down, Left, Redo, CheckOne, SettingOne } from '@icon-park/react';
import { ipcBridge } from '@/common';
import type { TChatConversation } from '@/common/config/storage';
import type { OpenTaskPayload } from '@/renderer/pages/orchestrator/RunDetail/DagCanvas';
import { memberShortLabel } from '@/renderer/pages/orchestrator/RunDetail/memberLabel';
import { useArcoMessage } from '@/renderer/utils/ui/useArcoMessage';
import { useOrchestration } from './OrchestrationContext';
import NodePreconfigPanel from './NodePreconfigPanel';
import RouteErrorBoundary from '@/renderer/components/layout/RouteErrorBoundary';
import ReadOnlyConversationView from '@/renderer/pages/orchestrator/RunDetail/ReadOnlyConversationView';
import styles from './projectedWorkerView.module.css';

type ProjectedWorkerViewProps = {
  /** The clicked DAG node's payload (task + assignment + run id + refetch). */
  payload: OpenTaskPayload;
};

// These confirmation toasts float over the node banner. Keep them brief and
// click-through (`nomifun-message-passthrough` flips the Arco message box back to
// `pointer-events:none`) so they never block the banner's 重跑 / 采用 / 返回 main
// buttons while on screen. Errors linger a touch longer to stay readable.
const TOAST_CLASS = 'nomifun-message-passthrough';
const TOAST_OK_MS = 1500;
const TOAST_ERR_MS = 2500;

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
  const { returnToMain, detail } = useOrchestration();
  const [message, msgCtx] = useArcoMessage();

  const { task: snapshotTask, runId } = payload;
  // Re-resolve the node from the LIVE run detail by its stable id, rather than
  // reading the click-time snapshot in `payload.task`. The context's `detail` is
  // WS-driven (useRunLive refetches on every run-engine event), so a 重跑 / 采用 —
  // which resets this node and has the engine spawn a NEW worker conversation —
  // flows through here in real time: the node's `conversation_id` (and title /
  // status) update without the user having to switch nodes and back. Falls back to
  // the snapshot until `detail` first loads / for a node not (yet) in `detail`.
  const task = useMemo(
    () => detail?.tasks.find((tt) => tt.id === snapshotTask.id) ?? snapshotTask,
    [detail?.tasks, snapshotTask]
  );
  const conversationId = task.conversation_id;

  const [conversation, setConversation] = useState<TChatConversation | null>(null);
  const [loading, setLoading] = useState(false);
  // Guards the rerun trigger against a double-click while the request is in flight.
  const [rerunning, setRerunning] = useState(false);
  // Guards the「采用为该节点产出」trigger against a double-click while in flight.
  const [adopting, setAdopting] = useState(false);
  // Guards the「改用模型」(reassign) trigger against a double-click while in flight.
  const [reassigning, setReassigning] = useState(false);

  // The run's frozen fleet snapshot = the model pool this node may use, and the
  // node's LIVE assignment (resolved off `detail`, not the click-time snapshot, so
  // a reassign reflects immediately). Falls back to the payload for the first paint.
  const fleetMembers = detail?.fleet_members ?? payload.fleetMembers;
  const liveAssignment = useMemo(
    () => detail?.assignments.find((a) => a.task_id === task.id) ?? payload.assignment,
    [detail?.assignments, task.id, payload.assignment]
  );
  const currentMember = liveAssignment
    ? fleetMembers.find((m) => m.id === liveAssignment.member_id)
    : undefined;
  const currentModelLabel =
    memberShortLabel(currentMember) ?? t('orchestrator.run.assign.changeModel', { defaultValue: '改用模型' });

  // A node that has already run (or is running) needs an explicit 重跑 for the new
  // model to take effect; a not-yet-started node simply picks it up at dispatch.
  const taskSettled = ['running', 'done', 'completed', 'failed', 'error'].includes(task.status);

  // 启动前配置台 (迁移 025): a node that is NOT currently running can have its model
  // + 预置要求 configured. A pending (no-conversation) node shows the panel as its
  // body (applied at dispatch); a settled node shows it as a collapsible "重跑配置"
  // above its transcript (applied on the next 重跑). A running node never shows it.
  const canConfig = task.status !== 'running';
  const [configOpen, setConfigOpen] = useState(false);

  // Reassign this node to a different fleet member (= a different model). The
  // backend only updates the assignment row — for a PENDING node the engine reads
  // it fresh at dispatch (no rerun needed); for a settled/running node the user
  // must 重跑 (right here in the same banner) for the new model to actually run.
  const doReassign = async (memberId: string) => {
    if (reassigning || memberId === liveAssignment?.member_id) return;
    setReassigning(true);
    try {
      await ipcBridge.orchestrator.runs.reassign.invoke({
        run_id: runId,
        task_id: task.id,
        updates: { member_id: memberId },
      });
      message.success({
        content: taskSettled
          ? t('orchestrator.run.assign.reassignThenRerun', {
              defaultValue: '已改用模型；该节点已运行过，点「重跑」用新模型重跑',
            })
          : t('orchestrator.run.assign.reassignSuccess', { defaultValue: '已更新分派' }),
        duration: taskSettled ? TOAST_ERR_MS : TOAST_OK_MS,
        className: TOAST_CLASS,
      });
      await payload.refetch();
    } catch (e) {
      message.error({
        content: t('orchestrator.run.assign.reassignError', {
          defaultValue: '更新分派失败：{{error}}',
          error: String(e),
        }),
        duration: TOAST_ERR_MS,
        className: TOAST_CLASS,
      });
    } finally {
      setReassigning(false);
    }
  };

  const modelMenu = (
    <Menu
      selectedKeys={liveAssignment ? [liveAssignment.member_id] : []}
      onClickMenuItem={(key) => {
        void doReassign(key);
      }}
    >
      {fleetMembers.map((m) => (
        <Menu.Item key={m.id}>{memberShortLabel(m) ?? m.model ?? m.agent_id ?? m.id}</Menu.Item>
      ))}
    </Menu>
  );

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
      message.success({
        content: t('orchestrator.run.rerun.ok', { defaultValue: '已重跑该节点' }),
        duration: TOAST_OK_MS,
        className: TOAST_CLASS,
      });
      await payload.refetch();
    } catch (e) {
      message.error({
        content: t('orchestrator.run.rerun.error', { defaultValue: '重跑失败:{{error}}', error: String(e) }),
        duration: TOAST_ERR_MS,
        className: TOAST_CLASS,
      });
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
      message.success({
        content: t('orchestrator.run.adopt.ok', { defaultValue: '已采用为该节点产出' }),
        duration: TOAST_OK_MS,
        className: TOAST_CLASS,
      });
      await payload.refetch();
    } catch (e) {
      message.error({
        content: t('orchestrator.run.adopt.error', { defaultValue: '采用失败:{{error}}', error: String(e) }),
        duration: TOAST_ERR_MS,
        className: TOAST_CLASS,
      });
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
          {/* 改用模型 — reassign this node to another model from the run's pool. Only
              shown when there's an actual choice (>1 member). A pending node picks
              the new model up at dispatch; a settled/running node needs 重跑 (right
              here) for it to take effect — the success toast says which. */}
          {fleetMembers.length > 1 && (
            <Dropdown trigger='click' position='br' droplist={modelMenu}>
              <div
                role='button'
                tabIndex={0}
                aria-label={t('orchestrator.run.assign.reassign', { defaultValue: '改用模型给其他成员' })}
                aria-disabled={reassigning}
                className={styles.action}
              >
                <Brain theme='outline' size='13' strokeWidth={3} />
                <span className='max-w-[140px] truncate'>{currentModelLabel}</span>
                <Down theme='outline' size='12' strokeWidth={3} />
              </div>
            </Dropdown>
          )}

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
        <RouteErrorBoundary>
          {conversationId === undefined ? (
            canConfig ? (
            // Pending node — no worker conversation yet. The body IS the 启动前配置台
            // (model override + 预置要求), applied automatically at dispatch. Own
            // scroll container so it fills the pane and is fully reachable.
            <div className='flex-1 min-h-0 overflow-y-auto'>
              <NodePreconfigPanel runId={runId} task={task} settled={false} onSaved={payload.refetch} />
            </div>
          ) : (
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
          )
        ) : loading ? (
          <Spin loading className='flex flex-1 items-center justify-center' />
        ) : conversation ? (
          // Settled node — an OPTIONAL collapsible 重跑配置 above the read-only
          // transcript. Rendered as SIBLINGS of the flex-col `.body` (NO wrapper
          // div) so ReadOnlyConversationView (NomiChat, `flex-1`) keeps filling the
          // remaining height exactly as before — wrapping it in a plain block broke
          // NomiChat's flex sizing (the composer floated to the top).
          <>
            {canConfig && (
              <div className='shrink-0 border-b border-solid border-[var(--color-border-2)]'>
                <div
                  role='button'
                  tabIndex={0}
                  aria-expanded={configOpen}
                  onClick={() => setConfigOpen((v) => !v)}
                  onKeyDown={(e) => {
                    if (e.key === 'Enter' || e.key === ' ') {
                      e.preventDefault();
                      setConfigOpen((v) => !v);
                    }
                  }}
                  className='flex cursor-pointer select-none items-center gap-6px px-16px py-9px text-12px font-600 text-[var(--color-text-2)] hover:text-[var(--color-text-1)]'
                >
                  <SettingOne theme='outline' size='13' strokeWidth={3} className='line-height-0 text-[rgb(var(--primary-6))]' />
                  <span>{t('orchestrator.run.preconfig.rerunConfig', { defaultValue: '重跑配置（模型 / 预置要求）' })}</span>
                  <Down
                    theme='outline'
                    size='12'
                    strokeWidth={3}
                    className={`line-height-0 transition-transform duration-150 ${configOpen ? 'rotate-180' : ''}`}
                  />
                </div>
                {configOpen && (
                  <div className='max-h-340px overflow-y-auto border-t border-solid border-[var(--color-border-2)]'>
                    <NodePreconfigPanel runId={runId} task={task} settled onSaved={payload.refetch} />
                  </div>
                )}
              </div>
            )}
            <ReadOnlyConversationView conversation={conversation} agent_name={task.title} />
          </>
        ) : canConfig ? (
          // A conversation id exists but the record couldn't load — still let the
          // user configure this node instead of showing a blank body.
          <div className='flex-1 min-h-0 overflow-y-auto'>
            <NodePreconfigPanel runId={runId} task={task} settled onSaved={payload.refetch} />
          </div>
        ) : null}
        </RouteErrorBoundary>
      </div>
    </div>
  );
};

export default ProjectedWorkerView;
