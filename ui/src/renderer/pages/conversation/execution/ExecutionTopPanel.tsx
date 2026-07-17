import { Modal, Spin } from '@arco-design/web-react';
import { EveryUser, Help, Loading } from '@icon-park/react';
import React, { Suspense, useCallback, useEffect, useMemo, useRef, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { ipcBridge } from '@/common';
import { latestAttemptForStep } from '@/common/types/agentExecution/agentExecutionTypes';
import { refreshOnVersionConflict } from './refreshOnVersionConflict';
import { useArcoMessage } from '@/renderer/utils/ui/useArcoMessage';
import ExecutionPlanEditor, { type ExecutionModelPoolSelection } from './ExecutionPlanEditor';
import ExecutionAdjustBox from './ExecutionAdjustBox';
import { ExecutionControls } from './ExecutionControls';
import { useExecution } from './ExecutionContext';
import type { OpenStepPayload } from './DagCanvas';
import { isTerminalExecutionStatus } from './executionStatusMeta';
import ParticipantProfilePanel from './ParticipantProfilePanel';
import { useExecutionModelPool } from './useExecutionModelPool';
import styles from './executionTopPanel.module.css';

const DagCanvas = React.lazy(() => import('./DagCanvas'));
const CANVAS_WIDTH_KEY = 'nomifun:execution-canvas-width';
const MIN_WIDTH = 320;
const MAX_WIDTH = 860;
const DEFAULT_WIDTH = 480;
const MIN_CHAT_WIDTH = 360;

function availableMaxWidth(containerWidth?: number): number {
  if (typeof window === 'undefined' || window.matchMedia('(max-width: 768px)').matches) return MAX_WIDTH;
  const layoutWidth = containerWidth && containerWidth > 0 ? containerWidth : window.innerWidth;
  return Math.max(MIN_WIDTH, Math.min(MAX_WIDTH, layoutWidth - MIN_CHAT_WIDTH));
}

function initialWidth(): number {
  try {
    const persisted = Number(localStorage.getItem(CANVAS_WIDTH_KEY));
    if (Number.isFinite(persisted) && persisted >= MIN_WIDTH && persisted <= MAX_WIDTH) {
      return Math.min(persisted, availableMaxWidth());
    }
  } catch {
    // localStorage may be unavailable in embedded surfaces.
  }
  return Math.min(DEFAULT_WIDTH, availableMaxWidth());
}

const ExecutionTopPanel: React.FC = () => {
  const { t } = useTranslation();
  const [message, messageContext] = useArcoMessage();
  const execution = useExecution();
  const { buildModelPool } = useExecutionModelPool();
  const [width, setWidth] = useState(initialWidth);
  const panelRef = useRef<HTMLDivElement | null>(null);
  const dragState = useRef<{ startX: number; startWidth: number } | null>(null);
  const [replanOpen, setReplanOpen] = useState(false);
  const [replanGoal, setReplanGoal] = useState('');
  const [replanModelPool, setReplanModelPool] = useState<ExecutionModelPoolSelection>({
    mode: 'automatic',
    single: '',
    range: [],
  });
  const [replanSubmitting, setReplanSubmitting] = useState(false);

  const {
    executionId,
    detail,
    leadThinking,
    loading,
    refetch,
    projectStep,
    projectedStepId,
    returnToMain,
    canvasOpen,
    setCanvasOpen,
  } = execution;

  useEffect(() => {
    try {
      localStorage.setItem(CANVAS_WIDTH_KEY, String(width));
    } catch {
      // Ignore unavailable storage.
    }
  }, [width]);

  const getAvailableMaxWidth = useCallback(
    () => availableMaxWidth(panelRef.current?.parentElement?.getBoundingClientRect().width),
    [],
  );

  useEffect(() => {
    const clampWidth = () => setWidth((current) => Math.min(current, getAvailableMaxWidth()));
    clampWidth();
    const parent = panelRef.current?.parentElement;
    const observer = parent && typeof ResizeObserver !== 'undefined' ? new ResizeObserver(clampWidth) : null;
    if (parent) observer?.observe(parent);
    window.addEventListener('resize', clampWidth);
    return () => {
      observer?.disconnect();
      window.removeEventListener('resize', clampWidth);
    };
  }, [getAvailableMaxWidth]);

  const onResizeStart = useCallback(
    (event: React.PointerEvent<HTMLDivElement>) => {
      if (event.button !== 0) return;
      event.preventDefault();
      dragState.current = { startX: event.clientX, startWidth: width };
      event.currentTarget.setPointerCapture(event.pointerId);
    },
    [width],
  );

  const onResizeMove = useCallback((event: React.PointerEvent<HTMLDivElement>) => {
    if (!dragState.current) return;
    const next = dragState.current.startWidth + dragState.current.startX - event.clientX;
    setWidth(Math.min(getAvailableMaxWidth(), Math.max(MIN_WIDTH, next)));
  }, [getAvailableMaxWidth]);

  const onResizeKeyDown = useCallback((event: React.KeyboardEvent<HTMLDivElement>) => {
    const step = event.shiftKey ? 64 : 24;
    if (event.key === 'ArrowLeft') {
      event.preventDefault();
      setWidth((current) => Math.min(getAvailableMaxWidth(), current + step));
    } else if (event.key === 'ArrowRight') {
      event.preventDefault();
      setWidth((current) => Math.max(MIN_WIDTH, current - step));
    } else if (event.key === 'Home') {
      event.preventDefault();
      setWidth(MIN_WIDTH);
    } else if (event.key === 'End') {
      event.preventDefault();
      setWidth(getAvailableMaxWidth());
    }
  }, [getAvailableMaxWidth]);

  const onResizeEnd = useCallback((event: React.PointerEvent<HTMLDivElement>) => {
    dragState.current = null;
    if (event.currentTarget.hasPointerCapture(event.pointerId)) {
      event.currentTarget.releasePointerCapture(event.pointerId);
    }
  }, []);

  const openReplan = useCallback(() => {
    setReplanGoal(detail?.execution.goal ?? '');
    setReplanModelPool({ mode: 'automatic', single: '', range: [] });
    setReplanOpen(true);
  }, [detail?.execution.goal]);

  const submitReplan = useCallback(
    async (goal: string) => {
      if (!executionId) return;
      const modelPool = buildModelPool(replanModelPool);
      if (!modelPool) {
        message.warning(
          t('agentExecution.editor.model.required', {
            defaultValue: '请选择可用模型',
          }),
        );
        return;
      }
      setReplanSubmitting(true);
      try {
        await ipcBridge.agentExecution.replan.invoke({
          id: executionId,
          updates: {
            goal: goal.trim(),
            model_pool: modelPool,
            expected_version: detail?.execution.version ?? 0,
          },
        });
        returnToMain();
        await refetch();
        setReplanOpen(false);
        message.success(
          t('agentExecution.controls.replanOk', {
            defaultValue: '协作计划已更新',
          }),
        );
      } catch (error) {
        await refreshOnVersionConflict(error, refetch);
        message.error(
          t('agentExecution.controls.replanError', {
            defaultValue: '更新失败：{{error}}',
            error: String(error),
          }),
        );
      } finally {
        setReplanSubmitting(false);
      }
    },
    [buildModelPool, detail?.execution.version, executionId, message, refetch, replanModelPool, returnToMain, t],
  );

  const latestAttemptByStep = useMemo(
    () => new Map((detail?.steps ?? []).map((step) => [step.id, latestAttemptForStep(detail?.attempts ?? [], step.id)])),
    [detail?.attempts, detail?.steps],
  );

  const openStep = useCallback(
    (payload: OpenStepPayload) => {
      projectStep(payload);
      if (window.matchMedia('(max-width: 768px)').matches) setCanvasOpen(false);
    },
    [projectStep, setCanvasOpen],
  );

  if (!executionId || !canvasOpen) return null;

  const status = detail?.execution.status ?? '';
  const showControls = !isTerminalExecutionStatus(status);
  const steps = (detail?.steps ?? []).filter((step) => step.superseded_in_revision == null);
  const runningCount = steps.filter((step) => step.status === 'running').length;
  const completedCount = steps.filter((step) => step.status === 'completed').length;
  const failedCount = steps.filter((step) => step.status === 'failed').length;
  const completionPercent = steps.length > 0 ? Math.round((completedCount / steps.length) * 100) : 0;
  const offersReusableProfiles = status === 'completed' || status === 'completed_with_failures';
  const waitingSteps = steps.filter(
    (step) => step.status === 'waiting_input' && Boolean(latestAttemptByStep.get(step.id)?.question?.trim()),
  );
  const maxPanelWidth = getAvailableMaxWidth();

  return (
    <div ref={panelRef} className={`${styles.panel} shrink-0 flex flex-col`} style={{ width: Math.min(width, maxPanelWidth) }}>
      {messageContext}
      <div
        className={styles.resizeHandle}
        role='separator'
        tabIndex={0}
        aria-orientation='vertical'
        aria-valuemin={MIN_WIDTH}
        aria-valuemax={maxPanelWidth}
        aria-valuenow={Math.min(width, maxPanelWidth)}
        aria-label={t('agentExecution.panel.resize', {
          defaultValue: '调整协作面板宽度',
        })}
        onPointerDown={onResizeStart}
        onPointerMove={onResizeMove}
        onPointerUp={onResizeEnd}
        onPointerCancel={onResizeEnd}
        onKeyDown={onResizeKeyDown}
      />

      {(showControls || leadThinking.active) && (
        <div className={`${styles.header} flex flex-wrap items-center justify-end gap-x-10px gap-y-6px`}>
          {leadThinking.active && (
            <span className='inline-flex items-center gap-5px text-11px text-primary-6'>
              <Loading theme='outline' size='12' className='animate-spin' />
              {t('agentExecution.thinking.short', { defaultValue: '规划中…' })}
            </span>
          )}

          {showControls && (
            <ExecutionControls
              executionId={executionId}
              executionVersion={detail?.execution.version ?? 0}
              status={status}
              inFlightCount={runningCount}
              refetch={refetch}
              onReplan={openReplan}
            />
          )}
        </div>
      )}

      {steps.length > 0 && (
        <div className={styles.canvasProgress} data-testid='execution-canvas-progress'>
          <div className={styles.canvasProgressHeader}>
            <EveryUser theme='outline' size='14' className={styles.canvasProgressIcon} />
            <span className={styles.canvasProgressText}>
              {t('agentExecution.progress.summary', {
                defaultValue: '{{done}}/{{total}} 个任务已完成',
                done: completedCount,
                total: steps.length,
              })}
            </span>
            {runningCount > 0 && (
              <span className={styles.progressMetric} data-tone='active'>
                {t('agentExecution.progress.running', { count: runningCount })}
              </span>
            )}
            {failedCount > 0 && (
              <span className={styles.progressMetric} data-tone='danger'>
                {t('agentExecution.progress.failed', { count: failedCount })}
              </span>
            )}
            <span
              className={styles.progressTrack}
              role='progressbar'
              aria-label={t('agentExecution.progress.title', { defaultValue: '协作进度' })}
              aria-valuemin={0}
              aria-valuemax={steps.length}
              aria-valuenow={completedCount}
            >
              <span className={styles.progressBar} style={{ width: `${completionPercent}%` }} />
            </span>
            {offersReusableProfiles && detail && <ParticipantProfilePanel detail={detail} />}
          </div>

          {waitingSteps.map((step) => {
            const question = latestAttemptByStep.get(step.id)?.question;
            return (
              <button
                key={step.id}
                type='button'
                className={styles.questionBanner}
                onClick={() => {
                  const participant = step.assigned_participant_id
                    ? detail?.participants.find((item) => item.id === step.assigned_participant_id)
                    : undefined;
                  openStep({
                    step,
                    participant,
                    participants: detail?.participants ?? [],
                    attempt: latestAttemptByStep.get(step.id),
                    executionId,
                    refetch,
                  });
                }}
              >
                <span className={styles.questionPulse}>
                  <Help theme='filled' size='14' />
                </span>
                <span className={styles.questionText}>
                  {t('agentExecution.progress.question', {
                    defaultValue: '任务「{{title}}」需要你的决定',
                    title: step.title,
                  })}
                  <b className={styles.questionPreview}>{question}</b>
                </span>
              </button>
            );
          })}
        </div>
      )}

      <div className={`${styles.body} flex-1 min-h-0`}>
        <Suspense fallback={<Spin className='m-auto' />}>
          <DagCanvas
            executionId={executionId}
            detail={detail}
            loading={loading}
            refetch={refetch}
            onOpenStep={openStep}
            leadThinking={leadThinking}
            activeStepId={projectedStepId}
          />
        </Suspense>
      </div>

      {detail && ['running', 'paused', 'awaiting_approval'].includes(status) && (
        <ExecutionAdjustBox detail={detail} refetch={refetch} onApplied={returnToMain} />
      )}

      <Modal
        title={t('agentExecution.controls.replan', {
          defaultValue: '重新规划',
        })}
        visible={replanOpen}
        footer={null}
        onCancel={() => !replanSubmitting && setReplanOpen(false)}
        maskClosable={!replanSubmitting}
        autoFocus={false}
        unmountOnExit
        style={{ width: 'min(640px, calc(100vw - 32px))' }}
      >
        <ExecutionPlanEditor
          fluid
          value={replanGoal}
          onChange={setReplanGoal}
          onSubmit={submitReplan}
          submitting={replanSubmitting}
          placeholder={t('agentExecution.editor.goalPlaceholder', {
            defaultValue: '描述要重新规划的目标…',
          })}
          label={t('agentExecution.controls.replan', {
            defaultValue: '重新规划',
          })}
          showModelPool
          modelPool={replanModelPool}
          onModelPoolChange={setReplanModelPool}
        />
      </Modal>
    </div>
  );
};

export default ExecutionTopPanel;
