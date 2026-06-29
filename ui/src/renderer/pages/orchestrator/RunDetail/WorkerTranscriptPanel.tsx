/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import React, { useEffect, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Button, Drawer, Input, Popconfirm, Select, Spin, Switch, Tooltip } from '@arco-design/web-react';
import { Comment, Lightning, Redo, Send } from '@icon-park/react';
import { ipcBridge } from '@/common';
import type { TChatConversation } from '@/common/config/storage';
import type { TAssignment, TFleetMember } from '@/common/types/orchestrator/orchestratorTypes';
import { useArcoMessage } from '@/renderer/utils/ui/useArcoMessage';
import ReadOnlyConversationView from './ReadOnlyConversationView';
import type { OpenTaskPayload } from './DagCanvas';
import { memberLogo, memberShortLabel } from './memberLabel';
import { taskStatusMeta } from './nodes/TaskNode';
import { formatDuration, taskHasTiming } from './runFormat';

type WorkerTranscriptPanelProps = {
  /** The clicked DAG node's payload (task + assignment + fleet snapshot + refetch).
   * Null = nothing to show (drawer closed). */
  open: OpenTaskPayload | null;
  onClose: () => void;
};

/** Longest persona preview shown inline; the rest lives behind a tooltip. */
const PERSONA_PREVIEW_LEN = 140;

/** One reassign Select option: friendly agent/model label + role hint, plus a logo. */
const memberOption = (m: TFleetMember, roleLabel: (role: string) => string) => {
  const label = memberShortLabel(m) ?? m.id;
  const role = m.role_hint ? roleLabel(m.role_hint) : null;
  return { member: m, label, role, logo: memberLogo(m) };
};

/** A single label→value row in the config block (label muted-left, value right). */
const ConfigRow: React.FC<{ label: string; children: React.ReactNode }> = ({ label, children }) => (
  <div className='flex items-start gap-12px'>
    <span className='w-44px shrink-0 pt-1px text-12px font-500 leading-18px text-t-tertiary'>{label}</span>
    <div className='min-w-0 flex-1 text-12px leading-18px text-t-primary'>{children}</div>
  </div>
);

/**
 * Side drawer for one orchestration task. Stacked sections, top → bottom:
 *
 *  1. Agent config — WHO is on this task and HOW it's set up: role, model,
 *     persona (truncated), skills, live status, plus the WHY (rationale) and the
 *     management controls to **reassign** the task to another fleet member and
 *     **lock** the assignment against the auto-router. Reassign/lock call
 *     `PUT …/assignment` then refetch so the canvas + panel resync.
 *  2. Worker transcript — the live, read-only conversation record (mirrors
 *     SubagentDrawer: ReadOnlyConversationView with the send box hidden). Shown
 *     only once a worker has picked up the task and a conversation exists.
 *  3. Steer — mid-turn inject into a running worker's live conversation.
 *
 * Data comes straight off `OpenTaskPayload` (task + assignment + the run's fleet
 * snapshot, already enriched with description/system_prompt/enabled_skills in
 * P4), so the panel never re-fetches the run to render config.
 *
 * `TRunTask.conversation_id` is already the backend INTEGER id, passed straight
 * through with no conversion (unlike TeamAgent.conversation_id, a string).
 */
