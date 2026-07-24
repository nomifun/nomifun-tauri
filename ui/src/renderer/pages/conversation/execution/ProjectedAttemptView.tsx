import { Button, Dropdown, Input, Menu, Spin } from '@arco-design/web-react';
import { Brain, CheckOne, Comment, Down, Help, Left, Redo } from '@icon-park/react';
import React, { useCallback, useEffect, useMemo, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { ipcBridge } from '@/common';
import type { TChatConversation } from '@/common/config/storage';
import { tryParseEntityId, type ExecutionParticipantId } from '@/common/types/ids';
import type {
  TConfigureExecutionStep,
  TExecutionModelRef,
  TExecutionParticipant,
  TExecutionStep,
  TExecutionStepProfile,
} from '@/common/types/agentExecution/agentExecutionTypes';
import { latestAttemptForStep } from '@/common/types/agentExecution/agentExecutionTypes';
import RouteErrorBoundary from '@/renderer/components/layout/RouteErrorBoundary';
import { useArcoMessage } from '@/renderer/utils/ui/useArcoMessage';
import type { OpenStepPayload } from './DagCanvas';
import { useExecution } from './ExecutionContext';
import { participantShortLabel } from './participantLabel';
import ReadOnlyConversationView from './ReadOnlyConversationView';
import StepConfigBar from './StepConfigBar';
import styles from './projectedAttemptView.module.css';
import { refreshOnVersionConflict } from './refreshOnVersionConflict';
import { canSteerExecutionAttempt } from './executionStatusMeta';

type ProjectedAttemptViewProps = { payload: OpenStepPayload };
type StepConfigDraft = Pick<TConfigureExecutionStep, 'model' | 'preset_prompt'>;
const TOAST_CLASS = 'nomifun-message-passthrough';
const TOAST_OK_MS = 1500;
const TOAST_ERROR_MS = 2500;

function participantSupportsProfile(participant: TExecutionParticipant, profile: TExecutionStepProfile | null): boolean {
  if (!profile) return true;
  const allowedKinds = participant.constraints?.allowed_profile_kinds;
  if (allowedKinds && !allowedKinds.includes(profile.kind)) return false;
  if (profile.needs_vision && !participant.capability?.modalities.includes('vision')) return false;
  if (profile.kind === 'tool' && participant.capability?.tools !== true) return false;
  return true;
}

const ProjectedAttemptView: React.FC<ProjectedAttemptViewProps> = ({ payload }) => {
  const { t } = useTranslation();
  const { returnToMain, detail, refetch, projectStep } = useExecution();
  const [message, messageContext] = useArcoMessage();
  const [conversation, setConversation] = useState<TChatConversation | null>(null);
  const [loading, setLoading] = useState(false);
  const [retrying, setRetrying] = useState(false);
  const [adopting, setAdopting] = useState(false);
  const [reassigning, setReassigning] = useState(false);
  const [decisionAnswer, setDecisionAnswer] = useState('');
  const [answering, setAnswering] = useState(false);
  const [steerText, setSteerText] = useState('');
  const [steering, setSteering] = useState(false);

  const step = useMemo(
    () => detail?.steps.find((candidate) => candidate.step_id === payload.step.step_id) ?? payload.step,
    [detail?.steps, payload.step],
  );
  const attempts = detail?.attempts ?? (payload.attempt ? [payload.attempt] : []);
  const attempt = latestAttemptForStep(attempts, step.step_id) ?? payload.attempt;
  const conversationId = attempt?.conversation_id;
  const participants = detail?.participants ?? payload.participants;
  const assignableParticipants = participants.filter(
    (participant) => participant.retired_in_revision == null && participantSupportsProfile(participant, step.profile),
  );
  const currentParticipant = step.assigned_participant_id
    ? participants.find((participant) => participant.participant_id === step.assigned_participant_id)
    : payload.participant;

  useEffect(() => {
    setDecisionAnswer('');
    setSteerText('');
  }, [attempt?.attempt_id]);
  const canConfigure = step.kind === 'agent' && step.status === 'pending';
  const attemptActive = attempt ? ['queued', 'running', 'waiting_input'].includes(attempt.status) : false;
  const canSteer = canSteerExecutionAttempt(attempt?.status, detail?.execution.status);
  const executionCancelled = detail?.execution.status === 'cancelled';
  const canRetry =
    !executionCancelled &&
    (['completed', 'failed', 'skipped'].includes(step.status) || (step.status === 'pending' && step.dispatch_after != null));
  const canAdopt =
    conversationId != null && Boolean(detail?.execution) && !executionCancelled && step.status !== 'cancelled' && !attemptActive;

  const projectReplacementStep = useCallback(
    (replacement: TExecutionStep) => {
      projectStep({
        ...payload,
        projectionKey: payload.projectionKey ?? payload.step.step_id,
        step: replacement,
        participant: participants.find(
          (participant) => participant.participant_id === replacement.assigned_participant_id,
        ),
        participants,
        attempt: undefined,
        refetch,
      });
    },
    [participants, payload, projectStep, refetch],
  );

  const applyStepConfig = useCallback(
    async (patch: StepConfigDraft) => {
      try {
        const replacement = await ipcBridge.agentExecution.configure.invoke({
          execution_id: payload.executionId,
          step_id: step.step_id,
          updates: {
            model: patch.model,
            preset_prompt: patch.preset_prompt,
            expected_execution_version: detail?.execution.version ?? 0,
            expected_step_version: step.version,
          },
        });
        projectReplacementStep(replacement);
        await refetch();
      } catch (error) {
        await refreshOnVersionConflict(error, refetch);
        throw error;
      }
    },
    [
      detail?.execution.version,
      payload.executionId,
      projectReplacementStep,
      refetch,
      step.step_id,
      step.version,
    ],
  );

  const applyModel = useCallback((model: TExecutionModelRef | null) => applyStepConfig({ model }), [applyStepConfig]);
  const applyPreset = useCallback((preset: string) => applyStepConfig({ preset_prompt: preset.trim() || null }), [applyStepConfig]);

  const reassign = async (participantId: ExecutionParticipantId) => {
    if (reassigning || participantId === step.assigned_participant_id) return;
    setReassigning(true);
    try {
      const replacement = await ipcBridge.agentExecution.reassign.invoke({
        execution_id: payload.executionId,
        step_id: step.step_id,
        updates: {
          participant_id: participantId,
          locked: true,
          expected_execution_version: detail?.execution.version ?? 0,
          expected_step_version: step.version,
        },
      });
      message.success({
        content: t('agentExecution.assignment.updated', {
          defaultValue: '协作者已更新',
        }),
        duration: TOAST_OK_MS,
        className: TOAST_CLASS,
      });
      projectReplacementStep(replacement);
      await refetch();
    } catch (error) {
      await refreshOnVersionConflict(error, refetch);
      message.error({
        content: t('agentExecution.assignment.error', {
          defaultValue: '更换协作者失败：{{error}}',
          error: String(error),
        }),
        duration: TOAST_ERROR_MS,
        className: TOAST_CLASS,
      });
    } finally {
      setReassigning(false);
    }
  };

  const participantMenu = (
    <Menu
      selectedKeys={step.assigned_participant_id ? [String(step.assigned_participant_id)] : []}
      onClickMenuItem={(key) => {
        const participantId = tryParseEntityId('execution-participant', key);
        if (participantId != null) void reassign(participantId);
      }}
    >
      {assignableParticipants.map((participant) => (
        <Menu.Item key={participant.participant_id}>
          {participantShortLabel(participant) ?? participant.participant_id}
        </Menu.Item>
      ))}
    </Menu>
  );

  useEffect(() => {
    if (conversationId == null) {
      setConversation(null);
      setLoading(false);
      return;
    }
    let cancelled = false;
    setLoading(true);
    void ipcBridge.conversation.get
      .invoke({ conversation_id: conversationId })
      .then((next) => !cancelled && setConversation(next ?? null))
      .catch((error) => {
        console.error('[ProjectedAttemptView] Failed to load conversation:', error);
        if (!cancelled) setConversation(null);
      })
      .finally(() => !cancelled && setLoading(false));
    return () => {
      cancelled = true;
    };
  }, [conversationId]);

  const retry = async () => {
    if (retrying) return;
    setRetrying(true);
    try {
      await ipcBridge.agentExecution.retry.invoke({
        execution_id: payload.executionId,
        step_id: step.step_id,
        updates: {
          expected_execution_version: detail?.execution.version ?? 0,
          expected_step_version: step.version,
        },
      });
      await payload.refetch();
      message.success({
        content: t('agentExecution.retry.ok', { defaultValue: '任务已重试' }),
        duration: TOAST_OK_MS,
        className: TOAST_CLASS,
      });
    } catch (error) {
      await refreshOnVersionConflict(error, refetch);
      message.error({
        content: t('agentExecution.retry.error', {
          defaultValue: '重试失败：{{error}}',
          error: String(error),
        }),
        duration: TOAST_ERROR_MS,
        className: TOAST_CLASS,
      });
    } finally {
      setRetrying(false);
    }
  };

  const adopt = async () => {
    if (adopting) return;
    setAdopting(true);
    try {
      await ipcBridge.agentExecution.adopt.invoke({
        execution_id: payload.executionId,
        step_id: step.step_id,
        updates: {
          expected_execution_version: detail?.execution.version ?? 0,
          expected_step_version: step.version,
        },
      });
      await payload.refetch();
      message.success({
        content: t('agentExecution.adopt.ok', {
          defaultValue: '已采用当前结果',
        }),
        duration: TOAST_OK_MS,
        className: TOAST_CLASS,
      });
    } catch (error) {
      await refreshOnVersionConflict(error, refetch);
      message.error({
        content: t('agentExecution.adopt.error', {
          defaultValue: '采用失败：{{error}}',
          error: String(error),
        }),
        duration: TOAST_ERROR_MS,
        className: TOAST_CLASS,
      });
    } finally {
      setAdopting(false);
    }
  };

  const answerDecision = async () => {
    const answer = decisionAnswer.trim();
    if (answering || !answer || !attempt || !detail?.execution) return;
    setAnswering(true);
    try {
      await ipcBridge.agentExecution.answerDecision.invoke({
        execution_id: payload.executionId,
        step_id: step.step_id,
        attempt_id: attempt.attempt_id,
        updates: {
          answer,
          expected_execution_version: detail.execution.version,
          expected_step_version: step.version,
          expected_attempt_version: attempt.version,
        },
      });
      setDecisionAnswer('');
      await refetch();
      message.success({
        content: t('agentExecution.question.ok', {
          defaultValue: '决定已提交',
        }),
        duration: TOAST_OK_MS,
        className: TOAST_CLASS,
      });
    } catch (error) {
      await refreshOnVersionConflict(error, refetch);
      message.error({
        content: t('agentExecution.question.error', {
          defaultValue: '提交决定失败：{{error}}',
          error: String(error),
        }),
        duration: TOAST_ERROR_MS,
        className: TOAST_CLASS,
      });
    } finally {
      setAnswering(false);
    }
  };

  const steer = async () => {
    const text = steerText.trim();
    if (steering || !text || !canSteer || !detail?.execution) return;
    setSteering(true);
    try {
      await ipcBridge.agentExecution.steer.invoke({
        execution_id: payload.executionId,
        step_id: step.step_id,
        updates: {
          text,
          expected_execution_version: detail.execution.version,
          expected_step_version: step.version,
        },
      });
      setSteerText('');
      await refetch();
      message.success({
        content: t('agentExecution.steer.ok'),
        duration: TOAST_OK_MS,
        className: TOAST_CLASS,
      });
    } catch (error) {
      await refreshOnVersionConflict(error, refetch);
      message.error({
        content: t('agentExecution.steer.error', { error: String(error) }),
        duration: TOAST_ERROR_MS,
        className: TOAST_CLASS,
      });
    } finally {
      setSteering(false);
    }
  };

  return (
    <div className={styles.root}>
      {messageContext}
      <div className={styles.banner}>
        <div className={styles.bannerLead}>
          <span className={styles.bannerBadge}>
            <Comment theme='outline' size='13' />
          </span>
          <span className={styles.bannerEyebrow}>
            {t('agentExecution.projection.viewing', {
              defaultValue: '当前任务',
            })}
          </span>
          <span className={styles.bannerTitle}>{step.title}</span>
        </div>
        <div className={styles.bannerActions}>
          {canConfigure && assignableParticipants.length > 1 && (
            <Dropdown trigger='click' position='br' droplist={participantMenu}>
              <div role='button' tabIndex={0} className={styles.action}>
                <Brain theme='outline' size='13' />
                <span className='max-w-[140px] truncate'>
                  {participantShortLabel(currentParticipant) ??
                    t('agentExecution.assignment.change', {
                      defaultValue: '更换协作者',
                    })}
                </span>
                <Down theme='outline' size='12' />
              </div>
            </Dropdown>
          )}
          {canAdopt && (
            <div
              role='button'
              tabIndex={0}
              aria-disabled={adopting}
              className={`${styles.action} ${styles.actionAdopt}`}
              onClick={adopting ? undefined : () => void adopt()}
            >
              <CheckOne theme='outline' size='13' />
              {t('agentExecution.adopt.button', {
                defaultValue: '采用当前结果',
              })}
            </div>
          )}
          {canRetry && (
            <div
              role='button'
              tabIndex={0}
              aria-disabled={retrying}
              className={styles.action}
              onClick={retrying ? undefined : () => void retry()}
            >
              <Redo theme='outline' size='13' />
              {t('agentExecution.retry.button', { defaultValue: '重试' })}
            </div>
          )}
          <div role='button' tabIndex={0} className={`${styles.action} ${styles.actionPrimary}`} onClick={returnToMain}>
            <Left theme='outline' size='13' />
            {t('agentExecution.projection.return', {
              defaultValue: '返回主对话',
            })}
          </div>
        </div>
      </div>

      {canSteer && (
        <div className={styles.steerBar}>
          <Input.TextArea
            value={steerText}
            onChange={setSteerText}
            autoSize={{ minRows: 1, maxRows: 3 }}
            placeholder={t('agentExecution.steer.placeholder')}
          />
          <Button
            type='primary'
            size='small'
            loading={steering}
            disabled={!steerText.trim()}
            onClick={() => void steer()}
          >
            {t('agentExecution.steer.submit')}
          </Button>
        </div>
      )}

      {step.status === 'waiting_input' && attempt?.question?.trim() && (
        <div className={styles.questionBanner} role='status'>
          <span className={styles.questionIcon}>
            <Help theme='filled' size='15' />
          </span>
          <div className={styles.questionBody}>
            <span className={styles.questionEyebrow}>
              {t('agentExecution.question.title', {
                defaultValue: '需要你的决定',
              })}
            </span>
            <span className={styles.questionContent}>{attempt.question}</span>
            <span className={styles.questionHint}>
              {t('agentExecution.question.hint', {
                defaultValue: '直接提交你的选择，协作者会据此继续任务。',
              })}
            </span>
            <div className={styles.questionActions}>
              <Input.TextArea
                value={decisionAnswer}
                onChange={setDecisionAnswer}
                autoSize={{ minRows: 1, maxRows: 4 }}
                placeholder={t('agentExecution.question.placeholder', {
                  defaultValue: '输入你的决定…',
                })}
              />
              <Button
                type='primary'
                size='small'
                loading={answering}
                disabled={!decisionAnswer.trim() || !detail?.execution}
                onClick={() => void answerDecision()}
              >
                {t('agentExecution.question.submit', {
                  defaultValue: '提交决定',
                })}
              </Button>
            </div>
          </div>
        </div>
      )}

      <div className={styles.body}>
        <RouteErrorBoundary>
          {canConfigure ? (
            <StepConfigBar
              step={step}
              participant={currentParticipant}
              onApplyModel={applyModel}
              onApplyPreset={applyPreset}
            />
          ) : conversationId == null ? (
            <div className={styles.center}>
              <Comment theme='outline' size='26' />
              <div className={styles.emptyTitle}>
                {t('agentExecution.transcript.notStarted', {
                  defaultValue: '协作者尚未开始',
                })}
              </div>
            </div>
          ) : loading ? (
            <Spin loading className='flex flex-1 items-center justify-center' />
          ) : conversation ? (
            <ReadOnlyConversationView conversation={conversation} agent_name={step.title} />
          ) : null}
        </RouteErrorBoundary>
      </div>
    </div>
  );
};

export default ProjectedAttemptView;
