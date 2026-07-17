import { Spin, Tooltip } from '@arco-design/web-react';
import { Branch, FullScreen } from '@icon-park/react';
import {
  Background,
  BackgroundVariant,
  Controls,
  MarkerType,
  MiniMap,
  Panel,
  ReactFlow,
  type Edge,
  type ReactFlowInstance,
} from '@xyflow/react';
import '@xyflow/react/dist/style.css';
import React, { useEffect, useMemo, useRef, useState } from 'react';
import { useTranslation } from 'react-i18next';
import type {
  TAgentExecutionDetail,
  TExecutionAttempt,
  TExecutionParticipant,
  TExecutionStep,
} from '@/common/types/agentExecution/agentExecutionTypes';
import { latestAttemptForStep } from '@/common/types/agentExecution/agentExecutionTypes';
import type { ExecutionId, ExecutionStepId } from '@/common/types/ids';
import { resolveExecutionCanvasFocusStepId, summarizeExecutionText } from './executionCanvasPresentation';
import { collectExecutionDagFocus, executionDagEdgeId, layoutExecutionDag } from './layoutExecutionDag';
import { participantLogo, participantShortLabel } from './participantLabel';
import StepNode, { type JudgeWinner, type LoopState, normalizeStepKind, type StepFlowNode, type VerifyVerdict } from './nodes/StepNode';
import type { LeadThinkingState } from './useLeadThinking';
import './dag-canvas.css';

const NODE_TYPES = { step: StepNode } as const;
const FIT_VIEW_OPTIONS = { padding: 0.18, maxZoom: 1.35 } as const;
const NODE_WIDTH = 184;
const NODE_HEIGHT = 76;

