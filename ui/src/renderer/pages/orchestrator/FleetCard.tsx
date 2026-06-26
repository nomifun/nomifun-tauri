/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import React, { useMemo } from 'react';
import { useTranslation } from 'react-i18next';
import { Avatar, Dropdown, Menu, Popconfirm, Tooltip } from '@arco-design/web-react';
import { Delete, More, PeopleTopCard, Split } from '@icon-park/react';
import classNames from 'classnames';
import type { TFleet } from '@/common/types/orchestrator/orchestratorTypes';
import type { AgentMetadata } from '@/renderer/utils/model/agentTypes';
import { resolveAgentLogo } from '@/renderer/utils/model/agentLogo';

interface FleetCardProps {
  fleet: TFleet;
  agentsById: Map<string, AgentMetadata>;
  onEdit: () => void;
  onDelete: () => void;
}

const MAX_AVATARS = 5;

/**
 * A single fleet card: name + member count, a stacked row of member avatars,
 * model chips, a max-parallel badge, and a quiet corner「⋯」menu carrying a
 * delete with二次确认 (Popconfirm). Clicking the body opens the edit drawer.
 *
 * Mirrors `AgentCard`'s card chrome (rounded, border-2 hairline, bg-2 surface,
 * hover border lift). All colors via theme variables.
 */
