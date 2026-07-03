/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import React from 'react';
import { useTranslation } from 'react-i18next';
import { Tooltip } from '@arco-design/web-react';
import { BookOne, Connection, Right, SafeRetrieval } from '@icon-park/react';
import type { IPublicAgent } from '@/common/adapter/ipcBridge';
import { AgentSeal, StatusPill } from './components';

interface Props {
  agent: IPublicAgent;
  onOpen: () => void;
}

/** One compact metric tile inside a card. */
const Metric: React.FC<{ icon: React.ReactNode; value: React.ReactNode; label: string; active?: boolean }> = ({
  icon,
  value,
  label,
  active,
}) => (
  <div className='flex-1 min-w-0 flex items-center gap-8px rd-10px bg-fill-1 px-10px py-8px'>
    <span
      className={[
        'flex shrink-0 items-center justify-center w-26px h-26px rd-8px',
        active ? 'text-[rgb(var(--primary-6))] bg-[rgba(var(--primary-6),0.12)]' : 'text-t-tertiary bg-fill-3',
      ].join(' ')}
    >
      {icon}
    </span>
    <span className='min-w-0 flex flex-col leading-none'>
      <span className='text-16px font-700 text-t-primary'>{value}</span>
      <span className='mt-3px text-11px text-t-tertiary truncate'>{label}</span>
    </span>
  </div>
);

/**
 * 对外伙伴花名册卡片 —— 企业客服「席位卡」：headset 印记 + 名称 + 启停状态 +
 * 公开知识库 / 绑定渠道 / 关键指标。点击进入专属管理页。
 */
const PublicAgentCard: React.FC<Props> = ({ agent, onOpen }) => {
  const { t } = useTranslation();
  const modelReady = Boolean(agent.model.provider_id && agent.model.model);
  const kbCount = agent.knowledge_base_ids.length;

  return (
    <div
      onClick={onOpen}
      role='button'
      tabIndex={0}
      onKeyDown={(e) => {
        if (e.key === 'Enter' || e.key === ' ') {
          e.preventDefault();
          onOpen();
        }
      }}
      className='group relative flex flex-col overflow-hidden rd-16px border border-solid border-[var(--color-border-2)] bg-[var(--color-bg-2)] cursor-pointer outline-none transition-all hover:border-[rgba(var(--primary-6),0.5)] hover:shadow-[0_12px_30px_rgba(var(--primary-6),0.12)] hover:-translate-y-2px focus-visible:border-[rgb(var(--primary-6))]'
    >
      {/* Identity strip */}
      <div className='flex items-start gap-12px px-16px pt-16px pb-14px'>
        <AgentSeal size={48} enabled={agent.enabled} />
        <div className='min-w-0 flex-1 pt-1px'>
          <div className='flex items-center gap-8px'>
            <span className='text-15px font-700 text-t-primary truncate'>{agent.name}</span>
            <Right
              theme='outline'
              size='15'
              fill='currentColor'
              className='ml-auto shrink-0 text-t-quaternary transition-transform group-hover:translate-x-1px group-hover:text-[rgb(var(--primary-6))]'
              style={{ lineHeight: 0 }}
            />
          </div>
          <div className='mt-6px flex items-center gap-6px flex-wrap'>
            <StatusPill enabled={agent.enabled} t={t} />
            {agent.grounded_mode && (
              <Tooltip content={t('publicCompanion.knowledge.groundedHint')}>
                <span className='inline-flex items-center gap-4px rd-full px-8px py-2px text-11px font-600 leading-none text-[rgb(var(--primary-6))] bg-[rgba(var(--primary-6),0.10)] cursor-help'>
                  <SafeRetrieval theme='outline' size='11' fill='currentColor' className='block' style={{ lineHeight: 0 }} />
                  {t('publicCompanion.knowledge.groundedBadge', { defaultValue: '严格模式' })}
                </span>
              </Tooltip>
            )}
            <span className='inline-flex items-center gap-4px text-11px text-t-tertiary'>
              <span
                className='w-6px h-6px rd-full'
                style={{ background: modelReady ? 'rgb(var(--success-6))' : 'rgb(var(--warning-6))' }}
              />
              {modelReady
                ? t('publicCompanion.card.modelReady', { defaultValue: '模型已配置' })
                : t('publicCompanion.card.modelUnset', { defaultValue: '未配置模型' })}
            </span>
          </div>
        </div>
      </div>

      {/* Metrics */}
      <div className='flex items-stretch gap-8px px-16px pb-16px'>
        <Metric
          active={kbCount > 0}
          icon={<BookOne theme='outline' size='15' fill='currentColor' className='block' style={{ lineHeight: 0 }} />}
          value={kbCount}
          label={t('publicCompanion.card.knowledge', { defaultValue: '公开知识库' })}
        />
        <Tooltip content={t('publicCompanion.card.channelsPending')}>
          <div className='flex-1 min-w-0'>
            <Metric
              icon={<Connection theme='outline' size='15' fill='currentColor' className='block' style={{ lineHeight: 0 }} />}
              value={<span className='text-t-tertiary'>—</span>}
              label={t('publicCompanion.card.channels', { defaultValue: '绑定渠道' })}
            />
          </div>
        </Tooltip>
        <Tooltip content={t('publicCompanion.card.metricPending')}>
          <div className='flex-1 min-w-0'>
            <Metric
              icon={<SafeRetrieval theme='outline' size='15' fill='currentColor' className='block' style={{ lineHeight: 0 }} />}
              value={<span className='text-t-tertiary'>—</span>}
              label={t('publicCompanion.card.served', { defaultValue: '近 7 日服务' })}
            />
          </div>
        </Tooltip>
      </div>
    </div>
  );
};

export default PublicAgentCard;
