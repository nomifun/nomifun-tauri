/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import React, { useMemo } from 'react';
import { useTranslation } from 'react-i18next';
import { InputNumber, Select } from '@arco-design/web-react';
import { Delete } from '@icon-park/react';
import classNames from 'classnames';
import type { AgentMetadata } from '@/renderer/utils/model/agentTypes';
import type { IProvider } from '@/common/config/storage';
import { resolveAgentLogo } from '@/renderer/utils/model/agentLogo';
import { COST_TIER_KEYS, STRENGTH_KEYS } from './fleetConstants';
import type { FleetMemberDraft } from './fleetConstants';

/** Resolve a logo url for an agent metadata row (mirrors AgentCard). */
const agentLogo = (agent: AgentMetadata): string | null =>
  resolveAgentLogo({
    icon: agent.icon,
    backend: agent.backend || agent.agent_type,
    custom_agent_id: agent.id,
    isExtension: agent.agent_source === 'extension',
  });

interface FleetMemberRowProps {
  index: number;
  member: FleetMemberDraft;
  agents: AgentMetadata[];
  providers: IProvider[];
  getAvailableModels: (provider: IProvider) => string[];
  onChange: (next: FleetMemberDraft) => void;
  onRemove: () => void;
}

/**
 * One editable fleet-member row: agent + provider/model + role hint + strength
 * tags + constraints (max concurrency, cost tier). Laid out as a self-contained
 * card so a vertical stack of rows reads as a clean list inside the drawer.
 *
 * Provider+model mirror `CompanionModelControl` (two dependent selects driven by
 * `useModelProviderList`). Strength tags are a free-form multi-select seeded with
 * a curated vocabulary; cost tier is a small enumerated select. All colors flow
 * through theme variables / UnoCSS tokens.
 */
