/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import React, { useCallback, useMemo, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Spin } from '@arco-design/web-react';
import { PeopleTopCard, Plus } from '@icon-park/react';
import classNames from 'classnames';
import { ipcBridge } from '@/common';
import type { TFleet } from '@/common/types/orchestrator/orchestratorTypes';
import type { AgentMetadata } from '@/renderer/utils/model/agentTypes';
import { useAgents } from '@/renderer/hooks/agent/useAgents';
import { useArcoMessage } from '@/renderer/utils/ui/useArcoMessage';
import { useFleets } from './useOrchestratorData';
import FleetCard from './FleetCard';
import FleetEditDrawer from './FleetEditDrawer';

/**
 * FleetManager — the「编队」section of the orchestration page. A responsive card
 * grid of fleets with a「新建编队」action, an empty state, and an edit drawer
 * (create + edit) wired through `ipcBridge.orchestrator.fleets`. Deletes are
 * confirmed inline on each card (Popconfirm) and revalidate the shared SWR key.
 *
 * The grid uses `minmax(min(320px, 100%), 1fr)` so a single card never overflows
 * a narrow content pane (the `min(...)` guard is required).
 */
const FleetManager: React.FC = () => {
  const { t } = useTranslation();
  const [message, ctx] = useArcoMessage();
  const { data: fleets, isLoading, error, mutate } = useFleets();
  const { agents } = useAgents();

  const [drawerOpen, setDrawerOpen] = useState(false);
  const [editingFleet, setEditingFleet] = useState<TFleet | null>(null);

  const agentsById = useMemo(() => {
    const map = new Map<string, AgentMetadata>();
    for (const a of agents) map.set(a.id, a);
    return map;
  }, [agents]);

  const list = fleets ?? [];

  const openCreate = useCallback(() => {
    setEditingFleet(null);
    setDrawerOpen(true);
  }, []);

  const openEdit = useCallback((fleet: TFleet) => {
    setEditingFleet(fleet);
    setDrawerOpen(true);
  }, []);

  const handleDelete = useCallback(
    async (fleet: TFleet) => {
      try {
        await ipcBridge.orchestrator.fleets.remove.invoke({ id: fleet.id });
        message.success(t('orchestrator.fleet.deleteOk'));
        await mutate();
      } catch (e) {
        message.error(t('orchestrator.fleet.deleteError', { error: String(e) }));
      }
    },
    [message, mutate, t]
  );

  return (
    <div className='w-full'>
      {ctx}

      {/* Header */}
      <div className='flex items-center justify-between gap-12px mb-16px'>
        <div className='min-w-0'>
          <div className='text-18px font-600 text-t-primary leading-tight'>{t('orchestrator.fleet.title')}</div>
          <div className='mt-4px text-12px leading-16px text-t-tertiary'>{t('orchestrator.fleet.subtitle')}</div>
        </div>
        <div
          role='button'
          tabIndex={0}
          onClick={openCreate}
          onKeyDown={(e) => {
            if (e.key === 'Enter' || e.key === ' ') {
              e.preventDefault();
              openCreate();
            }
          }}
          className={classNames(
            'shrink-0 h-34px px-14px rd-8px flex items-center gap-6px cursor-pointer select-none',
            'bg-primary-6 text-white hover:opacity-90 active:opacity-80 transition-opacity'
          )}
        >
          <Plus theme='outline' size='15' strokeWidth={4} />
          <span className='text-13px font-500'>{t('orchestrator.fleet.create')}</span>
        </div>
      </div>

      {/* Body */}
      {isLoading ? (
        <div className='py-48px flex items-center justify-center'>
          <Spin />
        </div>
      ) : error ? (
        <div className='py-48px text-center text-13px text-t-tertiary'>{t('orchestrator.fleet.loadError')}</div>
      ) : list.length === 0 ? (
        <div className='py-56px flex flex-col items-center justify-center text-center'>
          <span className='size-52px rd-16px bg-[var(--color-primary-light-1)] text-[rgb(var(--primary-6))] flex items-center justify-center'>
            <PeopleTopCard theme='outline' size='26' strokeWidth={3} />
          </span>
          <div className='mt-14px text-14px font-600 text-t-primary'>{t('orchestrator.fleet.emptyTitle')}</div>
          <div className='mt-4px max-w-360px text-12px leading-18px text-t-tertiary'>
            {t('orchestrator.fleet.emptyDesc')}
          </div>
          <div
            role='button'
            tabIndex={0}
            onClick={openCreate}
            onKeyDown={(e) => {
              if (e.key === 'Enter' || e.key === ' ') {
                e.preventDefault();
                openCreate();
              }
            }}
            className={classNames(
              'mt-18px h-34px px-16px rd-8px flex items-center gap-6px cursor-pointer select-none',
              'bg-primary-6 text-white hover:opacity-90 active:opacity-80 transition-opacity'
            )}
          >
            <Plus theme='outline' size='15' strokeWidth={4} />
            <span className='text-13px font-500'>{t('orchestrator.fleet.create')}</span>
          </div>
        </div>
      ) : (
        <div className='grid gap-12px' style={{ gridTemplateColumns: 'repeat(auto-fill, minmax(min(320px, 100%), 1fr))' }}>
          {list.map((fleet) => (
            <FleetCard
              key={fleet.id}
              fleet={fleet}
              agentsById={agentsById}
              onEdit={() => openEdit(fleet)}
              onDelete={() => void handleDelete(fleet)}
            />
          ))}
        </div>
      )}

      <FleetEditDrawer visible={drawerOpen} fleet={editingFleet} onClose={() => setDrawerOpen(false)} />
    </div>
  );
};

export default FleetManager;
