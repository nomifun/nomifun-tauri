/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import React, { useMemo } from 'react';
import { useTranslation } from 'react-i18next';
import { useNavigate, useParams, useSearchParams } from 'react-router-dom';
import { Button, Empty, Spin } from '@arco-design/web-react';
import {
  BookOne,
  Connection,
  DocDetail,
  History,
  Left,
  SafeRetrieval,
  User,
} from '@icon-park/react';
import { useArcoMessage } from '@renderer/utils/ui/useArcoMessage';
import { usePublicAgent } from './usePublicAgents';
import { AgentSeal, StatusPill } from './components';
import OverviewSection from './sections/OverviewSection';
import IdentitySection from './sections/IdentitySection';
import KnowledgeSection from './sections/KnowledgeSection';
import PolicySection from './sections/PolicySection';
import AuditSection from './sections/AuditSection';
import ChannelsSection from './sections/ChannelsSection';

// Order = discoverability of the two setup essentials first: 概览 (which now hosts
// the 对话模型 config — the hard prerequisite for every reply) then 渠道部署 (where
// it goes live). Behaviour tabs (身份/知识/守则) follow; 审计 stays last (monitoring).
const SECTIONS = ['overview', 'channels', 'identity', 'knowledge', 'policy', 'audit'] as const;
type SectionKey = (typeof SECTIONS)[number];

const iconOf = (key: SectionKey, size = 15): React.ReactNode => {
  const props = { theme: 'outline' as const, size: String(size), fill: 'currentColor', className: 'block', style: { lineHeight: 0 } };
  switch (key) {
    case 'overview':
      return <SafeRetrieval {...props} />;
    case 'identity':
      return <User {...props} />;
    case 'knowledge':
      return <BookOne {...props} />;
    case 'policy':
      return <DocDetail {...props} />;
    case 'audit':
      return <History {...props} />;
    case 'channels':
      return <Connection {...props} />;
  }
};

/**
 * 对外伙伴专属管理页（/public-companions/:id）—— 左侧子导航 + 右侧分区：
 * 概览(含对话模型) / 渠道部署 / 身份&话术 / 知识库 / 服务守则 / 审计&分析。
 */
