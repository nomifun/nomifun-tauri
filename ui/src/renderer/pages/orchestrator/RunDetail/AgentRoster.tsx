/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import React, { useCallback, useMemo } from 'react';
import { useTranslation } from 'react-i18next';
import './dag-canvas.css';
import type {
  TAssignment,
  TFleetMember,
  TRunDetail,
  TRunTask,
} from '@/common/types/orchestrator/orchestratorTypes';
import type { OpenTaskPayload } from './DagCanvas';
import { memberLogo } from './memberLabel';
import { taskStatusMeta } from './nodes/TaskNode';

export interface AgentRosterProps {
  /** Live run detail (run / tasks / deps / assignments / fleet_members). */
  detail: TRunDetail;
  /** Currently-inspected task id (the open inspector), for the active highlight. */
  selectedTaskId: string | null;
  /** Open the task inspector for the clicked card — mirrors a DAG node click. */
  onSelectTask: (payload: OpenTaskPayload) => void;
  /** Re-pulls run detail so the canvas + inspector resync after a reassignment. */
  refetch: () => Promise<void>;
}

/**
 * AgentRoster — a compact horizontal strip summarizing every task in a run as a
 * small clickable card: **role** (task-named, falling back to the assigned
 * member's `role_hint`) · **model** (the fleet member's model) · a status dot
 * colored by {@link taskStatusMeta}. It's the at-a-glance "who's on the team"
 * companion to the DAG canvas — clicking a card opens the same task inspector
 * the DAG nodes do, by building the identical {@link OpenTaskPayload}.
 *
 * Members are resolved null-safely through `assignment.member_id → fleet_members`;
 * an unassigned / unresolvable task still renders (role/model gracefully blank).
 * Renders nothing while the run has no tasks (the canvas shows its planning hint).
 */
const AgentRoster: React.FC<AgentRosterProps> = ({ detail, selectedTaskId, onSelectTask, refetch }) => {
  const { t } = useTranslation();

  // task_id → assignment, and member_id → fleet member (the run's fleet snapshot).
  const assignmentByTask = useMemo(() => {
    const map = new Map<string, TAssignment>();
    for (const a of detail.assignments) map.set(a.task_id, a);
    return map;
  }, [detail.assignments]);

  const memberById = useMemo(() => {
    const map = new Map<string, TFleetMember>();
    for (const m of detail.fleet_members) map.set(m.id, m);
    return map;
  }, [detail.fleet_members]);

  const roleLabel = useCallback(
    (role: string) =>
      t(`orchestrator.run.role.${role}` as 'orchestrator.run.role.planner', { defaultValue: role }),
    [t]
  );

  const handleSelect = useCallback(
    (task: TRunTask) => {
      onSelectTask({
        task,
        assignment: assignmentByTask.get(task.id) ?? null,
        fleetMembers: detail.fleet_members,
        runId: detail.run.id,
        refetch,
      });
    },
    [onSelectTask, assignmentByTask, detail.fleet_members, detail.run.id, refetch]
  );

  if (detail.tasks.length === 0) return null;

  return (
    <div className='shrink-0 border-b border-b-base bg-1 px-16px py-10px'>
      <div className='mb-8px flex items-center gap-8px'>
        <span className='text-12px font-600 leading-none text-t-secondary'>
          {t('orchestrator.roster.title')}
        </span>
        <span className='text-11px leading-none text-t-tertiary'>
          {t('orchestrator.roster.count', { count: detail.tasks.length })}
        </span>
      </div>

      {/* Horizontal scroll strip — one compact card per task. */}
      <div className='flex gap-8px overflow-x-auto nomi-roster-scroll pb-2px'>
        {detail.tasks.map((task) => {
          const assignment = assignmentByTask.get(task.id);
          const member = assignment ? memberById.get(assignment.member_id) : undefined;
          const meta = taskStatusMeta(task.status);
          const role = task.role ?? member?.role_hint;
          const roleText = role ? roleLabel(role) : t('orchestrator.roster.roleUnknown');
          const model = member?.model?.trim();
          const logo = memberLogo(member);
          const selected = selectedTaskId === task.id;
          const statusText = t(`orchestrator.run.task.status.${task.status}`, {
            defaultValue: t('orchestrator.run.status.unknown'),
          });

          return (
            <div
              key={task.id}
              role='button'
              tabIndex={0}
              aria-label={`${task.title || roleText} · ${statusText}`}
              aria-pressed={selected}
              onClick={() => handleSelect(task)}
              onKeyDown={(e) => {
                if (e.key === 'Enter' || e.key === ' ') {
                  e.preventDefault();
                  handleSelect(task);
                }
              }}
              className='group flex w-160px shrink-0 cursor-pointer select-none flex-col gap-6px rd-10px px-10px py-8px outline-none transition-all duration-150'
              style={{
                background: selected ? 'color-mix(in srgb, rgb(var(--primary-6)) 8%, var(--bg-2))' : 'var(--bg-2)',
                border: `1px solid ${selected ? 'rgb(var(--primary-6))' : 'var(--border-base)'}`,
                boxShadow: selected
                  ? '0 0 0 3px color-mix(in srgb, rgb(var(--primary-6)) 18%, transparent)'
                  : '0 1px 4px rgba(0,0,0,0.06)',
              }}
            >
              {/* Role + status dot */}
              <div className='flex items-center gap-6px'>
                <span
                  className={`size-7px shrink-0 rd-full ${meta.pulse ? 'nomi-dag-pulse' : ''}`}
                  style={{ background: meta.color, boxShadow: `0 0 0 2px color-mix(in srgb, ${meta.color} 20%, transparent)` }}
                />
                <span className='min-w-0 flex-1 truncate text-12px font-600 leading-none text-t-primary'>
                  {roleText}
                </span>
              </div>

              {/* Model chip — logo + model name (or a muted dash when unresolved) */}
              <div className='flex items-center gap-5px'>
                {logo ? (
                  <img src={logo} alt='' className='size-12px shrink-0 object-contain' />
                ) : (
                  <span className='size-5px shrink-0 rd-full' style={{ background: 'var(--bg-5)' }} />
                )}
                <span className='min-w-0 flex-1 truncate text-11px leading-none text-t-tertiary' title={model}>
                  {model ?? t('orchestrator.roster.modelUnknown')}
                </span>
              </div>
            </div>
          );
        })}
      </div>
    </div>
  );
};

export default AgentRoster;