const FleetMemberRow: React.FC<FleetMemberRowProps> = ({
  index,
  member,
  agents,
  providers,
  getAvailableModels,
  onChange,
  onRemove,
}) => {
  const { t } = useTranslation();

  const currentProvider = useMemo(
    () => providers.find((p) => p.id === member.provider_id),
    [providers, member.provider_id]
  );

  const selectedAgent = useMemo(() => agents.find((a) => a.id === member.agent_id), [agents, member.agent_id]);
  const selectedLogo = selectedAgent ? agentLogo(selectedAgent) : null;

  const strengthOptions = useMemo(
    () =>
      STRENGTH_KEYS.map((key) => ({
        label: t(`orchestrator.fleet.strength.${key}` as const),
        value: key,
      })),
    [t]
  );

  return (
    <div className='rd-12px border border-solid border-[var(--color-border-2)] bg-[var(--color-bg-2)] p-12px'>
      {/* Row header: ordinal badge + agent picker + remove */}
      <div className='flex items-center gap-10px'>
        <span className='size-22px shrink-0 rd-7px bg-[var(--color-primary-light-1)] text-[rgb(var(--primary-6))] text-12px font-600 flex items-center justify-center'>
          {index + 1}
        </span>
        <Select
          className='flex-1 min-w-0'
          size='small'
          placeholder={t('orchestrator.fleet.member.agentPlaceholder')}
          value={member.agent_id || undefined}
          onChange={(agent_id: string) => onChange({ ...member, agent_id })}
          showSearch
          filterOption={(input, option) => {
            const id = (option as React.ReactElement<{ value?: string }>)?.props?.value;
            const name = agents.find((a) => a.id === id)?.name ?? '';
            return name.toLowerCase().includes(input.toLowerCase());
          }}
          renderFormat={() =>
            selectedAgent ? (
              <span className='flex items-center gap-6px min-w-0'>
                <span className='size-16px shrink-0 flex items-center justify-center'>
                  {selectedLogo ? (
                    <img src={selectedLogo} alt='' className='size-16px object-contain' />
                  ) : (
                    <span className='text-12px leading-none'>🤖</span>
                  )}
                </span>
                <span className='truncate'>{selectedAgent.name}</span>
              </span>
            ) : (
              <span className='text-t-tertiary'>{t('orchestrator.fleet.member.agentPlaceholder')}</span>
            )
          }
        >
          {agents.map((agent) => {
            const logo = agentLogo(agent);
            return (
              <Select.Option key={agent.id} value={agent.id}>
                <span className='flex items-center gap-8px'>
                  <span className='size-18px shrink-0 flex items-center justify-center'>
                    {logo ? <img src={logo} alt='' className='size-18px object-contain' /> : <span>🤖</span>}
                  </span>
                  {agent.name}
                </span>
              </Select.Option>
            );
          })}
        </Select>
        <div
          role='button'
          tabIndex={0}
          aria-label={t('orchestrator.fleet.member.remove')}
          onClick={onRemove}
          onKeyDown={(e) => {
            if (e.key === 'Enter' || e.key === ' ') {
              e.preventDefault();
              onRemove();
            }
          }}
          className={classNames(
            'size-26px shrink-0 rd-7px flex items-center justify-center cursor-pointer transition-colors',
            'text-t-tertiary hover:text-[rgb(var(--danger-6))] hover:bg-[rgba(var(--danger-6),0.1)]'
          )}
        >
          <Delete theme='outline' size='15' strokeWidth={3} />
        </div>
      </div>

      {/* Provider + model */}
      <div className='mt-10px flex items-center gap-8px'>
        <Select
          className='flex-1 min-w-0'
          size='small'
          placeholder={t('orchestrator.fleet.member.providerPlaceholder')}
          value={member.provider_id || undefined}
          allowClear
          onChange={(provider_id?: string) => onChange({ ...member, provider_id, model: undefined })}
          options={providers.map((p) => ({ label: p.name, value: p.id }))}
        />
        <Select
          className='flex-1 min-w-0'
          size='small'
          placeholder={t('orchestrator.fleet.member.modelPlaceholder')}
          value={member.model || undefined}
          allowClear
          disabled={!currentProvider}
          onChange={(model?: string) => onChange({ ...member, model })}
          options={(currentProvider ? getAvailableModels(currentProvider) : []).map((m) => ({ label: m, value: m }))}
        />
      </div>

      {/* Role hint + constraints */}
      <div className='mt-10px grid gap-8px' style={{ gridTemplateColumns: 'minmax(0, 1.6fr) minmax(0, 1fr) minmax(0, 1fr)' }}>
        <Select
          size='small'
          allowCreate
          showSearch
          allowClear
          placeholder={t('orchestrator.fleet.member.roleHintPlaceholder')}
          value={member.role_hint || undefined}
          onChange={(role_hint?: string) => onChange({ ...member, role_hint: role_hint || undefined })}
          options={['planner', 'coder', 'reviewer', 'researcher', 'tester'].map((r) => ({
            label: t(`orchestrator.fleet.role.${r}` as const),
            value: r,
          }))}
        />
        <InputNumber
          size='small'
          min={1}
          max={64}
          placeholder={t('orchestrator.fleet.member.concurrencyPlaceholder')}
          value={member.max_concurrency}
          onChange={(value?: number) => onChange({ ...member, max_concurrency: value ?? undefined })}
        />
        <Select
          size='small'
          allowClear
          placeholder={t('orchestrator.fleet.member.costTierPlaceholder')}
          value={member.cost_tier || undefined}
          onChange={(cost_tier?: string) => onChange({ ...member, cost_tier: cost_tier || undefined })}
          options={COST_TIER_KEYS.map((key) => ({ label: t(`orchestrator.fleet.costTier.${key}` as const), value: key }))}
        />
      </div>

      {/* Strength tags */}
      <div className='mt-10px'>
        <Select
          mode='multiple'
          size='small'
          allowCreate
          placeholder={t('orchestrator.fleet.member.strengthsPlaceholder')}
          value={member.strengths}
          onChange={(strengths: string[]) => onChange({ ...member, strengths })}
          options={strengthOptions}
          maxTagCount={4}
        />
      </div>
    </div>
  );
};

export default FleetMemberRow;