const PublicAgentDetailPage: React.FC = () => {
  const { t } = useTranslation();
  const navigate = useNavigate();
  const { id = null } = useParams();
  const [searchParams, setSearchParams] = useSearchParams();
  const [message, holder] = useArcoMessage();

  const { agent, loading, patch } = usePublicAgent(id);

  const sectionParam = searchParams.get('section');
  const active: SectionKey = useMemo(
    () => (sectionParam && (SECTIONS as readonly string[]).includes(sectionParam) ? (sectionParam as SectionKey) : 'overview'),
    [sectionParam]
  );

  const sectionLabel = (key: SectionKey): string => {
    switch (key) {
      case 'overview':
        return t('publicCompanion.nav.overview', { defaultValue: '概览' });
      case 'identity':
        return t('publicCompanion.nav.identity', { defaultValue: '身份 & 话术' });
      case 'knowledge':
        return t('publicCompanion.nav.knowledge', { defaultValue: '知识库' });
      case 'policy':
        return t('publicCompanion.nav.policy', { defaultValue: '服务守则' });
      case 'audit':
        return t('publicCompanion.nav.audit', { defaultValue: '审计 & 分析' });
      case 'channels':
        return t('publicCompanion.nav.channels', { defaultValue: '渠道部署' });
    }
  };

  const setSection = (key: SectionKey) =>
    setSearchParams(
      (prev) => {
        prev.set('section', key);
        return prev;
      },
      { replace: true }
    );

  const backToRoster = () => void navigate('/public-companions');

  return (
    <div className='w-full min-h-full box-border overflow-y-auto px-16px py-20px'>
      {holder}
      <div className='mx-auto flex w-full max-w-[1160px] box-border flex-col gap-16px'>
        {/* Back link */}
        <div
          role='button'
          tabIndex={0}
          onClick={backToRoster}
          onKeyDown={(e) => {
            if (e.key === 'Enter' || e.key === ' ') {
              e.preventDefault();
              backToRoster();
            }
          }}
          className='inline-flex w-fit items-center gap-5px text-13px text-t-secondary cursor-pointer hover:text-[rgb(var(--primary-6))] transition-colors'
        >
          <Left theme='outline' size='15' fill='currentColor' className='block' style={{ lineHeight: 0 }} />
          {t('publicCompanion.detail.back', { defaultValue: '返回对外伙伴' })}
        </div>

        {loading ? (
          <div className='flex justify-center py-56px'>
            <Spin />
          </div>
        ) : !agent ? (
          <div className='flex flex-col items-center gap-14px rd-16px border border-dashed border-[var(--color-border-2)] bg-fill-1 px-20px py-52px text-center'>
            <Empty description={t('publicCompanion.detail.notFound', { defaultValue: '找不到该对外伙伴' })} />
            <Button type='primary' onClick={backToRoster}>
              {t('publicCompanion.detail.back', { defaultValue: '返回对外伙伴' })}
            </Button>
          </div>
        ) : (
          <>
            {/* Identity header */}
            <div
              className='flex items-center gap-14px rd-16px px-18px py-16px border border-solid'
              style={{
                background: 'linear-gradient(135deg, rgba(var(--primary-6),0.07) 0%, rgba(var(--primary-6),0.02) 100%)',
                borderColor: 'rgba(var(--primary-6),0.18)',
              }}
            >
              <AgentSeal size={52} enabled={agent.enabled} />
              <div className='min-w-0 flex-1'>
                <div className='text-18px font-700 text-t-primary truncate'>{agent.name}</div>
                <div className='mt-6px flex items-center gap-8px flex-wrap'>
                  <StatusPill enabled={agent.enabled} t={t} />
                  <span className='text-12px text-t-tertiary'>
                    {t('publicCompanion.detail.subtitle', { defaultValue: '面向陌生人的企业级客服' })}
                  </span>
                </div>
              </div>
            </div>

            {/* Sub-nav + content */}
            <div className='flex flex-col md:flex-row gap-16px items-start'>
              <nav className='w-full md:w-208px shrink-0 flex md:flex-col gap-2px overflow-x-auto md:overflow-visible pb-2px md:pb-0'>
                {SECTIONS.map((key) => {
                  const isActive = key === active;
                  return (
                    <div
                      key={key}
                      role='button'
                      tabIndex={0}
                      onClick={() => setSection(key)}
                      onKeyDown={(e) => {
                        if (e.key === 'Enter' || e.key === ' ') {
                          e.preventDefault();
                          setSection(key);
                        }
                      }}
                      className={[
                        'group flex shrink-0 items-center gap-9px rd-10px px-12px h-38px cursor-pointer transition-colors select-none',
                        isActive
                          ? '!bg-[rgba(var(--primary-6),0.10)] !text-[rgb(var(--primary-6))] font-600'
                          : 'text-t-secondary hover:bg-fill-2 hover:text-t-primary',
                      ].join(' ')}
                    >
                      <span className='shrink-0 flex items-center justify-center'>{iconOf(key)}</span>
                      <span className='text-13px whitespace-nowrap'>{sectionLabel(key)}</span>
                    </div>
                  );
                })}
              </nav>

              <div className='flex-1 min-w-0 w-full'>
                {active === 'overview' && (
                  <OverviewSection agent={agent} patch={patch} message={message} onDeleted={backToRoster} />
                )}
                {active === 'identity' && <IdentitySection agent={agent} patch={patch} message={message} />}
                {active === 'knowledge' && <KnowledgeSection agent={agent} patch={patch} message={message} />}
                {active === 'policy' && <PolicySection agent={agent} patch={patch} message={message} />}
                {active === 'audit' && <AuditSection agent={agent} patch={patch} message={message} />}
                {active === 'channels' && <ChannelsSection agent={agent} message={message} />}
              </div>
            </div>
          </>
        )}
      </div>
    </div>
  );
};

export default PublicAgentDetailPage;
