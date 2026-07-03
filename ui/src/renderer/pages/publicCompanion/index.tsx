/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import React, { useState } from 'react';
import { useTranslation } from 'react-i18next';
import { useNavigate } from 'react-router-dom';
import { Button, Spin } from '@arco-design/web-react';
import { Headset, Lock, Plus, SafeRetrieval, Comments } from '@icon-park/react';
import { usePublicAgents } from './usePublicAgents';
import PublicAgentCard from './PublicAgentCard';
import CreatePublicAgentModal from './CreatePublicAgentModal';

/**
 * 对外伙伴（/public-companions）—— 面向陌生人的企业级客服控制台首页（花名册）。
 *
 * 与「桌面伙伴」完全分离的一级域：只做问答 + 知识库检索，高危能力一律关闭。
 * 卡片网格 + 创建 + 空态；点击卡片进入专属管理页 /public-companions/:id。
 */
const PublicCompanionRosterPage: React.FC = () => {
  const { t } = useTranslation();
  const navigate = useNavigate();
  const { agents, loading, create } = usePublicAgents();
  const [createOpen, setCreateOpen] = useState(false);

  const openAgent = (id: string) => void navigate(`/public-companions/${id}`);

  return (
    <div className='w-full min-h-full box-border overflow-y-auto px-16px py-20px'>
      <div className='mx-auto flex w-full max-w-[1160px] box-border flex-col gap-16px'>
        {/* Header */}
        <div className='flex items-start justify-between gap-16px flex-wrap'>
          <div className='flex items-start gap-12px min-w-0'>
            <span
              className='flex items-center justify-center w-40px h-40px rd-11px shrink-0 text-[rgb(var(--primary-6))]'
              style={{
                background: 'linear-gradient(150deg, rgba(var(--primary-5),0.16) 0%, rgba(var(--primary-6),0.26) 100%)',
                border: '1px solid rgba(var(--primary-6),0.22)',
              }}
            >
              <Headset theme='outline' size='22' fill='currentColor' className='block' style={{ lineHeight: 0 }} />
            </span>
            <div className='min-w-0'>
              <h1 className='m-0 mb-3px text-20px font-700 text-t-primary'>
                {t('publicCompanion.title', { defaultValue: '对外伙伴' })}
              </h1>
              <p className='m-0 text-13px text-t-secondary leading-19px max-w-[560px]'>
                {t('publicCompanion.subtitle', {
                  defaultValue: '面向陌生人的企业级客服 —— 窄而深：只做问答与知识库检索，高危能力一律关闭。',
                })}
              </p>
            </div>
          </div>
          <Button type='primary' size='default' className='shrink-0' onClick={() => setCreateOpen(true)}>
            <span className='inline-flex items-center gap-6px'>
              <Plus theme='outline' size='15' fill='currentColor' className='block' style={{ lineHeight: 0 }} />
              {t('publicCompanion.create.action', { defaultValue: '创建对外伙伴' })}
            </span>
          </Button>
        </div>

        {/* Trust banner — what this domain guarantees. */}
        <div
          className='flex flex-wrap items-center gap-x-20px gap-y-8px rd-14px px-16px py-12px border border-solid'
          style={{
            background: 'linear-gradient(135deg, rgba(var(--primary-6),0.06) 0%, rgba(var(--primary-6),0.02) 100%)',
            borderColor: 'rgba(var(--primary-6),0.18)',
          }}
        >
          <span className='inline-flex items-center gap-7px text-12px text-t-secondary'>
            <Comments theme='outline' size='15' fill='rgb(var(--primary-6))' className='block' style={{ lineHeight: 0 }} />
            {t('publicCompanion.trust.qa', { defaultValue: '仅问答与知识检索' })}
          </span>
          <span className='inline-flex items-center gap-7px text-12px text-t-secondary'>
            <Lock theme='outline' size='15' fill='rgb(var(--primary-6))' className='block' style={{ lineHeight: 0 }} />
            {t('publicCompanion.trust.locked', { defaultValue: '终端 / 文件 / 电脑 / 浏览器 等高危能力已关闭' })}
          </span>
          <span className='inline-flex items-center gap-7px text-12px text-t-secondary'>
            <SafeRetrieval theme='outline' size='15' fill='rgb(var(--primary-6))' className='block' style={{ lineHeight: 0 }} />
            {t('publicCompanion.trust.audited', { defaultValue: '全程审计留痕' })}
          </span>
        </div>

        {/* Roster */}
        {loading ? (
          <div className='flex justify-center py-56px'>
            <Spin />
          </div>
        ) : agents.length === 0 ? (
          <div className='flex flex-col items-center gap-14px rd-16px border border-dashed border-[var(--color-border-2)] bg-fill-1 px-20px py-52px text-center'>
            <span
              className='flex items-center justify-center w-56px h-56px rd-16px text-[rgb(var(--primary-6))]'
              style={{
                background: 'linear-gradient(150deg, rgba(var(--primary-5),0.16) 0%, rgba(var(--primary-6),0.28) 100%)',
                border: '1px solid rgba(var(--primary-6),0.22)',
              }}
            >
              <Headset theme='outline' size='28' fill='currentColor' className='block' style={{ lineHeight: 0 }} />
            </span>
            <div className='flex flex-col gap-4px'>
              <span className='text-15px font-600 text-t-primary'>
                {t('publicCompanion.empty.title', { defaultValue: '还没有对外伙伴' })}
              </span>
              <span className='text-13px text-t-tertiary max-w-[440px]'>
                {t('publicCompanion.empty.desc', {
                  defaultValue: '创建一位对外伙伴，绑定公开知识库与服务守则，让它安全地接待陌生用户。',
                })}
              </span>
            </div>
            <Button type='primary' onClick={() => setCreateOpen(true)}>
              <span className='inline-flex items-center gap-6px'>
                <Plus theme='outline' size='15' fill='currentColor' className='block' style={{ lineHeight: 0 }} />
                {t('publicCompanion.empty.action', { defaultValue: '创建第一位对外伙伴' })}
              </span>
            </Button>
          </div>
        ) : (
          <div className='grid gap-16px' style={{ gridTemplateColumns: 'repeat(auto-fill, minmax(min(320px, 100%), 1fr))' }}>
            {agents.map((a) => (
              <PublicAgentCard key={a.id} agent={a} onOpen={() => openAgent(a.id)} />
            ))}
          </div>
        )}
      </div>

      <CreatePublicAgentModal
        visible={createOpen}
        onClose={() => setCreateOpen(false)}
        onCreated={(agent) => openAgent(agent.id)}
        create={create}
      />
    </div>
  );
};

export default PublicCompanionRosterPage;