const VERDICT_RE = /^VERDICT:\s+(PASS|FAIL)\s+\((\d+)\/(\d+)/;
const WINNER_RE = /^WINNER:\s+(?:candidate\s+(\d+)|none)/;
const WINNER_AGGREGATION_RE = /aggregate=(mean|borda)/;
const WINNER_JUDGES_RE = /judges=(\d+\/\d+)/;
const LOOP_RE = /^LOOP:\s+(DONE|FAILED)\s+\(reason=([a-z_]+),\s*iterations=(\d+),\s*max_iter=(\d+)\)/;

function parseVerifyVerdict(summary?: string): VerifyVerdict {
  const match = summary ? VERDICT_RE.exec(summary.trim()) : null;
  return match ? { pass: match[1] === 'PASS', tally: `${match[2]}/${match[3]}` } : { pass: null, tally: null };
}

function parseJudgeWinner(summary?: string): JudgeWinner {
  const match = summary ? WINNER_RE.exec(summary.trim()) : null;
  if (!match) return { winner: null, aggregate: null, judges: null };
  const aggregation = WINNER_AGGREGATION_RE.exec(summary ?? '');
  const judges = WINNER_JUDGES_RE.exec(summary ?? '');
  return {
    winner: match[1] == null ? null : Number(match[1]),
    aggregate: aggregation ? (aggregation[1] as 'mean' | 'borda') : null,
    judges: judges?.[1] ?? null,
  };
}

function parseLoopState(summary?: string): LoopState {
  const match = summary ? LOOP_RE.exec(summary.trim()) : null;
  return match
    ? {
        state: match[1] === 'DONE' ? 'done' : 'failed',
        reason: match[2],
        iterations: Number(match[3]),
        maxIter: Number(match[4]),
      }
    : { state: null, reason: null, iterations: null, maxIter: null };
}

const MINI_MAP_COLORS: Record<'light' | 'dark', Record<string, string>> = {
  light: {
    running: '#2f6bff',
    completed: '#aeb6c3',
    failed: '#dc2626',
    waiting_input: '#d97706',
    skipped: '#94a3b8',
    cancelled: '#94a3b8',
    pending: '#b4bccb',
  },
  dark: {
    running: '#5b8bff',
    completed: '#626b7a',
    failed: '#f04438',
    waiting_input: '#f59e0b',
    skipped: '#64748b',
    cancelled: '#64748b',
    pending: '#5a6273',
  },
};

export interface OpenStepPayload {
  /** Stable UI identity across immutable replacements of the same selected step. */
  projectionKey?: string;
  step: TExecutionStep;
  participant?: TExecutionParticipant;
  participants: TExecutionParticipant[];
  attempt?: TExecutionAttempt;
  executionId: ExecutionId;
  refetch: () => Promise<void>;
}

interface DagCanvasProps {
  executionId: ExecutionId;
  detail: TAgentExecutionDetail | null;
  loading: boolean;
  refetch: () => Promise<void>;
  onOpenStep: (payload: OpenStepPayload) => void;
  leadThinking?: LeadThinkingState;
  activeStepId?: ExecutionStepId | null;
}

const DagCanvas: React.FC<DagCanvasProps> = ({ executionId, detail, loading, refetch, onOpenStep, leadThinking, activeStepId }) => {
  const { t, i18n } = useTranslation();
  const flowInstance = useRef<ReactFlowInstance<StepFlowNode, Edge> | null>(null);
  const previousPlanRevision = useRef(detail?.execution.plan_revision);
  const [hoveredStepId, setHoveredStepId] = useState<ExecutionStepId | null>(null);
  const [overviewOpen, setOverviewOpen] = useState(false);
  const [theme, setTheme] = useState<'light' | 'dark'>(() =>
    document.documentElement.getAttribute('data-theme') === 'dark' ? 'dark' : 'light',
  );

  useEffect(() => {
    const updateTheme = () => setTheme(document.documentElement.getAttribute('data-theme') === 'dark' ? 'dark' : 'light');
    const observer = new MutationObserver(updateTheme);
    observer.observe(document.documentElement, {
      attributes: true,
      attributeFilter: ['data-theme'],
    });
    return () => observer.disconnect();
  }, []);

  const participantById = useMemo(
    () => new Map((detail?.participants ?? []).map((participant) => [participant.id, participant])),
    [detail?.participants],
  );

  const activeSteps = useMemo(() => (detail?.steps ?? []).filter((step) => step.superseded_in_revision == null), [detail?.steps]);
  const activeDependencies = useMemo(
    () => (detail?.dependencies ?? []).filter((dependency) => dependency.superseded_in_revision == null),
    [detail?.dependencies],
  );

  useEffect(() => {
    const revision = detail?.execution.plan_revision;
    const previousRevision = previousPlanRevision.current;
    previousPlanRevision.current = revision;
    if (revision == null || previousRevision == null || revision === previousRevision) return;

    const frame = requestAnimationFrame(() => {
      flowInstance.current?.fitView({ ...FIT_VIEW_OPTIONS, duration: 220 });
    });
    return () => cancelAnimationFrame(frame);
  }, [detail?.execution.plan_revision]);

  const latestAttemptByStep = useMemo(
    () => new Map(activeSteps.map((step) => [step.id, latestAttemptForStep(detail?.attempts ?? [], step.id)])),
    [activeSteps, detail?.attempts],
  );

  const titleByStepId = useMemo(
    () => new Map(activeSteps.map((step) => [step.id, step.title || t('agentExecution.step.untitled')])),
    [activeSteps, i18n.language, t],
  );

  const relationByStepId = useMemo(() => {
    const relations = new Map<string, { upstream: string[]; downstream: string[] }>();
    for (const step of activeSteps) relations.set(step.id, { upstream: [], downstream: [] });
    for (const dependency of activeDependencies) {
      const sourceTitle = titleByStepId.get(dependency.blocker_step_id);
      const targetTitle = titleByStepId.get(dependency.blocked_step_id);
      if (sourceTitle) relations.get(dependency.blocked_step_id)?.upstream.push(sourceTitle);
      if (targetTitle) relations.get(dependency.blocker_step_id)?.downstream.push(targetTitle);
    }
    return relations;
  }, [activeDependencies, activeSteps, titleByStepId]);

  const activeStepIds = useMemo(() => new Set(activeSteps.map((step) => step.id)), [activeSteps]);
  const focusStepId = resolveExecutionCanvasFocusStepId(activeStepIds, hoveredStepId, activeStepId);
  const focusedPath = useMemo(
    () => (focusStepId ? collectExecutionDagFocus(focusStepId, activeDependencies) : null),
    [activeDependencies, focusStepId],
  );

  const nodes = useMemo<StepFlowNode[]>(() => {
    const steps = activeSteps;
    const dependencies = activeDependencies;
    const fallbackPositions = layoutExecutionDag(steps, dependencies);

    return steps.map((step, index) => {
      const participant = step.assigned_participant_id ? participantById.get(step.assigned_participant_id) : undefined;
      const attempt = latestAttemptByStep.get(step.id);
      const displayKind = step.kind === 'agent' && step.agent_mode === 'synthesis' ? 'synthesis' : step.kind;
      const normalizedKind = normalizeStepKind(displayKind);
      const groupLabel = step.fanout_group?.trim() || undefined;
      const rawOutputSummary = attempt?.output_summary ?? undefined;
      const outputSummary = summarizeExecutionText(rawOutputSummary);
      const relations = relationByStepId.get(step.id) ?? { upstream: [], downstream: [] };
      const relationState = !focusedPath
        ? 'idle'
        : step.id === focusStepId
          ? 'focus'
          : focusedPath.stepIds.has(step.id)
            ? 'related'
            : 'muted';

      return {
        id: step.id,
        type: 'step',
        selected: activeStepId === step.id,
        // React Flow disables pointer events when selection, dragging and its
        // own node callbacks are all off. The compact card owns click/hover
        // interactions itself, so keep the wrapper interactive explicitly.
        style: { pointerEvents: 'all' },
        position:
          step.graph_x != null && step.graph_y != null
            ? { x: step.graph_x, y: step.graph_y }
            : (fallbackPositions[step.id] ?? { x: 0, y: 0 }),
        initialWidth: NODE_WIDTH,
        initialHeight: NODE_HEIGHT,
        data: {
          title: step.title || t('agentExecution.step.untitled', { defaultValue: '未命名任务' }),
          spec: summarizeExecutionText(step.spec, 220),
          outputSummary,
          errorSummary: summarizeExecutionText(attempt?.error, 180),
          status: step.status,
          statusLabel: t(`agentExecution.status.step.${step.status}`, {
            defaultValue: step.status,
          }),
          kind: displayKind,
          enterIndex: Math.min(index, 12),
          synthesisLabel: normalizedKind === 'synthesis' ? t('agentExecution.kind.synthesis', { defaultValue: '汇总' }) : undefined,
          verifyLabel: normalizedKind === 'verify' ? t('agentExecution.kind.verify', { defaultValue: '验证' }) : undefined,
          judgeLabel: normalizedKind === 'judge' ? t('agentExecution.kind.judge', { defaultValue: '评审' }) : undefined,
          loopLabel: normalizedKind === 'loop' ? t('agentExecution.kind.loop', { defaultValue: '循环' }) : undefined,
          verifyVerdict: normalizedKind === 'verify' ? parseVerifyVerdict(rawOutputSummary) : undefined,
          verifyVerdictLabels: {
            pass: t('agentExecution.verdict.pass', { defaultValue: '通过' }),
            fail: t('agentExecution.verdict.fail', { defaultValue: '未通过' }),
            pending: t('agentExecution.verdict.pending', {
              defaultValue: '验证中',
            }),
          },
          judgeWinner: normalizedKind === 'judge' ? parseJudgeWinner(rawOutputSummary) : undefined,
          judgeWinnerLabels: {
            winner: t('agentExecution.judge.winner', { defaultValue: '胜出' }),
            none: t('agentExecution.judge.none', { defaultValue: '暂无结果' }),
            pending: t('agentExecution.judge.pending', {
              defaultValue: '评审中',
            }),
          },
          loopState: normalizedKind === 'loop' ? parseLoopState(rawOutputSummary) : undefined,
          loopStateLabels: {
            done: t('agentExecution.loop.done', { defaultValue: '完成' }),
            failed: t('agentExecution.loop.failed', { defaultValue: '失败' }),
            iterating: t('agentExecution.loop.iterating', {
              defaultValue: '迭代中',
            }),
          },
          groupLabel,
          groupChipLabel: groupLabel
            ? t('agentExecution.kind.parallelGroup', {
                defaultValue: '并行：{{label}}',
                label: groupLabel,
              })
            : undefined,
          participantId: participant?.id,
          chipLabel: participantShortLabel(participant) ?? undefined,
          participantLogo: participantLogo(participant),
          roleLabel: step.role?.trim() || undefined,
          modelLabel: participant?.model?.trim() || undefined,
          locked: step.assignment_locked,
          attempt: attempt?.attempt_no ?? 0,
          tokens: attempt?.tokens ?? undefined,
          tokensLabel: t('agentExecution.step.tokens', {
            defaultValue: 'tokens',
          }),
          pendingQuestion: summarizeExecutionText(attempt?.question, 160),
          questionLabel: t('agentExecution.step.waitingInput', {
            defaultValue: '待回答',
          }),
          relationState,
          upstreamLabels: relations.upstream,
          downstreamLabels: relations.downstream,
          quickLookLabels: {
            result: t('agentExecution.canvas.result', { defaultValue: '结果摘要' }),
            upstream: t('agentExecution.canvas.upstream', { defaultValue: '上游' }),
            downstream: t('agentExecution.canvas.downstream', { defaultValue: '下游' }),
            model: t('agentExecution.canvas.model', { defaultValue: '模型' }),
            role: t('agentExecution.canvas.role', { defaultValue: '角色' }),
            attempts: t('agentExecution.canvas.attempts', { defaultValue: '尝试' }),
            inspect: t('agentExecution.canvas.inspect', { defaultValue: '点击查看完整执行记录' }),
          },
          onHoverChange: (active: boolean) => setHoveredStepId(active ? step.id : null),
          onOpen: () =>
            onOpenStep({
              step,
              participant,
              participants: detail?.participants ?? [],
              attempt,
              executionId,
              refetch,
            }),
        },
      };
    });
  }, [
    activeStepId,
    activeDependencies,
    activeSteps,
    detail?.participants,
    executionId,
    i18n.language,
    latestAttemptByStep,
    onOpenStep,
    participantById,
    focusedPath,
    focusStepId,
    refetch,
    relationByStepId,
    t,
  ]);

  const edges = useMemo<Edge[]>(() => {
    const statusById = new Map(activeSteps.map((step) => [step.id, step.status]));
    return activeDependencies.map((dependency) => {
      const id = executionDagEdgeId(dependency.blocker_step_id, dependency.blocked_step_id);
      const pathFocused = focusedPath?.edgeIds.has(id) ?? false;
      const dimmed = focusedPath != null && !pathFocused;
      const animated = !dimmed && statusById.get(dependency.blocked_step_id) === 'running';
      const stroke = pathFocused
        ? 'rgb(var(--primary-6))'
        : animated
          ? 'rgb(var(--primary-6))'
          : 'var(--border-base)';
      return {
        id,
        source: dependency.blocker_step_id,
        target: dependency.blocked_step_id,
        type: 'smoothstep',
        animated,
        className: [
          animated ? 'nomi-dag-edge-live' : '',
          pathFocused ? 'nomi-dag-edge-focused' : '',
          dimmed ? 'nomi-dag-edge-muted' : '',
        ]
          .filter(Boolean)
          .join(' '),
        style: {
          stroke,
          strokeWidth: pathFocused || animated ? 2 : 1.25,
          opacity: dimmed ? 0.16 : 1,
        },
        markerEnd: {
          type: MarkerType.ArrowClosed,
          color: stroke,
          width: 12,
          height: 12,
        },
        interactionWidth: 16,
        zIndex: pathFocused ? 2 : 0,
      };
    });
  }, [activeDependencies, activeSteps, focusedPath]);

  if (loading && !detail) {
    return <Spin className='m-auto' />;
  }
  if (!detail) {
    return (
      <div className='flex size-full flex-col items-center justify-center gap-12px text-t-tertiary'>
        <Branch theme='outline' size='24' />
        {t('agentExecution.detail.loadError', {
          defaultValue: '协作进度加载失败',
        })}
      </div>
    );
  }

  return (
    <div className='size-full min-h-0 flex flex-col'>
      <div className='flex-1 min-h-0'>
        {activeSteps.length === 0 ? (
          <div className='flex size-full flex-col items-center justify-center gap-10px px-24px text-center'>
            <Branch className='nomi-dag-pulse text-primary-6' theme='outline' size='26' />
            <strong>
              {t('agentExecution.thinking.title', {
                defaultValue: '正在准备协作计划',
              })}
            </strong>
            {leadThinking?.active && leadThinking.reasoning && (
              <span className='max-w-340px text-11px text-t-tertiary line-clamp-3'>{leadThinking.reasoning.slice(-160)}</span>
            )}
          </div>
        ) : (
          <ReactFlow
            className='nomi-dag-flow'
            onInit={(instance) => {
              flowInstance.current = instance;
            }}
            nodes={nodes}
            edges={edges}
            nodeTypes={NODE_TYPES}
            colorMode={theme}
            fitView
            fitViewOptions={FIT_VIEW_OPTIONS}
            minZoom={0.2}
            maxZoom={1.8}
            proOptions={{ hideAttribution: true }}
            nodesConnectable={false}
            nodesDraggable={false}
            nodesFocusable={false}
            edgesFocusable={false}
            elementsSelectable={false}
          >
            <Background variant={BackgroundVariant.Dots} gap={22} size={1.2} color={theme === 'dark' ? '#333333' : '#d1d5e5'} />
            <Controls showFitView={false} showInteractive={false} />
            <Panel position='top-right' className='nomi-dag-toolbar'>
              <Tooltip content={t('agentExecution.canvas.fit', { defaultValue: '适应视图' })} position='bottom'>
                <button
                  type='button'
                  className='nomi-dag-toolbar-button'
                  aria-label={t('agentExecution.canvas.fit', { defaultValue: '适应视图' })}
                  onClick={() => flowInstance.current?.fitView({ ...FIT_VIEW_OPTIONS, duration: 220 })}
                >
                  <FullScreen theme='outline' size='15' strokeWidth={3} />
                </button>
              </Tooltip>
              <Tooltip
                content={t(overviewOpen ? 'agentExecution.canvas.hideOverview' : 'agentExecution.canvas.showOverview', {
                  defaultValue: overviewOpen ? '收起概览' : '显示概览',
                })}
                position='bottom'
              >
                <button
                  type='button'
                  className='nomi-dag-toolbar-button'
                  data-active={overviewOpen ? 'true' : undefined}
                  aria-pressed={overviewOpen}
                  aria-label={t(overviewOpen ? 'agentExecution.canvas.hideOverview' : 'agentExecution.canvas.showOverview')}
                  onClick={() => setOverviewOpen((open) => !open)}
                >
                  <Branch theme='outline' size='15' strokeWidth={3} />
                </button>
              </Tooltip>
            </Panel>
            {overviewOpen && (
              <MiniMap
                pannable
                zoomable
                position='bottom-right'
                className='nomi-dag-minimap'
                maskColor={theme === 'dark' ? 'rgba(0,0,0,.55)' : 'rgba(255,255,255,.62)'}
                nodeColor={(node) => {
                  const nodeData = node.data as { status?: string; relationState?: string };
                  if (nodeData.relationState === 'focus') return 'rgb(var(--primary-6))';
                  const status = String(nodeData.status ?? 'pending');
                  return MINI_MAP_COLORS[theme][status] ?? MINI_MAP_COLORS[theme].pending;
                }}
                nodeStrokeWidth={2}
              />
            )}
          </ReactFlow>
        )}
      </div>
    </div>
  );
};

export default DagCanvas;
