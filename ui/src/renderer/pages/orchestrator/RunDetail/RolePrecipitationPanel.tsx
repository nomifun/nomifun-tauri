/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import React, { useCallback, useEffect, useMemo, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { AddUser, Check, Down, Experiment, Up } from '@icon-park/react';
import { ipcBridge } from '@/common';
import type { CreateAssistantRequest } from '@/common/types/agent/assistantTypes';
import type { TRunDetail } from '@/common/types/orchestrator/orchestratorTypes';
import { useArcoMessage } from '@/renderer/utils/ui/useArcoMessage';

/**
 * A role candidate distilled from a completed run's tasks: a distinct
 * `task.role` plus the models that actually worked it and the union of those
 * workers' skill sets. One candidate → one suggested 助手 (assistant).
 */
interface RoleCandidate {
  /** The role name; becomes the assistant's name. */
  name: string;
  /** Short synthesized one-liner shown on the card + saved as the description. */
  description: string;
  /** Distinct models that worked this role (resolved via assignments). */
  models: string[];
  /** Union of `enabled_skills` over the role's workers. */
  enabledSkills: string[];
  /** Union of `disabled_builtin_skills` over the role's workers. */
  disabledBuiltinSkills: string[];
  /** True when an 助手 with this name already exists (case-insensitive). */
  exists: boolean;
}

/** Push every non-empty, de-duplicated value of `items` into `set`. */
function collect(set: Set<string>, items: readonly string[] | undefined): void {
  if (!items) return;
  for (const raw of items) {
    const v = raw?.trim();
    if (v) set.add(v);
  }
}

/**
 * RolePrecipitationPanel — when a Run finishes, its planner-named roles are the
 * organisation's accidental org-chart: roles that actually got work done, paired
 * with the models that did it. This panel surfaces each distinct role as a
 * one-click 「保存为助手」 suggestion so the user can precipitate (沉淀) a
 * proven role into a reusable assistant without re-typing models/skills.
 *
 * Candidate derivation (per distinct, non-empty `task.role`):
 *  - `models` — distinct `member.model` resolved through
 *    `assignment.task_id → member_id → fleet_members`.
 *  - `enabled_skills` / `disabled_builtin_skills` — union over those members.
 *  - `description` — a short line (role + brief from task titles, or a worker's
 *    own `description` when present).
 *
 * Existing assistants are loaded once and matched by name (case-insensitive,
 * trimmed) so already-precipitated roles are shown disabled rather than offered
 * again. If there are no roles (an older run) or every role already exists, the
 * panel renders nothing — it never intrudes on the canvas.
 *
 * The persona rule text is intentionally NOT written back here; the user edits
 * it later on the assistant page (carry-forward, see Task 3 brief).
 */
const RolePrecipitationPanel: React.FC<{ detail: TRunDetail }> = ({ detail }) => {
  const { t } = useTranslation();
  const [message, ctx] = useArcoMessage();
  const [collapsed, setCollapsed] = useState(false);
  /** Names of assistants that already exist, lower-cased + trimmed. */
  const [existingNames, setExistingNames] = useState<Set<string> | null>(null);
  /** Role names the user has just saved this session (for the ✓ saved state). */
  const [savedNames, setSavedNames] = useState<Set<string>>(() => new Set());
  /** Role names whose save call is currently in flight. */
  const [savingNames, setSavingNames] = useState<Set<string>>(() => new Set());

  // member_id → fleet member (the run's fleet snapshot).
  const memberById = useMemo(() => {
    const map = new Map<string, (typeof detail.fleet_members)[number]>();
    for (const m of detail.fleet_members) map.set(m.id, m);
    return map;
  }, [detail.fleet_members]);

  // task_id → member (the worker that was assigned the task).
  const memberByTask = useMemo(() => {
    const map = new Map<string, (typeof detail.fleet_members)[number]>();
    for (const a of detail.assignments) {
      const m = memberById.get(a.member_id);
      if (m) map.set(a.task_id, m);
    }
    return map;
  }, [detail.assignments, memberById]);

  // Group the run's tasks by their planner-named role into ranked candidates.
  const candidates = useMemo<RoleCandidate[]>(() => {
    interface Acc {
      name: string;
      titles: string[];
      memberDescription?: string;
      models: Set<string>;
      enabledSkills: Set<string>;
      disabledBuiltinSkills: Set<string>;
    }
    const byRole = new Map<string, Acc>();
    for (const task of detail.tasks) {
      const role = task.role?.trim();
      if (!role) continue;
      const key = role.toLowerCase();
      let acc = byRole.get(key);
      if (!acc) {
        acc = {
          name: role,
          titles: [],
          models: new Set<string>(),
          enabledSkills: new Set<string>(),
          disabledBuiltinSkills: new Set<string>(),
        };
        byRole.set(key, acc);
      }
      const title = task.title?.trim();
      if (title && acc.titles.length < 3 && !acc.titles.includes(title)) acc.titles.push(title);
      const member = memberByTask.get(task.id);
      if (member) {
        const model = member.model?.trim();
        if (model) acc.models.add(model);
        collect(acc.enabledSkills, member.enabled_skills);
        collect(acc.disabledBuiltinSkills, member.disabled_builtin_skills);
        const desc = member.description?.trim();
        if (desc && !acc.memberDescription) acc.memberDescription = desc;
      }
    }

    return Array.from(byRole.values())
      .map((acc) => {
        // Prefer a worker's own description; otherwise synthesize a short line
        // from the role + a couple of the task titles it covered.
        const description = acc.memberDescription
          ? acc.memberDescription
          : acc.titles.length > 0
            ? t('orchestrator.run.precipitate.synthDesc', {
                role: acc.name,
                tasks: acc.titles.join(t('orchestrator.run.precipitate.taskSep')),
              })
            : t('orchestrator.run.precipitate.synthDescBare', { role: acc.name });
        const lowerName = acc.name.toLowerCase();
        return {
          name: acc.name,
          description,
          models: Array.from(acc.models),
          enabledSkills: Array.from(acc.enabledSkills),
          disabledBuiltinSkills: Array.from(acc.disabledBuiltinSkills),
          exists: existingNames?.has(lowerName) ?? false,
        } satisfies RoleCandidate;
      })
      .sort((a, b) => a.name.localeCompare(b.name));
  }, [detail.tasks, memberByTask, existingNames, t]);

  // There are roles to precipitate at all — gate the one-time assistant fetch on
  // this so older (role-less) runs never make the request.
  const hasRoles = candidates.length > 0;

  // Load the existing assistants once (when there is at least one role) so we
  // can mark already-precipitated roles instead of offering a duplicate.
  useEffect(() => {
    if (!hasRoles || existingNames !== null) return;
    let cancelled = false;
    void (async () => {
      try {
        const list = await ipcBridge.assistants.list.invoke();
        if (cancelled) return;
        const names = new Set<string>();
        for (const a of list ?? []) collect(names, [a.name.toLowerCase()]);
        setExistingNames(names);
      } catch {
        // Non-fatal: if the list can't load we just show every role as new.
        if (!cancelled) setExistingNames(new Set<string>());
      }
    })();
    return () => {
      cancelled = true;
    };
  }, [hasRoles, existingNames]);

  const handleSave = useCallback(
    async (candidate: RoleCandidate) => {
      setSavingNames((prev) => {
        const next = new Set(prev);
        next.add(candidate.name);
        return next;
      });
      try {
        const payload: CreateAssistantRequest = {
          name: candidate.name,
          description: candidate.description,
          models: candidate.models,
          enabled_skills: candidate.enabledSkills,
          disabled_builtin_skills: candidate.disabledBuiltinSkills,
          preset_agent_type: 'nomi',
        };
        await ipcBridge.assistants.create.invoke(payload);
        setSavedNames((prev) => {
          const next = new Set(prev);
          next.add(candidate.name);
          return next;
        });
        message.success(t('orchestrator.run.precipitate.saveOk', { name: candidate.name }));
      } catch (e) {
        message.error(t('orchestrator.run.precipitate.saveError', { error: String(e) }));
      } finally {
        setSavingNames((prev) => {
          const next = new Set(prev);
          next.delete(candidate.name);
          return next;
        });
      }
    },
    [message, t]
  );

  // Render nothing for role-less runs or once every role is already an assistant.
  // (Locally-saved roles still render — disabled with a ✓ — so the user gets the
  // satisfying confirmation; only roles that pre-existed before this panel
  // opened are hidden entirely.)
  if (!hasRoles) return null;
  const visible = candidates.filter((c) => !c.exists);
  if (visible.length === 0) return null;

  return (
    <div className='shrink-0 border-b border-b-base bg-1'>
      {ctx}
      {/* Banner header — click to collapse/expand without ever covering the canvas. */}
      <div
        role='button'
        tabIndex={0}
        aria-expanded={!collapsed}
        onClick={() => setCollapsed((v) => !v)}
        onKeyDown={(e) => {
          if (e.key === 'Enter' || e.key === ' ') {
            e.preventDefault();
            setCollapsed((v) => !v);
          }
        }}
        className='flex cursor-pointer select-none items-center gap-8px px-16px py-10px transition-colors hover:bg-fill-1'
      >
        <span className='flex size-22px shrink-0 items-center justify-center rd-6px bg-fill-2 text-primary-6'>
          <Experiment theme='outline' size='14' strokeWidth={3} />
        </span>
        <div className='min-w-0 flex-1'>
          <div className='truncate text-13px font-600 text-t-primary'>
            {t('orchestrator.run.precipitate.title')}
          </div>
          <div className='truncate text-11px text-t-tertiary'>
            {t('orchestrator.run.precipitate.subtitle', { count: visible.length })}
          </div>
        </div>
        <span className='flex size-22px shrink-0 items-center justify-center text-t-tertiary'>
          {collapsed ? (
            <Down theme='outline' size='16' strokeWidth={3} />
          ) : (
            <Up theme='outline' size='16' strokeWidth={3} />
          )}
        </span>
      </div>

      {!collapsed && (
        <div className='flex flex-wrap gap-10px px-16px pb-12px'>
          {visible.map((candidate) => {
            const saved = savedNames.has(candidate.name);
            const saving = savingNames.has(candidate.name);
            return (
              <div
                key={candidate.name}
                className='flex min-w-220px flex-1 flex-col gap-8px rd-10px border border-b-base bg-2 p-12px'
                style={{ maxWidth: '320px' }}
              >
                <div className='flex items-start justify-between gap-8px'>
                  <div className='min-w-0'>
                    <div className='truncate text-13px font-600 text-t-primary'>{candidate.name}</div>
                    <div className='mt-2px line-clamp-2 text-11px leading-16px text-t-tertiary'>
                      {candidate.description}
                    </div>
                  </div>
                </div>

                {/* Models + skill count meta */}
                <div className='flex flex-col gap-5px'>
                  {candidate.models.length > 0 && (
                    <div className='flex flex-wrap items-center gap-4px'>
                      <span className='shrink-0 text-10px text-t-tertiary'>
                        {t('orchestrator.run.precipitate.modelsLabel')}
                      </span>
                      {candidate.models.map((m) => (
                        <span
                          key={m}
                          className='rd-full bg-fill-2 px-7px py-1px text-10px font-500 text-t-secondary'
                        >
                          {m}
                        </span>
                      ))}
                    </div>
                  )}
                  {candidate.enabledSkills.length > 0 && (
                    <div className='text-10px text-t-tertiary'>
                      {t('orchestrator.run.precipitate.skillsLabel', {
                        count: candidate.enabledSkills.length,
                      })}
                    </div>
                  )}
                </div>

                {/* Save-as-assistant control */}
                <div
                  role='button'
                  tabIndex={saved || saving ? -1 : 0}
                  aria-disabled={saved || saving}
                  aria-label={t('orchestrator.run.precipitate.saveAsAssistant')}
                  onClick={saved || saving ? undefined : () => void handleSave(candidate)}
                  onKeyDown={(e) => {
                    if ((e.key === 'Enter' || e.key === ' ') && !saved && !saving) {
                      e.preventDefault();
                      void handleSave(candidate);
                    }
                  }}
                  className='flex h-28px items-center justify-center gap-5px rd-8px text-12px font-500 transition-all'
                  style={
                    saved
                      ? {
                          background: 'var(--bg-3)',
                          color: 'var(--success)',
                          cursor: 'default',
                        }
                      : {
                          background: 'rgb(var(--primary-6))',
                          color: '#fff',
                          cursor: saving ? 'default' : 'pointer',
                          opacity: saving ? 0.6 : 1,
                        }
                  }
                >
                  {saved ? (
                    <>
                      <Check theme='outline' size='13' strokeWidth={4} />
                      <span>{t('orchestrator.run.precipitate.saved')}</span>
                    </>
                  ) : (
                    <>
                      <AddUser theme='outline' size='13' strokeWidth={3} />
                      <span>{t('orchestrator.run.precipitate.saveAsAssistant')}</span>
                    </>
                  )}
                </div>
              </div>
            );
          })}
        </div>
      )}
    </div>
  );
};

export default RolePrecipitationPanel;
