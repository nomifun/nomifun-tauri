/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import React, { useEffect, useMemo, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { mutate } from 'swr';
import { Button, Drawer, Input, InputNumber, Spin } from '@arco-design/web-react';
import { Close, Plus } from '@icon-park/react';
import { ipcBridge } from '@/common';
import type { TFleet } from '@/common/types/orchestrator/orchestratorTypes';
import { useAgents } from '@/renderer/hooks/agent/useAgents';
import { useModelProviderList } from '@/renderer/hooks/agent/useModelProviderList';
import { useArcoMessage } from '@/renderer/utils/ui/useArcoMessage';
import { ORCH_FLEETS_SWR_KEY } from './useOrchestratorData';
import FleetMemberRow from './FleetMemberRow';
import { blankMemberDraft, fromMember, toMemberInput } from './fleetConstants';
import type { FleetMemberDraft } from './fleetConstants';

interface FleetEditDrawerProps {
  visible: boolean;
  /** When set, the drawer edits this fleet; otherwise it creates a new one. */
  fleet: TFleet | null;
  onClose: () => void;
}

/**
 * Create / edit a fleet. A right-side Arco Drawer with the fleet identity
 * (name / description / max parallel) atop a members editor — a stack of
 * `FleetMemberRow`s with add/remove. Save routes to
 * `ipcBridge.orchestrator.fleets.create|update`, revalidates the shared SWR key,
 * and toasts via `useArcoMessage`. Validates: name non-empty and ≥1 member with
 * an agent selected.
 *
 * Visual language mirrors `AssistantEditDrawer`: a bg-1 shell, a bg-fill-2 inner
 * scroll surface holding labelled field blocks, and a footer with primary
 * Save + ghost Cancel. All colors via theme variables.
 */
const FleetEditDrawer: React.FC<FleetEditDrawerProps> = ({ visible, fleet, onClose }) => {
  const { t } = useTranslation();
  const [message, ctx] = useArcoMessage();
  const isEditing = Boolean(fleet);

  const { agents, isLoading: agentsLoading } = useAgents();
  const { providers, getAvailableModels } = useModelProviderList();

  // Only local engines that are enabled + available make sensible fleet members.
  const selectableAgents = useMemo(
    () => agents.filter((a) => a.enabled && a.available && a.agent_type !== 'remote'),
    [agents]
  );

  const [name, setName] = useState('');
  const [description, setDescription] = useState('');
  const [maxParallel, setMaxParallel] = useState<number | undefined>(undefined);
  const [members, setMembers] = useState<FleetMemberDraft[]>([]);
  const [submitting, setSubmitting] = useState(false);

  // Responsive drawer width (mirrors AssistantEditDrawer).
  const [drawerWidth, setDrawerWidth] = useState(560);
  useEffect(() => {
    const update = () => {
      if (typeof window === 'undefined') return;
      setDrawerWidth(Math.min(720, Math.max(520, Math.floor(window.innerWidth * 0.5))));
    };
    update();
    window.addEventListener('resize', update);
    return () => window.removeEventListener('resize', update);
  }, []);

  // Reset / hydrate form whenever the drawer opens.
  useEffect(() => {
    if (!visible) return;
    if (fleet) {
      setName(fleet.name);
      setDescription(fleet.description ?? '');
      setMaxParallel(fleet.max_parallel);
      setMembers(fleet.members.map(fromMember));
    } else {
      setName('');
      setDescription('');
      setMaxParallel(undefined);
      setMembers([blankMemberDraft()]);
    }
  }, [visible, fleet]);

  const updateMember = (key: string, next: FleetMemberDraft) =>
    setMembers((prev) => prev.map((m) => (m.key === key ? next : m)));

  const removeMember = (key: string) => setMembers((prev) => prev.filter((m) => m.key !== key));

  const addMember = () => setMembers((prev) => [...prev, blankMemberDraft()]);

  const handleSave = async () => {
    const trimmedName = name.trim();
    if (!trimmedName) {
      message.warning(t('orchestrator.fleet.edit.nameRequired'));
      return;
    }
    const validMembers = members.filter((m) => m.agent_id);
    if (validMembers.length === 0) {
      message.warning(t('orchestrator.fleet.edit.membersRequired'));
      return;
    }

    const memberInputs = validMembers.map((m, i) => toMemberInput(m, i));
    setSubmitting(true);
    try {
      if (fleet) {
        await ipcBridge.orchestrator.fleets.update.invoke({
          id: fleet.id,
          updates: {
            name: trimmedName,
            description: description.trim() || undefined,
            max_parallel: maxParallel,
            members: memberInputs,
          },
        });
        message.success(t('orchestrator.fleet.edit.updateOk'));
      } else {
        await ipcBridge.orchestrator.fleets.create.invoke({
          name: trimmedName,
          description: description.trim() || undefined,
          max_parallel: maxParallel,
          members: memberInputs,
        });
        message.success(t('orchestrator.fleet.edit.createOk'));
      }
      await mutate(ORCH_FLEETS_SWR_KEY);
      onClose();
    } catch (e) {
      message.error(t('orchestrator.fleet.edit.saveError', { error: String(e) }));
    } finally {
      setSubmitting(false);
    }
  };

  return (
    <>
      {ctx}
      <Drawer
        title={
          <>
            <span>{isEditing ? t('orchestrator.fleet.edit.editTitle') : t('orchestrator.fleet.edit.createTitle')}</span>
            <div
              onClick={(e) => {
                e.stopPropagation();
                onClose();
              }}
              className='absolute right-4 top-2 cursor-pointer text-t-secondary hover:text-t-primary transition-colors p-1'
              style={{ zIndex: 10 } as React.CSSProperties}
            >
              <Close size={18} />
            </div>
          </>
        }
        closable={false}
        visible={visible}
        placement='right'
        width={drawerWidth}
        zIndex={1200}
        getPopupContainer={() => document.body}
        autoFocus={false}
        onCancel={onClose}
        headerStyle={{ background: 'var(--color-bg-1)' }}
        bodyStyle={{ background: 'var(--color-bg-1)' }}
        footer={
          <div className='flex items-center gap-8px'>
            <Button
              type='primary'
              loading={submitting}
              onClick={() => void handleSave()}
              className='w-[110px] rounded-[100px]'
            >
              {isEditing ? t('common.save', { defaultValue: 'Save' }) : t('common.create', { defaultValue: 'Create' })}
            </Button>
            <Button onClick={onClose} className='w-[100px] rounded-[100px] bg-fill-2'>
              {t('common.cancel', { defaultValue: 'Cancel' })}
            </Button>
          </div>
        }
      >
        <div className='flex flex-col h-full overflow-hidden'>
          <div className='flex flex-col flex-1 gap-18px bg-fill-2 rounded-16px p-20px overflow-y-auto'>
            {/* Name */}
            <div className='flex-shrink-0'>
              <div className='text-13px font-600 text-t-primary'>
                <span className='text-[rgb(var(--danger-6))]'>*</span> {t('orchestrator.fleet.edit.nameLabel')}
              </div>
              <Input
                className='mt-8px rounded-8px bg-bg-1'
                value={name}
                onChange={setName}
                allowClear
                autoFocus
                placeholder={t('orchestrator.fleet.edit.namePlaceholder')}
              />
            </div>

            {/* Description */}
            <div className='flex-shrink-0'>
              <div className='text-13px font-600 text-t-primary'>{t('orchestrator.fleet.edit.descLabel')}</div>
              <Input.TextArea
                className='mt-8px rounded-8px bg-bg-1'
                value={description}
                onChange={setDescription}
                autoSize={{ minRows: 2, maxRows: 4 }}
                placeholder={t('orchestrator.fleet.edit.descPlaceholder')}
              />
            </div>

            {/* Max parallel */}
            <div className='flex-shrink-0'>
              <div className='text-13px font-600 text-t-primary'>{t('orchestrator.fleet.edit.maxParallelLabel')}</div>
              <div className='mt-8px flex items-center gap-8px'>
                <InputNumber
                  className='w-160px'
                  min={1}
                  max={64}
                  value={maxParallel}
                  onChange={(value?: number) => setMaxParallel(value ?? undefined)}
                  placeholder={t('orchestrator.fleet.edit.maxParallelPlaceholder')}
                />
                <span className='text-12px text-t-tertiary'>{t('orchestrator.fleet.edit.maxParallelHint')}</span>
              </div>
            </div>

            {/* Members editor */}
            <div className='flex-shrink-0'>
              <div className='flex items-center justify-between'>
                <div className='text-13px font-600 text-t-primary'>
                  <span className='text-[rgb(var(--danger-6))]'>*</span> {t('orchestrator.fleet.edit.membersLabel')}
                  <span className='ml-6px text-12px font-400 text-t-tertiary'>
                    {t('orchestrator.fleet.edit.membersCount', { count: members.filter((m) => m.agent_id).length })}
                  </span>
                </div>
                <Button
                  size='small'
                  type='outline'
                  icon={<Plus theme='outline' size='13' strokeWidth={4} />}
                  onClick={addMember}
                  className='rounded-[100px]'
                >
                  {t('orchestrator.fleet.edit.addMember')}
                </Button>
              </div>

              {agentsLoading ? (
                <div className='mt-12px py-24px flex items-center justify-center'>
                  <Spin />
                </div>
              ) : (
                <div className='mt-12px flex flex-col gap-10px'>
                  {members.length === 0 ? (
                    <div className='py-20px text-center text-12px text-t-tertiary rd-12px border border-dashed border-[var(--color-border-2)]'>
                      {t('orchestrator.fleet.edit.membersEmpty')}
                    </div>
                  ) : (
                    members.map((member, i) => (
                      <FleetMemberRow
                        key={member.key}
                        index={i}
                        member={member}
                        agents={selectableAgents}
                        providers={providers}
                        getAvailableModels={getAvailableModels}
                        onChange={(next) => updateMember(member.key, next)}
                        onRemove={() => removeMember(member.key)}
                      />
                    ))
                  )}
                </div>
              )}
            </div>
          </div>
        </div>
      </Drawer>
    </>
  );
};

export default FleetEditDrawer;
