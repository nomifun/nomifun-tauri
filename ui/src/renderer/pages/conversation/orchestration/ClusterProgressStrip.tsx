/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import React, { useCallback, useEffect, useMemo, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Down, EveryUser, Help, Up } from '@icon-park/react';
import type { TRunTask } from '@/common/types/orchestrator/orchestratorTypes';
import { taskStatusMeta } from '@/renderer/pages/orchestrator/RunDetail/nodes/TaskNode';
import { STATUS_META } from '@/renderer/pages/orchestrator/RunDetail/runStatusMeta';
import { useOrchestrationSafe } from './OrchestrationContext';
import styles from './clusterProgressStrip.module.css';

/** Collapsed preference, persisted so the strip stays where the user left it. */
const STRIP_COLLAPSED_KEY = 'nomifun:cluster-strip-collapsed';

function readInitialCollapsed(): boolean {
  try {
    return localStorage.getItem(STRIP_COLLAPSED_KEY) === '1';
  } catch {
    return false;
  }
}

/** 规划阶段 key → i18n（与 RunDecisionFeed.phaseNarration / DagCanvas 同源映射）。 */
const PHASE_I18N: Record<string, string> = {
  planning_started: 'orchestrator.run.thinking.phase.planningStarted',
  decomposing: 'orchestrator.run.thinking.phase.decomposing',
  assigning: 'orchestrator.run.thinking.phase.assigning',
  plan_ready: 'orchestrator.run.thinking.phase.planReady',
};

/** 一个节点是否算「已交付」（进度分子）。 */
function isSettledDone(status: string): boolean {
  return status === 'done' || status === 'completed';
}

/**
 * ClusterProgressStrip —「agent 集群」的会话内实时进度层（需求4）。
 *
 * 挂在会话内容区顶部（PlanApprovalBanner 下方），run 存在即显示：
 *  - 规划期：主 agent 的设计流程叙事（leadThinking 阶段逐条点亮）；
 *  - 执行期：每节点 chip（状态色点 + 标题 + 提问徽标），WS 驱动实时刷新，
 *    点击 chip 直接把该 worker 投影进内容区（复用 projectTask）；
 *  - 审批模式：任一节点挂起决策问题时，顶部亮琥珀横幅，点击直达该节点作答。
 *
 * 这是「每个阶段环节都有反馈、不可能断联」的保障层——纯 WS 事件驱动、不经过
 * 任何模型；lead 的叙事回执（BatchProgress 等）是它之上的语义层。可折叠
 * （localStorage 记忆）；无 Provider / 无 run 时渲染 null（伙伴等表面零足迹）。
 */
