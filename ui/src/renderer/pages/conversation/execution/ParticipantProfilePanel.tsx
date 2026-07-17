/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import { Button, Drawer } from '@arco-design/web-react';
import React, { useCallback, useEffect, useMemo, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { AddUser, Check, Down, Experiment, Right } from '@icon-park/react';
import { ipcBridge } from '@/common';
import type { CreatePresetRequest } from '@/common/types/agent/presetTypes';
import type { TAgentExecutionDetail } from '@/common/types/agentExecution/agentExecutionTypes';
import { latestAttemptForStep } from '@/common/types/agentExecution/agentExecutionTypes';
import { useArcoMessage } from '@/renderer/utils/ui/useArcoMessage';
import type { ProviderId } from '@/common/types/ids';
import styles from './participantProfilePanel.module.css';

/** A reusable role candidate distilled from one completed execution. */
interface RoleCandidate {
  /** The role name; becomes the preset's name. */
  name: string;
  /** Short synthesized one-liner shown on the card + saved as the description. */
  description: string;
  taskCount: number;
  /** Distinct models used by participants in this role. */
  models: string[];
  modelPreferences: Array<{
    provider_id?: ProviderId;
    model: string;
    required: false;
  }>;
  agentIds: string[];
  instructions: string;
  /** Union of `enabled_skills` over the role's participants. */
  enabledSkills: string[];
  /** Union of `disabled_builtin_skills` over the role's participants. */
  disabledBuiltinSkills: string[];
  /** True when a preset with this name already exists (case-insensitive). */
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
 * Offers reusable presets for roles observed in a completed execution. Each
 * candidate retains the models, skills, and task descriptions that produced it.
 */
const ParticipantProfilePanel: React.FC<{ detail: TAgentExecutionDetail }> = ({ detail }) => {
  const { t } = useTranslation();
  const [message, ctx] = useArcoMessage();
  const [open, setOpen] = useState(false);
  const [expandedNames, setExpandedNames] = useState<Set<string>>(() => new Set());
  /** Names of presets that already exist, lower-cased + trimmed. */
  const [existingNames, setExistingNames] = useState<Set<string> | null>(null);
  /** Role names the user has just saved this session (for the ✓ saved state). */
  const [savedNames, setSavedNames] = useState<Set<string>>(() => new Set());
  /** Role names whose save call is currently in flight. */
  const [savingNames, setSavingNames] = useState<Set<string>>(() => new Set());

  const participantById = useMemo(() => {
    const map = new Map<string, (typeof detail.participants)[number]>();
    for (const participant of detail.participants) map.set(participant.id, participant);
    return map;
  }, [detail.participants]);

  const participantByStep = useMemo(() => {
    const map = new Map<string, (typeof detail.participants)[number]>();
    for (const step of detail.steps.filter((item) => item.superseded_in_revision == null)) {
      const attempt = latestAttemptForStep(detail.attempts, step.id);
      const participantId = attempt?.participant_id ?? step.assigned_participant_id;
      const participant = participantId ? participantById.get(participantId) : undefined;
      if (participant) map.set(step.id, participant);
    }
    return map;
  }, [detail.attempts, detail.steps, participantById]);

  // Group current tasks by their planner-named role into ranked candidates.
  const candidates = useMemo<RoleCandidate[]>(() => {
    interface Acc {
      name: string;
      titles: string[];
      taskCount: number;
      memberDescription?: string;
      models: Set<string>;
      modelPreferences: Map<string, { provider_id?: ProviderId; model: string; required: false }>;
      agentIds: Set<string>;
      enabledSkills: Set<string>;
      disabledBuiltinSkills: Set<string>;
    }
    const byRole = new Map<string, Acc>();
    for (const step of detail.steps.filter((item) => item.superseded_in_revision == null)) {
      const role = step.role?.trim();
      if (!role) continue;
      const key = role.toLowerCase();
      let acc = byRole.get(key);
      if (!acc) {
        acc = {
          name: role,
          titles: [],
          taskCount: 0,
          models: new Set<string>(),
          modelPreferences: new Map(),
          agentIds: new Set<string>(),
          enabledSkills: new Set<string>(),
          disabledBuiltinSkills: new Set<string>(),
        };
        byRole.set(key, acc);
      }
      acc.taskCount += 1;
      const title = step.title?.trim();
      if (title && acc.titles.length < 3 && !acc.titles.includes(title)) acc.titles.push(title);
      const participant = participantByStep.get(step.id);
      if (participant) {
        const model = participant.model?.trim();
        if (model) {
          acc.models.add(model);
          const providerId = participant.provider_id ?? undefined;
          acc.modelPreferences.set(`${providerId ?? ''}::${model}`, {
            provider_id: providerId,
            model,
            required: false,
          });
        }
        if (participant.source_agent_id?.trim()) acc.agentIds.add(participant.source_agent_id.trim());
        collect(acc.enabledSkills, participant.enabled_skills);
        collect(acc.disabledBuiltinSkills, participant.disabled_builtin_skills);
        const desc = participant.description?.trim();
        if (desc && !acc.memberDescription) acc.memberDescription = desc;
      }
    }

    return Array.from(byRole.values())
      .map((acc) => {
        // Prefer a participant's own description; otherwise synthesize a short line
        // from the role + a couple of the task titles it covered.
        const description = acc.memberDescription
          ? acc.memberDescription
          : acc.titles.length > 0
            ? t('agentExecution.profile.synthDesc', {
                role: acc.name,
                tasks: acc.titles.join(t('agentExecution.profile.taskSep')),
              })
            : t('agentExecution.profile.synthDescBare', { role: acc.name });
        const lowerName = acc.name.toLowerCase();
        return {
          name: acc.name,
          description,
          taskCount: acc.taskCount,
          models: Array.from(acc.models),
          modelPreferences: Array.from(acc.modelPreferences.values()),
          agentIds: Array.from(acc.agentIds),
          instructions: acc.memberDescription || description,
          enabledSkills: Array.from(acc.enabledSkills),
          disabledBuiltinSkills: Array.from(acc.disabledBuiltinSkills),
          exists: existingNames?.has(lowerName) ?? false,
        } satisfies RoleCandidate;
      })
      .sort((a, b) => a.name.localeCompare(b.name));
  }, [detail.steps, participantByStep, existingNames, t]);

  // There are roles to precipitate at all — gate the one-time preset fetch on
  // this so role-less executions never make the request.
  const hasRoles = candidates.length > 0;

  // Load the existing presets once (when there is at least one role) so we
  // can mark already-precipitated roles instead of offering a duplicate.
  useEffect(() => {
    if (!hasRoles || existingNames !== null) return;
    let cancelled = false;
    void (async () => {
      try {
        const list = await ipcBridge.presets.list.invoke();
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
        const payload: CreatePresetRequest = {
          name: candidate.name,
          description: candidate.description,
          routing_description: candidate.description,
          instructions: candidate.instructions,
          targets: ['conversation', 'execution_step'],
          agent_preferences: candidate.agentIds.map((agent_id) => ({
            agent_id,
            required: false,
          })),
          model_preferences: candidate.modelPreferences,
          included_skills: candidate.enabledSkills.map((skill_name) => ({
            skill_name,
            required: false,
          })),
          excluded_auto_skills: candidate.disabledBuiltinSkills,
          fallback_allowed: true,
          knowledge_policy: {
            enabled: false,
            mode: 'inherit',
            writeback: false,
            grounded: false,
          },
        };
        await ipcBridge.presets.create.invoke(payload);
        setSavedNames((prev) => {
          const next = new Set(prev);
          next.add(candidate.name);
          return next;
        });
        message.success(t('agentExecution.profile.saveOk', { name: candidate.name }));
      } catch (e) {
        message.error(t('agentExecution.profile.saveError', { error: String(e) }));
      } finally {
        setSavingNames((prev) => {
          const next = new Set(prev);
          next.delete(candidate.name);
          return next;
        });
      }
    },
    [message, t],
  );

  // Wait for duplicate detection before exposing the entry so completed
  // executions never flash a large, inaccurate candidate count.
  if (!hasRoles || existingNames === null) return null;
  const visible = candidates.filter((c) => !c.exists);
  if (visible.length === 0) return null;

  const toggleExpanded = (name: string) => {
    setExpandedNames((previous) => {
      const next = new Set(previous);
      if (next.has(name)) next.delete(name);
      else next.add(name);
      return next;
    });
  };

  return (
    <>
      {ctx}
      <button
        type='button'
        className={styles.entry}
        aria-label={t('agentExecution.profile.open', { count: visible.length })}
        onClick={() => setOpen(true)}
      >
        <span className={styles.entryIcon}>
          <Experiment theme='outline' size='14' strokeWidth={3} />
        </span>
        <span className={styles.entryLabel}>{t('agentExecution.profile.entry')}</span>
        <b className={styles.entryCount}>{visible.length}</b>
      </button>

      <Drawer
        visible={open}
        width='min(520px, calc(100vw - 12px))'
        placement='right'
        title={t('agentExecution.profile.title')}
        footer={null}
        focusLock
        autoFocus
        getPopupContainer={() => document.body}
        onCancel={() => setOpen(false)}
        bodyStyle={{ padding: 0 }}
      >
        <div className={styles.drawerIntro}>{t('agentExecution.profile.drawerHint', { count: visible.length })}</div>
        <div className={styles.list}>
          {visible.map((candidate) => {
            const saved = savedNames.has(candidate.name);
            const saving = savingNames.has(candidate.name);
            const expanded = expandedNames.has(candidate.name);
            return (
              <div key={candidate.name} className={styles.candidate} data-expanded={expanded ? 'true' : undefined}>
                <div className={styles.candidateRow}>
                  <button
                    type='button'
                    className={styles.candidateToggle}
                    aria-expanded={expanded}
                    onClick={() => toggleExpanded(candidate.name)}
                  >
                    <span className={styles.chevron}>
                      {expanded ? <Down theme='outline' size='13' strokeWidth={3} /> : <Right theme='outline' size='13' strokeWidth={3} />}
                    </span>
                    <span className={styles.candidateCopy}>
                      <strong>{candidate.name}</strong>
                      <span>{candidate.description}</span>
                      <small>
                        {t('agentExecution.profile.candidateSummary', {
                          tasks: candidate.taskCount,
                          models: candidate.models.length,
                          skills: candidate.enabledSkills.length,
                        })}
                      </small>
                    </span>
                  </button>
                  <Button
                    size='small'
                    type={saved ? 'secondary' : 'primary'}
                    status={saved ? 'success' : undefined}
                    loading={saving}
                    disabled={saved || saving}
                    icon={saved ? <Check theme='outline' size='13' strokeWidth={4} /> : <AddUser theme='outline' size='13' strokeWidth={3} />}
                    onClick={() => void handleSave(candidate)}
                  >
                    {saved ? t('agentExecution.profile.saved') : t('agentExecution.profile.saveAsPreset')}
                  </Button>
                </div>

                {expanded && (
                  <div className={styles.candidateDetails}>
                    <p>{candidate.description}</p>
                    {candidate.models.length > 0 && (
                      <div className={styles.detailGroup}>
                        <b>{t('agentExecution.profile.modelsLabel')}</b>
                        <span className={styles.tags}>
                          {candidate.models.map((model) => (
                            <span key={model}>{model}</span>
                          ))}
                        </span>
                      </div>
                    )}
                    {candidate.enabledSkills.length > 0 && (
                      <div className={styles.detailGroup}>
                        <b>{t('agentExecution.profile.skillsLabel', { count: candidate.enabledSkills.length })}</b>
                        <span className={styles.tags}>
                          {candidate.enabledSkills.map((skill) => (
                            <span key={skill}>{skill}</span>
                          ))}
                        </span>
                      </div>
                    )}
                  </div>
                )}
              </div>
            );
          })}
        </div>
      </Drawer>
    </>
  );
};

export default ParticipantProfilePanel;