const FleetCard: React.FC<FleetCardProps> = ({ fleet, agentsById, onEdit, onDelete }) => {
  const { t } = useTranslation();

  const members = fleet.members;
  const visibleMembers = members.slice(0, MAX_AVATARS);
  const overflow = members.length - visibleMembers.length;

  // Distinct, non-empty model names across members → compact chip row.
  const modelChips = useMemo(() => {
    const seen = new Set<string>();
    const chips: string[] = [];
    for (const m of members) {
      if (m.model && !seen.has(m.model)) {
        seen.add(m.model);
        chips.push(m.model);
      }
    }
    return chips;
  }, [members]);

  const memberLabel = (agentId: string): string => agentsById.get(agentId)?.name ?? t('orchestrator.fleet.card.unknownAgent');

  const memberLogo = (agentId: string): string | null => {
    const agent = agentsById.get(agentId);
    if (!agent) return null;
    return resolveAgentLogo({
      icon: agent.icon,
      backend: agent.backend || agent.agent_type,
      custom_agent_id: agent.id,
      isExtension: agent.agent_source === 'extension',
    });
  };

  return (
    <div
      role='button'
      tabIndex={0}
      onClick={onEdit}
      onKeyDown={(e) => {
        if (e.key === 'Enter' || e.key === ' ') {
          e.preventDefault();
          onEdit();
        }
      }}
      className={classNames(
        'group relative flex flex-col rd-12px border border-solid p-16px cursor-pointer select-none transition-colors',
        'border-[var(--color-border-2)] bg-[var(--color-bg-2)] hover:border-[var(--color-primary-light-3)]'
      )}
    >
      {/* Header: icon + name + member count, corner menu */}
      <div className='flex items-start gap-10px'>
        <span className='size-34px shrink-0 rd-10px bg-[var(--color-primary-light-1)] text-[rgb(var(--primary-6))] flex items-center justify-center'>
          <PeopleTopCard theme='outline' size='18' strokeWidth={3} />
        </span>
        <div className='min-w-0 flex-1 pr-22px'>
          <div className='text-14px font-600 text-t-primary truncate leading-tight'>{fleet.name}</div>
          <div className='mt-3px text-12px text-t-tertiary truncate'>
            {fleet.description?.trim()
              ? fleet.description
              : t('orchestrator.fleet.card.memberCount', { count: members.length })}
          </div>
        </div>
        <div className='absolute right-10px top-12px' onClick={(e) => e.stopPropagation()}>
          <Dropdown
            trigger='click'
            position='br'
            getPopupContainer={() => document.body}
            droplist={
              <Menu>
                <Menu.Item key='delete' className='!p-0'>
                  <Popconfirm
                    title={t('orchestrator.fleet.card.deleteConfirm', { name: fleet.name })}
                    okText={t('common.delete', { defaultValue: 'Delete' })}
                    cancelText={t('common.cancel', { defaultValue: 'Cancel' })}
                    onOk={onDelete}
                    getPopupContainer={() => document.body}
                  >
                    <div className='flex items-center gap-7px px-12px py-6px text-13px text-[rgb(var(--danger-6))]'>
                      <Delete theme='outline' size='14' strokeWidth={3} />
                      {t('orchestrator.fleet.card.delete')}
                    </div>
                  </Popconfirm>
                </Menu.Item>
              </Menu>
            }
          >
            <div
              role='button'
              tabIndex={0}
              aria-label={t('orchestrator.fleet.card.menu')}
              className={classNames(
                'size-24px rd-7px flex items-center justify-center cursor-pointer transition-colors',
                'text-t-tertiary opacity-0 group-hover:opacity-100 hover:bg-fill-2 hover:text-t-primary'
              )}
            >
              <More theme='outline' size='16' strokeWidth={3} />
            </div>
          </Dropdown>
        </div>
      </div>

      {/* Member avatars */}
      <div className='mt-14px flex items-center gap-6px min-h-28px'>
        {members.length === 0 ? (
          <span className='text-12px text-t-tertiary'>{t('orchestrator.fleet.card.noMembers')}</span>
        ) : (
          <>
            <div className='flex items-center'>
              {visibleMembers.map((m, i) => {
                const logo = memberLogo(m.agent_id);
                return (
                  <Tooltip key={m.id || `${m.agent_id}-${i}`} content={memberLabel(m.agent_id)}>
                    <span
                      className={classNames(
                        'size-28px rd-full flex items-center justify-center bg-[var(--color-bg-1)] border-2 border-solid border-[var(--color-bg-2)]',
                        i > 0 && '-ml-8px'
                      )}
                    >
                      {logo ? (
                        <img src={logo} alt='' className='size-18px object-contain' />
                      ) : (
                        <Avatar size={24} style={{ backgroundColor: 'var(--color-fill-2)', fontSize: 13 }}>
                          🤖
                        </Avatar>
                      )}
                    </span>
                  </Tooltip>
                );
              })}
            </div>
            {overflow > 0 && <span className='text-12px text-t-tertiary'>+{overflow}</span>}
          </>
        )}
      </div>

      {/* Footer: model chips + max-parallel badge */}
      <div className='mt-14px flex items-center justify-between gap-8px'>
        <div className='flex items-center gap-6px min-w-0 flex-wrap'>
          {modelChips.slice(0, 2).map((model) => (
            <span
              key={model}
              className='max-w-140px truncate text-11px leading-18px px-7px rd-6px bg-[var(--color-fill-2)] text-t-secondary'
            >
              {model}
            </span>
          ))}
          {modelChips.length > 2 && (
            <span className='text-11px leading-18px px-6px rd-6px bg-[var(--color-fill-2)] text-t-tertiary'>
              +{modelChips.length - 2}
            </span>
          )}
          {modelChips.length === 0 && (
            <span className='text-11px text-t-tertiary'>{t('orchestrator.fleet.card.noModel')}</span>
          )}
        </div>
        {typeof fleet.max_parallel === 'number' && fleet.max_parallel > 0 && (
          <Tooltip content={t('orchestrator.fleet.card.maxParallelHint')}>
            <span className='shrink-0 flex items-center gap-4px text-11px leading-18px px-7px rd-6px bg-[var(--color-primary-light-1)] text-[rgb(var(--primary-6))] cursor-help'>
              <Split theme='outline' size='12' strokeWidth={3} />
              {fleet.max_parallel}
            </span>
          </Tooltip>
        )}
      </div>
    </div>
  );
};

export default FleetCard;