const ClusterProgressStrip: React.FC = () => {
  const { t } = useTranslation();
  const orch = useOrchestrationSafe();
  const [collapsed, setCollapsed] = useState<boolean>(readInitialCollapsed);

  useEffect(() => {
    try {
      localStorage.setItem(STRIP_COLLAPSED_KEY, collapsed ? '1' : '0');
    } catch {
      /* ignore */
    }
  }, [collapsed]);

  const detail = orch?.detail ?? null;
  const tasks = useMemo(() => detail?.tasks ?? [], [detail?.tasks]);
  const questionTasks = useMemo(
    () => tasks.filter((task) => task.status === 'needs_review' && task.pending_question?.trim()),
    [tasks]
  );

  const openTask = useCallback(
    (task: TRunTask) => {
      if (!orch || !orch.detail || !orch.runId) return;
      orch.projectTask({
        task,
        assignment: orch.detail.assignments.find((a) => a.task_id === task.id) ?? null,
        fleetMembers: orch.detail.fleet_members,
        runId: orch.runId,
        refetch: orch.refetch,
      });
    },
    [orch]
  );

  if (!orch || !orch.runId) return null;

  const runStatus = detail?.run.status ?? '';
  const statusMeta = STATUS_META[runStatus];
  const statusColor = statusMeta?.color ?? 'var(--color-text-3)';
  const statusLabel = statusMeta
    ? t(`orchestrator.run.status.${statusMeta.key}`, { defaultValue: runStatus })
    : t('orchestrator.run.status.unknown', { defaultValue: runStatus });
  const doneCount = tasks.filter((task) => isSettledDone(task.status)).length;
  const planning = tasks.length === 0;
  const phaseKeys = orch.leadThinking.phaseKeys;
  const latestPhase = phaseKeys.length > 0 ? phaseKeys[phaseKeys.length - 1] : null;

  const title = t('conversation.cluster.stripTitle', { defaultValue: 'agent 集群' });
  const progressText = planning
    ? (latestPhase && PHASE_I18N[latestPhase] ? t(PHASE_I18N[latestPhase]) : t('conversation.orchestration.planning', { defaultValue: '规划中…' }))
    : t('conversation.cluster.progress', {
        done: doneCount,
        total: tasks.length,
        defaultValue: '{{done}}/{{total}} 节点已交付',
      });

  // ── 折叠态：单行摘要（状态点 + 进度 + 提问计数），点击展开。──────────────
  if (collapsed) {
    return (
      <div
        role='button'
        tabIndex={0}
        className={styles.collapsed}
        aria-label={t('conversation.cluster.expand', { defaultValue: '展开集群进度' })}
        onClick={() => setCollapsed(false)}
        onKeyDown={(e) => {
          if (e.key === 'Enter' || e.key === ' ') {
            e.preventDefault();
            setCollapsed(false);
          }
        }}
      >
        <EveryUser theme='outline' size='14' strokeWidth={3} className={styles.titleIcon} />
        <span className={styles.title}>{title}</span>
        <span className={styles.statusDot} style={{ background: statusColor }} />
        <span className={styles.summaryText}>
          {statusLabel} · {progressText}
        </span>
        {questionTasks.length > 0 && (
          <span className={styles.questionCount}>
            <Help theme='filled' size='12' />
            {questionTasks.length}
          </span>
        )}
        <Down theme='outline' size='14' strokeWidth={3} className={styles.chev} />
      </div>
    );
  }

  return (
    <div className={styles.strip} data-testid='cluster-progress-strip'>
      {/* ── 头行：标题 + run 状态 + 进度 + 折叠 ─────────────────────────── */}
      <div className={styles.header}>
        <EveryUser theme='outline' size='14' strokeWidth={3} className={styles.titleIcon} />
        <span className={styles.title}>{title}</span>
        <span
          className={styles.statusPill}
          style={{ color: statusColor, background: 'color-mix(in srgb, currentColor 12%, transparent)' }}
        >
          <span className={styles.statusDot} style={{ background: statusColor }} />
          {statusLabel}
        </span>
        <span className={styles.progressText}>{progressText}</span>
        <div
          role='button'
          tabIndex={0}
          className={styles.collapseBtn}
          aria-label={t('conversation.cluster.collapse', { defaultValue: '收起集群进度' })}
          onClick={() => setCollapsed(true)}
          onKeyDown={(e) => {
            if (e.key === 'Enter' || e.key === ' ') {
              e.preventDefault();
              setCollapsed(true);
            }
          }}
        >
          <Up theme='outline' size='14' strokeWidth={3} />
        </div>
      </div>

      {/* ── 提问横幅（审批模式）：点击直达该节点作答。────────────────────── */}
      {questionTasks.map((task) => (
        <div
          key={task.id}
          role='button'
          tabIndex={0}
          className={styles.questionBanner}
          onClick={() => openTask(task)}
          onKeyDown={(e) => {
            if (e.key === 'Enter' || e.key === ' ') {
              e.preventDefault();
              openTask(task);
            }
          }}
        >
          <span className={styles.questionPulse}>
            <Help theme='filled' size='14' />
          </span>
          <span className={styles.questionText}>
            {t('conversation.cluster.questionBanner', {
              title: task.title,
              defaultValue: '节点「{{title}}」有决策问题待你作答',
            })}
            <b className={styles.questionPreview}>{task.pending_question}</b>
          </span>
          <span className={styles.questionCta}>
            {t('conversation.cluster.questionCta', { defaultValue: '进入作答 →' })}
          </span>
        </div>
      ))}

      {/* ── 规划叙事（计划未落地时）────────────────────────────────────── */}
      {planning && phaseKeys.length > 0 && (
        <div className={styles.phaseList} aria-live='polite'>
          {phaseKeys.slice(-3).map((key) => (
            <span key={key} className={styles.phaseLine}>
              {PHASE_I18N[key] ? t(PHASE_I18N[key]) : key}
            </span>
          ))}
        </div>
      )}

      {/* ── 节点 chips：横向滚动，实时状态，点击投影。──────────────────── */}
      {tasks.length > 0 && (
        <div className={`${styles.chips} nomi-roster-scroll`}>
          {tasks.map((task) => {
            const meta = taskStatusMeta(task.status);
            const isActive = orch.projectedTaskId === task.id;
            const hasQuestion = task.status === 'needs_review' && Boolean(task.pending_question?.trim());
            return (
              <button
                key={task.id}
                type='button'
                className={styles.chip}
                data-active={isActive ? 'true' : undefined}
                data-question={hasQuestion ? 'true' : undefined}
                title={`${task.title} · ${task.status}`}
                onClick={() => openTask(task)}
              >
                <span
                  className={`${styles.chipDot} ${meta.pulse ? styles.chipDotPulse : ''}`}
                  style={{ background: meta.color }}
                />
                <span className={styles.chipTitle}>{task.title}</span>
                {hasQuestion && (
                  <Help theme='filled' size='11' className={styles.chipQuestion} />
                )}
              </button>
            );
          })}
        </div>
      )}
    </div>
  );
};

export default ClusterProgressStrip;