const WorkerTranscriptPanel: React.FC<WorkerTranscriptPanelProps> = ({ open, onClose }) => {
  const { t } = useTranslation();
  const [message, ctx] = useArcoMessage();
  const [conversation, setConversation] = useState<TChatConversation | null>(null);
  const [loading, setLoading] = useState(false);
  const [saving, setSaving] = useState(false);
  const [steerText, setSteerText] = useState('');
  const [steering, setSteering] = useState(false);
  // Editable 意图/prompt draft (UC-2a-fe). Mirrors the task's `spec`; saved back
  // via updateTaskSpec, then a later rerun re-executes the node with the new spec.
  const [specDraft, setSpecDraft] = useState('');
  const [specSaving, setSpecSaving] = useState(false);
  // Guards the rerun button against double-click while the request is in flight.
  const [rerunning, setRerunning] = useState(false);

  const task = open?.task ?? null;
  const assignment = open?.assignment ?? null;
  const conversationId = task?.conversation_id;
  // Steer is only meaningful for a task actively running on a worker with a live
  // conversation to inject into.
  const canSteer = task?.status === 'running' && conversationId !== undefined;
  // The spec and rerun controls are gated on the same backend invariant: neither
  // edits nor a reset are allowed while the node is actively running.
  const isRunning = task?.status === 'running';
  const specValue = task?.spec ?? '';
  const specDirty = specDraft !== specValue;

  // Reset the steer + spec drafts whenever the inspected task changes.
  useEffect(() => {
    setSteerText('');
  }, [task?.id]);

  // Re-seed the spec draft from the task whenever the inspected task changes OR
  // the upstream spec value updates (e.g. after a save round-trips via refetch).
  useEffect(() => {
    setSpecDraft(specValue);
  }, [task?.id, specValue]);

  useEffect(() => {
    if (!task || conversationId === undefined) {
      setConversation(null);
      return;
    }
    let cancelled = false;
    setLoading(true);
    // `TRunTask.conversation_id` is already the backend INTEGER id — no conversion.
    void ipcBridge.conversation.get
      .invoke({ id: conversationId })
      .then((conv) => {
        if (!cancelled) setConversation((conv as TChatConversation | null) ?? null);
      })
      .catch((e) => {
        console.error('[WorkerTranscriptPanel] load conversation failed:', e);
        if (!cancelled) setConversation(null);
      })
      .finally(() => {
        if (!cancelled) setLoading(false);
      });
    return () => {
      cancelled = true;
    };
  }, [task, conversationId]);

  const roleLabel = (role: string) =>
    t(`orchestrator.run.role.${role}` as 'orchestrator.run.role.planner', { defaultValue: role });

  // Apply a reassignment / lock change, then refetch so the canvas + panel sync.
  const applyReassign = async (memberId: string, locked: boolean) => {
    if (!open || !assignment) return;
    setSaving(true);
    try {
      await ipcBridge.orchestrator.runs.reassign.invoke({
        run_id: open.runId,
        task_id: open.task.id,
        updates: { member_id: memberId, locked },
      });
      message.success(t('orchestrator.run.assign.reassignSuccess'));
      await open.refetch();
    } catch (e) {
      message.error(t('orchestrator.run.assign.reassignError', { error: String(e) }));
    } finally {
      setSaving(false);
    }
  };

  // Inject a steering message into the running worker's live conversation. On
  // success we clear the draft; no refetch is needed (the transcript stream
  // surfaces the injected turn on its own).
  const sendSteer = async () => {
    if (!open || !canSteer) return;
    const text = steerText.trim();
    if (!text) return;
    setSteering(true);
    try {
      await ipcBridge.orchestrator.runs.steer.invoke({
        run_id: open.runId,
        task_id: open.task.id,
        updates: { text },
      });
      message.success(t('orchestrator.run.steer.sent'));
      setSteerText('');
    } catch (e) {
      message.error(t('orchestrator.run.steer.error', { error: String(e) }));
    } finally {
      setSteering(false);
    }
  };

  // Persist an edited 意图/prompt (spec) for a non-running node, then refetch so
  // the canvas + panel resync. The amended spec only takes effect on the next
  // rerun (the hint under the editor spells this out). Backend rejects a blank
  // spec or a running task — surfaced via message.error.
  const saveSpec = async () => {
    if (!open || isRunning) return;
    const next = specDraft.trim();
    if (!next || !specDirty) return;
    setSpecSaving(true);
    try {
      await ipcBridge.orchestrator.runs.updateTaskSpec.invoke({
        run_id: open.runId,
        task_id: open.task.id,
        updates: { spec: next },
      });
      message.success(t('orchestrator.run.spec.saveOk'));
      await open.refetch();
    } catch (e) {
      message.error(t('orchestrator.run.spec.saveError', { error: String(e) }));
    } finally {
      setSpecSaving(false);
    }
  };

  // Re-execute this node (and cascade-reset its settled downstream), re-activating
  // a terminal run so the engine re-drives it. Backend rejects a running task with
  // BadRequest — we also disable the trigger for `running` to mirror that. On
  // success we refetch so the canvas reflects the reset + re-drive immediately.
  const doRerun = async () => {
    if (!open || isRunning) return;
    setRerunning(true);
    try {
      await ipcBridge.orchestrator.runs.rerunTask.invoke({
        run_id: open.runId,
        task_id: open.task.id,
      });
      message.success(t('orchestrator.run.rerun.ok'));
      await open.refetch();
    } catch (e) {
      message.error(t('orchestrator.run.rerun.error', { error: String(e) }));
    } finally {
      setRerunning(false);
    }
  };

  const fleetMembers = open?.fleetMembers ?? [];
  const currentMember = assignment ? fleetMembers.find((m) => m.id === assignment.member_id) : undefined;

  // ── Config values (planner role wins over the member's static role_hint) ────
  const emptyDash = t('orchestrator.run.detail.config.empty');
  const roleText = task?.role?.trim()
    ? roleLabel(task.role)
    : currentMember?.role_hint
      ? roleLabel(currentMember.role_hint)
      : emptyDash;

  const modelName = currentMember?.model?.trim();
  const providerName = currentMember?.provider_id?.trim();

  const persona = currentMember?.system_prompt?.trim() ?? '';
  const personaTruncated = persona.length > PERSONA_PREVIEW_LEN;
  const personaPreview = personaTruncated ? `${persona.slice(0, PERSONA_PREVIEW_LEN)}…` : persona;

  const enabledSkills = currentMember?.enabled_skills ?? [];
  const disabledBuiltinCount = currentMember?.disabled_builtin_skills?.length ?? 0;

  const statusMeta = task ? taskStatusMeta(task.status) : null;
  const statusLabel = task
    ? t(`orchestrator.run.task.status.${task.status}` as 'orchestrator.run.task.status.pending', {
        defaultValue: t('orchestrator.run.status.unknown'),
      })
    : '';

  return (
    <Drawer
      width={560}
      visible={!!task}
      onCancel={onClose}
      footer={null}
      title={<span className='min-w-0 truncate pr-8px'>{task?.title}</span>}
    >
      {ctx}
      <div className='flex flex-col h-full overflow-hidden'>
        {/* ── Agent config (who / what / status) + management controls ───────── */}
        {task && (
          <div
            className='mb-12px shrink-0 rd-12px p-12px'
            style={{ background: 'var(--bg-2)', border: '1px solid var(--border-base)' }}
          >
            <div className='mb-8px flex items-center gap-8px'>
              <span className='text-11px font-600 uppercase tracking-wide text-t-tertiary'>
                {t('orchestrator.run.detail.config.title')}
              </span>
              {currentMember && (
                <span className='inline-flex min-w-0 items-center gap-5px'>
                  {memberLogo(currentMember) ? (
                    <img src={memberLogo(currentMember) ?? ''} alt='' className='size-13px shrink-0 object-contain' />
                  ) : null}
                  <span className='truncate text-11px text-t-secondary'>
                    {memberShortLabel(currentMember) ?? currentMember.id}
                  </span>
                </span>
              )}
            </div>

            <div className='flex flex-col gap-7px'>
              {/* 角色 */}
              <ConfigRow label={t('orchestrator.run.detail.config.role')}>
                <span className='font-500'>{roleText}</span>
              </ConfigRow>

              {/* 模型 */}
              <ConfigRow label={t('orchestrator.run.detail.config.model')}>
                {modelName || providerName ? (
                  <span className='inline-flex flex-wrap items-baseline gap-x-6px gap-y-2px'>
                    <span className='font-500'>{modelName ?? t('orchestrator.run.detail.config.modelNone')}</span>
                    {providerName && <span className='text-11px text-t-tertiary'>· {providerName}</span>}
                  </span>
                ) : (
                  <span className='text-t-tertiary'>{emptyDash}</span>
                )}
              </ConfigRow>

              {/* 人设 — preview + tooltip for the full text (only if present) */}
              {persona && (
                <ConfigRow label={t('orchestrator.run.detail.config.persona')}>
                  {personaTruncated ? (
                    <Tooltip
                      position='left'
                      content={
                        <span className='block max-h-280px max-w-360px overflow-auto whitespace-pre-wrap text-12px leading-18px'>
                          {persona}
                        </span>
                      }
                    >
                      <span className='cursor-help leading-18px text-t-secondary underline-offset-2 decoration-dotted hover:underline'>
                        {personaPreview}
                      </span>
                    </Tooltip>
                  ) : (
                    <span className='leading-18px text-t-secondary'>{personaPreview}</span>
                  )}
                </ConfigRow>
              )}

              {/* 技能 — small chips */}
              <ConfigRow label={t('orchestrator.run.detail.config.skills')}>
                {enabledSkills.length > 0 ? (
                  <div className='flex flex-col gap-4px'>
                    <div className='flex flex-wrap gap-4px'>
                      {enabledSkills.map((skill) => (
                        <span
                          key={skill}
                          className='inline-flex max-w-180px items-center rd-100px px-7px py-2px text-10px leading-none text-t-secondary'
                          style={{ background: 'var(--fill-0)', border: '1px solid var(--border-light)' }}
                          title={skill}
                        >
                          <span className='truncate'>{skill}</span>
                        </span>
                      ))}
                    </div>
                    {disabledBuiltinCount > 0 && (
                      <span className='text-10px leading-none text-t-tertiary'>
                        {t('orchestrator.run.detail.config.disabledBuiltin', { count: disabledBuiltinCount })}
                      </span>
                    )}
                  </div>
                ) : (
                  <span className='text-t-tertiary'>{t('orchestrator.run.detail.config.skillsNone')}</span>
                )}
              </ConfigRow>

              {/* 状态 — reuse the canvas status color mapping */}
              <ConfigRow label={t('orchestrator.run.detail.config.status')}>
                <span className='inline-flex items-center gap-5px'>
                  <span
                    className={`size-8px shrink-0 rd-full ${statusMeta?.pulse ? 'nomi-dag-pulse' : ''}`}
                    style={{ background: statusMeta?.color ?? 'var(--bg-6)' }}
                  />
                  <span className='font-500' style={{ color: statusMeta?.color ?? 'var(--text-2)' }}>
                    {statusLabel}
                  </span>
                </span>
              </ConfigRow>

              {/* 用时 — elapsed (done) / running-for, only for started tasks */}
              {task && taskHasTiming(task.status) && (
                <ConfigRow label={t('orchestrator.run.progress.elapsed')}>
                  <span className='tabular-nums text-t-secondary'>
                    {t(
                      task.status === 'running'
                        ? 'orchestrator.run.timing.running'
                        : 'orchestrator.run.timing.took',
                      { value: formatDuration(task.updated_at - task.created_at) }
                    )}
                  </span>
                </ConfigRow>
              )}

              {/* Tokens — per-task usage, only once the worker has produced turns */}
              {task && typeof task.tokens === 'number' && task.tokens > 0 && (
                <ConfigRow label={t('orchestrator.run.progress.tokens')}>
                  <span className='inline-flex items-center gap-4px tabular-nums text-t-secondary'>
                    <Lightning theme='outline' size='12' strokeWidth={3} className='shrink-0 text-t-tertiary' />
                    {task.tokens.toLocaleString()}
                  </span>
                </ConfigRow>
              )}
            </div>

            {/* Rationale + reassign + lock (management) — only with an assignment */}
            {assignment && (
              <div className='mt-12px border-t border-t-base pt-12px'>
                <div className='text-11px font-600 uppercase tracking-wide text-t-tertiary'>
                  {t('orchestrator.run.assign.rationaleTitle')}
                </div>
                <div className='mt-4px text-12px leading-18px text-t-secondary'>
                  {assignment.rationale?.trim() || t('orchestrator.run.assign.noRationale')}
                  {typeof assignment.score === 'number' && (
                    <span className='ml-6px text-t-tertiary'>
                      {t('orchestrator.run.assign.score', { score: assignment.score.toFixed(2) })}
                    </span>
                  )}
                </div>

                <div className='mt-10px flex flex-col gap-8px'>
                  <div>
                    <div className='mb-4px text-11px font-500 text-t-tertiary'>
                      {t('orchestrator.run.assign.reassign')}
                    </div>
                    <Select
                      className='w-full'
                      size='small'
                      disabled={saving}
                      value={assignment.member_id}
                      onChange={(memberId: string) => void applyReassign(memberId, assignment.locked)}
                      showSearch
                      filterOption={(input, option) => {
                        const id = (option as React.ReactElement<{ value?: string }>)?.props?.value;
                        const m = fleetMembers.find((fm) => fm.id === id);
                        const text = `${m?.agent_id ?? ''} ${m?.model ?? ''} ${m?.role_hint ?? ''}`.toLowerCase();
                        return text.includes(input.toLowerCase());
                      }}
                    >
                      {fleetMembers.map((m) => {
                        const opt = memberOption(m, roleLabel);
                        return (
                          <Select.Option key={m.id} value={m.id}>
                            <span className='flex items-center gap-8px'>
                              <span className='size-16px shrink-0 flex items-center justify-center'>
                                {opt.logo ? (
                                  <img src={opt.logo} alt='' className='size-16px object-contain' />
                                ) : (
                                  <span className='text-12px leading-none'>🤖</span>
                                )}
                              </span>
                              <span className='truncate'>{opt.label}</span>
                              {opt.role && <span className='shrink-0 text-t-tertiary text-11px'>· {opt.role}</span>}
                            </span>
                          </Select.Option>
                        );
                      })}
                    </Select>
                  </div>
                  <div className='flex items-center justify-between'>
                    <div className='flex flex-col'>
                      <span className='text-12px font-500 text-t-primary'>{t('orchestrator.run.assign.locked')}</span>
                      <span className='text-11px leading-15px text-t-tertiary'>
                        {t('orchestrator.run.assign.lockedHint')}
                      </span>
                    </div>
                    <Switch
                      size='small'
                      disabled={saving}
                      checked={assignment.locked}
                      onChange={(locked: boolean) => void applyReassign(assignment.member_id, locked)}
                    />
                  </div>
                </div>
              </div>
            )}

            {/* 意图/prompt 微调 + 重跑 (UC-2a-fe) — editable spec (read-only while
                running) and a confirm-guarded manual rerun. Always shown for a
                task regardless of assignment, so a failed/done/skipped/pending
                node can be fine-tuned then re-executed. */}
            <div className='mt-12px border-t border-t-base pt-12px'>
              <div className='mb-4px flex items-center justify-between gap-8px'>
                <span className='text-11px font-600 uppercase tracking-wide text-t-tertiary'>
                  {t('orchestrator.run.spec.title')}
                </span>
                {isRunning ? (
                  <span className='text-10px leading-none text-t-tertiary'>
                    {t('orchestrator.run.spec.runningHint')}
                  </span>
                ) : specDirty ? (
                  <span className='text-10px leading-none text-warning-6'>
                    {t('orchestrator.run.spec.dirtyHint')}
                  </span>
                ) : null}
              </div>
              <Input.TextArea
                value={specDraft}
                disabled={isRunning || specSaving}
                placeholder={t('orchestrator.run.spec.placeholder')}
                autoSize={{ minRows: 2, maxRows: 8 }}
                onChange={(v: string) => setSpecDraft(v)}
              />

              <div className='mt-8px flex items-center justify-between gap-8px'>
                {/* Rerun — confirm-guarded; disabled (with a tooltip) while running. */}
                <Tooltip
                  content={t('orchestrator.run.rerun.running')}
                  disabled={!isRunning}
                  position='top'
                >
                  {/* Popconfirm is suppressed while running (the trigger is disabled). */}
                  <Popconfirm
                    focusLock
                    disabled={isRunning || rerunning}
                    title={t('orchestrator.run.rerun.confirm')}
                    okText={t('orchestrator.run.rerun.confirmOk')}
                    cancelText={t('orchestrator.run.rerun.confirmCancel')}
                    onOk={() => void doRerun()}
                  >
                    <div
                      role='button'
                      tabIndex={isRunning ? -1 : 0}
                      aria-label={t('orchestrator.run.rerun.button')}
                      aria-disabled={isRunning || rerunning}
                      className='flex h-28px shrink-0 cursor-pointer items-center gap-5px rd-8px border border-b-base px-10px text-12px font-500 text-t-secondary transition-colors hover:border-primary hover:text-primary'
                      style={
                        isRunning || rerunning
                          ? { opacity: 0.5, cursor: 'not-allowed', pointerEvents: 'none' as const }
                          : undefined
                      }
                    >
                      <Redo theme='outline' size='13' strokeWidth={3} />
                      <span>{t('orchestrator.run.rerun.button')}</span>
                    </div>
                  </Popconfirm>
                </Tooltip>

                {/* Save spec — only enabled for a non-running node with a dirty,
                    non-blank draft. The new spec applies on the next rerun. */}
                <Button
                  size='small'
                  type='primary'
                  loading={specSaving}
                  disabled={isRunning || !specDirty || specDraft.trim().length === 0}
                  onClick={() => void saveSpec()}
                >
                  {specSaving ? t('orchestrator.run.spec.saving') : t('orchestrator.run.spec.save')}
                </Button>
              </div>
            </div>
          </div>
        )}

        {/* ── Worker transcript ─────────────────────────────────────────────── */}
        <div className='flex flex-1 min-h-0 flex-col overflow-hidden'>
          {conversationId === undefined ? (
            <div className='flex size-full flex-col items-center justify-center gap-12px px-24px text-center'>
              <span className='flex size-52px items-center justify-center rd-16px bg-fill-2 text-t-tertiary'>
                <Comment theme='outline' size='26' strokeWidth={3} />
              </span>
              <div className='text-15px font-600 text-t-primary'>{t('orchestrator.run.transcript.notStarted')}</div>
              <div className='max-w-320px text-12px leading-18px text-t-tertiary'>
                {t('orchestrator.run.transcript.noConversation')}
              </div>
            </div>
          ) : loading ? (
            <Spin loading className='flex flex-1 items-center justify-center' />
          ) : conversation ? (
            <ReadOnlyConversationView conversation={conversation} hideSendBox agent_name={task?.title} />
          ) : null}
        </div>

        {/* ── Steer (mid-turn inject), only for a running task with a live conv ── */}
        {canSteer && (
          <div
            className='mt-12px shrink-0 rd-12px p-12px'
            style={{ background: 'var(--bg-2)', border: '1px solid var(--border-base)' }}
          >
            <div className='text-11px font-600 uppercase tracking-wide text-t-tertiary'>
              {t('orchestrator.run.steer.title')}
            </div>
            <div className='mt-3px text-11px leading-15px text-t-tertiary'>{t('orchestrator.run.steer.hint')}</div>
            <div className='mt-8px flex items-center gap-8px'>
              <Input
                className='flex-1'
                size='small'
                allowClear
                disabled={steering}
                value={steerText}
                placeholder={t('orchestrator.run.steer.placeholder')}
                onChange={(v: string) => setSteerText(v)}
                onPressEnter={() => void sendSteer()}
              />
              <div
                role='button'
                tabIndex={0}
                aria-label={t('orchestrator.run.steer.send')}
                aria-disabled={steering || steerText.trim().length === 0}
                onClick={steering || steerText.trim().length === 0 ? undefined : () => void sendSteer()}
                onKeyDown={(e) => {
                  if ((e.key === 'Enter' || e.key === ' ') && !steering && steerText.trim().length > 0) {
                    e.preventDefault();
                    void sendSteer();
                  }
                }}
                className='flex h-28px shrink-0 cursor-pointer items-center gap-4px rd-8px px-10px text-12px font-500 text-white transition-opacity hover:opacity-90'
                style={{
                  background: 'rgb(var(--primary-6))',
                  ...(steering || steerText.trim().length === 0
                    ? { opacity: 0.5, pointerEvents: 'none' as const }
                    : {}),
                }}
              >
                <Send theme='outline' size='13' strokeWidth={3} />
                <span>{t('orchestrator.run.steer.send')}</span>
              </div>
            </div>
          </div>
        )}
      </div>
    </Drawer>
  );
};

export default WorkerTranscriptPanel;
