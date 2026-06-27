/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import React, { useEffect, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Drawer, Input, Select, Spin, Switch } from '@arco-design/web-react';
import { Comment, Send } from '@icon-park/react';
import { ipcBridge } from '@/common';
import type { TChatConversation } from '@/common/config/storage';
import type { TAssignment, TFleetMember } from '@/common/types/orchestrator/orchestratorTypes';
import { useArcoMessage } from '@/renderer/utils/ui/useArcoMessage';
import ReadOnlyConversationView from './ReadOnlyConversationView';
import type { OpenTaskPayload } from './DagCanvas';
import { memberLogo, memberShortLabel } from './memberLabel';

type WorkerTranscriptPanelProps = {
  /** The clicked DAG node's payload (task + assignment + fleet snapshot + refetch).
   * Null = nothing to show (drawer closed). */
  open: OpenTaskPayload | null;
  onClose: () => void;
};

/** One reassign Select option: friendly agent/model label + role hint, plus a logo. */
const memberOption = (m: TFleetMember, roleLabel: (role: string) => string) => {
  const label = memberShortLabel(m) ?? m.id;
  const role = m.role_hint ? roleLabel(m.role_hint) : null;
  return { member: m, label, role, logo: memberLogo(m) };
};

/**
 * Side drawer for one orchestration task. Two stacked sections:
 *
 *  1. Assignment inspector — WHY the task was routed to its member (the
 *     orchestrator's `rationale`), plus controls to **reassign** it to another
 *     fleet member and **lock** the assignment so the auto-router won't override
 *     it. Changes call `PUT …/assignment` and then refetch the run so the canvas
 *     and this panel reflect the new state.
 *  2. Worker transcript — the live, read-only conversation record (mirrors
 *     SubagentDrawer: ReadOnlyConversationView with the send box hidden). Shown
 *     only once a worker has picked up the task and a conversation exists.
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

  const task = open?.task ?? null;
  const assignment = open?.assignment ?? null;
  const conversationId = task?.conversation_id;
  // Steer is only meaningful for a task actively running on a worker with a live
  // conversation to inject into.
  const canSteer = task?.status === 'running' && conversationId !== undefined;

  // Reset the steer draft whenever the inspected task changes.
  useEffect(() => {
    setSteerText('');
  }, [task?.id]);

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

  const fleetMembers = open?.fleetMembers ?? [];
  const currentMember = assignment ? fleetMembers.find((m) => m.id === assignment.member_id) : undefined;

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
        {/* ── Assignment inspector ─────────────────────────────────────────── */}
        {assignment && (
          <div
            className='mb-12px shrink-0 rd-12px p-12px'
            style={{ background: 'var(--bg-2)', border: '1px solid var(--border-base)' }}
          >
            {/* Rationale: "why this member" */}
            <div className='text-11px font-600 uppercase tracking-wide text-t-tertiary'>
              {t('orchestrator.run.assign.rationaleTitle')}
            </div>
            <div className='mt-4px text-13px leading-19px text-t-primary'>
              {assignment.rationale?.trim() || t('orchestrator.run.assign.noRationale')}
            </div>
            {currentMember && (
              <div className='mt-6px flex items-center gap-6px text-12px text-t-secondary'>
                {memberLogo(currentMember) ? (
                  <img src={memberLogo(currentMember) ?? ''} alt='' className='size-14px shrink-0 object-contain' />
                ) : null}
                <span className='truncate'>{memberShortLabel(currentMember) ?? currentMember.id}</span>
                {typeof assignment.score === 'number' && (
                  <span className='shrink-0 text-t-tertiary'>
                    {t('orchestrator.run.assign.score', { score: assignment.score.toFixed(2) })}
                  </span>
                )}
              </div>
            )}

            {/* Reassign + lock controls */}
            <div className='mt-12px flex flex-col gap-8px'>
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
                  <span className='text-11px leading-15px text-t-tertiary'>{t('orchestrator.run.assign.lockedHint')}</span>
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
