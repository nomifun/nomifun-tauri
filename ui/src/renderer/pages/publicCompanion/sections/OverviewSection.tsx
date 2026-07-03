/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import React, { useEffect, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Modal, Switch } from '@arco-design/web-react';
import { BookOne, DataSheet, Delete, Message, SafeRetrieval } from '@icon-park/react';
import type { IPublicAgent, IPublicAgentModel, IPublicAgentPatch } from '@/common/adapter/ipcBridge';
import type { ArcoMessageInstance } from '@renderer/utils/ui/useArcoMessage';
import { ipcBridge } from '@/common';
import { SectionCard, StatusPill } from '../components';
import PublicAgentModelPicker from '../PublicAgentModelPicker';

interface Props {
  agent: IPublicAgent;
  patch: (p: IPublicAgentPatch) => Promise<IPublicAgent | undefined>;
  message: ArcoMessageInstance;
  onDeleted: () => void;
}

/** One at-a-glance stat tile. */
const StatTile: React.FC<{ icon: React.ReactNode; value: React.ReactNode; label: string }> = ({
  icon,
  value,
  label,
}) => (
  <div className='flex items-center gap-10px rd-12px border border-solid border-[var(--color-border-2)] bg-fill-1 px-14px py-12px'>
    <span className='flex shrink-0 items-center justify-center w-32px h-32px rd-9px text-[rgb(var(--primary-6))] bg-[rgba(var(--primary-6),0.10)]'>
      {icon}
    </span>
    <div className='min-w-0 flex flex-col leading-none'>
      <span className='text-17px font-700 text-t-primary truncate'>{value}</span>
      <span className='mt-4px text-11px text-t-tertiary truncate'>{label}</span>
    </div>
  </div>
);

/** 概览 —— 状态、关键指标、启停开关、危险区（删除）。 */
const OverviewSection: React.FC<Props> = ({ agent, patch, message, onDeleted }) => {
  const { t } = useTranslation();
  const modelReady = Boolean(agent.model.provider_id && agent.model.model);

  // 对话模型 is edited in-place here (the single authoritative entry for this
  // domain). Persist the moment a COMPLETE (provider + model) selection changes —
  // no separate Save step, mirroring the desktop companion's overview model入口.
  const [model, setModel] = useState<IPublicAgentModel>(agent.model);
  useEffect(() => {
    setModel(agent.model);
  }, [agent.id, agent.model.provider_id, agent.model.model]);

  const onModelChange = async (next: IPublicAgentModel) => {
    setModel(next); // reflect the in-progress selection (incl. provider-only interim)
    const complete = Boolean(next.provider_id && next.model);
    const changed = next.provider_id !== agent.model.provider_id || next.model !== agent.model.model;
    if (!complete || !changed) return;
    try {
      await patch({ model: next });
      message.success(t('publicCompanion.overview.modelSaved', { defaultValue: '对话模型已更新' }));
    } catch (e) {
      message.error(e instanceof Error ? e.message : String(e));
      setModel(agent.model); // revert the picker on failure
    }
  };

  const toggleEnabled = async (checked: boolean) => {
    try {
      await patch({ enabled: checked });
      message.success(
        checked
          ? t('publicCompanion.overview.enabledOk', { defaultValue: '已启用，开始对外服务' })
          : t('publicCompanion.overview.disabledOk', { defaultValue: '已停用，暂停对外服务' })
      );
    } catch (e) {
      message.error(e instanceof Error ? e.message : String(e));
    }
  };

  const confirmDelete = () => {
    Modal.confirm({
      title: t('publicCompanion.overview.deleteTitle', { defaultValue: '删除对外伙伴？' }),
      content: t('publicCompanion.overview.deleteBody', {
        defaultValue: '删除后该对外伙伴的配置与审计记录将一并移除，且无法恢复。绑定的知识库本身不会被删除。',
      }),
      okButtonProps: { status: 'danger' },
      okText: t('common.delete', { defaultValue: '删除' }),
      cancelText: t('common.cancel', { defaultValue: '取消' }),
      onOk: async () => {
        try {
          await ipcBridge.publicAgent.remove.invoke({ id: agent.id });
          message.success(t('publicCompanion.overview.deletedOk', { defaultValue: '已删除' }));
          onDeleted();
        } catch (e) {
          message.error(e instanceof Error ? e.message : String(e));
        }
      },
    });
  };

  return (
    <div className='flex flex-col gap-16px'>
      {/* 对话模型 —— 一切回复的前置条件，置顶就地可配（本域唯一入口）。未指定则用系统默认。 */}
      <SectionCard
        icon={<Message theme='outline' size='16' fill='currentColor' className='block' style={{ lineHeight: 0 }} />}
        title={t('publicCompanion.overview.modelTitle', { defaultValue: '对话模型' })}
        desc={t('publicCompanion.overview.modelDesc', {
          defaultValue: '对外伙伴回答陌生人所用的模型 —— 一切对话能力的前置条件。就地选择，立即生效。',
        })}
        action={
          modelReady ? (
            <span className='inline-flex items-center gap-5px rd-full px-9px py-2px text-11px font-600 leading-none text-[rgb(var(--success-6))] bg-[rgba(var(--success-6),0.12)] border border-solid border-[rgba(var(--success-6),0.26)]'>
              <span className='w-6px h-6px rd-full' style={{ background: 'rgb(var(--success-6))' }} />
              {t('publicCompanion.overview.modelReady', { defaultValue: '已配置' })}
            </span>
          ) : (
            <span className='inline-flex items-center gap-5px rd-full px-9px py-2px text-11px font-600 leading-none text-[rgb(var(--warning-6))] bg-[rgba(var(--warning-6),0.12)] border border-solid border-[rgba(var(--warning-6),0.28)]'>
              <span className='w-6px h-6px rd-full' style={{ background: 'rgb(var(--warning-6))' }} />
              {t('publicCompanion.overview.modelDefault', { defaultValue: '使用默认' })}
            </span>
          )
        }
      >
        <div className='flex flex-col gap-10px'>
          <PublicAgentModelPicker value={model} onChange={(m) => void onModelChange(m)} />
          {!modelReady && (
            <div
              className='flex items-start gap-7px rd-10px px-11px py-9px text-12px leading-18px'
              style={{
                background: 'rgba(var(--warning-6),0.08)',
                border: '1px solid rgba(var(--warning-6),0.20)',
                color: 'var(--color-text-2)',
              }}
            >
              <SafeRetrieval
                theme='outline'
                size='14'
                fill='rgb(var(--warning-6))'
                className='block shrink-0'
                style={{ lineHeight: 0, marginTop: 1 }}
              />
              <span>
                {t('publicCompanion.overview.modelDefaultHint', {
                  defaultValue: '尚未指定专属模型时，将自动使用系统默认模型作答；建议为它固定一个模型，保证回答稳定一致。',
                })}
              </span>
            </div>
          )}
        </div>
      </SectionCard>

      {/* Status + toggle */}
      <SectionCard
        icon={<SafeRetrieval theme='outline' size='16' fill='currentColor' className='block' style={{ lineHeight: 0 }} />}
        title={t('publicCompanion.overview.statusTitle', { defaultValue: '服务状态' })}
        desc={t('publicCompanion.overview.statusDesc', {
          defaultValue: '启用后对外伙伴开始接待陌生用户；停用则暂停一切对外服务。',
        })}
        action={<Switch checked={agent.enabled} onChange={(c) => void toggleEnabled(c)} />}
      >
        <div className='flex items-center gap-10px'>
          <StatusPill enabled={agent.enabled} t={t} />
          <span className='text-12px text-t-tertiary'>
            {t('publicCompanion.overview.createdAt', {
              defaultValue: '创建于 {{date}}',
              date: new Date(agent.created_at).toLocaleDateString(),
            })}
          </span>
        </div>
      </SectionCard>

      {/* Quick metrics */}
      <div className='grid gap-12px' style={{ gridTemplateColumns: 'repeat(auto-fill, minmax(min(200px, 100%), 1fr))' }}>
        <StatTile
          icon={<BookOne theme='outline' size='17' fill='currentColor' className='block' style={{ lineHeight: 0 }} />}
          value={agent.knowledge_base_ids.length}
          label={t('publicCompanion.overview.metricKb', { defaultValue: '公开知识库' })}
        />
        <StatTile
          icon={<SafeRetrieval theme='outline' size='17' fill='currentColor' className='block' style={{ lineHeight: 0 }} />}
          value={
            agent.grounded_mode
              ? t('publicCompanion.overview.groundedOn', { defaultValue: '严格' })
              : t('publicCompanion.overview.groundedOff', { defaultValue: '宽松' })
          }
          label={t('publicCompanion.overview.metricGrounded', { defaultValue: '知识库模式' })}
        />
        <StatTile
          icon={<DataSheet theme='outline' size='17' fill='currentColor' className='block' style={{ lineHeight: 0 }} />}
          value={t('publicCompanion.overview.retentionDays', { defaultValue: '{{n}} 天', n: agent.audit_retention_days })}
          label={t('publicCompanion.overview.metricRetention', { defaultValue: '审计保留' })}
        />
      </div>

      {/* Danger zone */}
      <div className='rd-14px border border-solid border-[rgba(var(--danger-6),0.28)] bg-[rgba(var(--danger-6),0.04)] p-16px'>
        <div className='flex items-start justify-between gap-12px flex-wrap'>
          <div className='min-w-0'>
            <div className='text-14px font-600 text-t-primary'>
              {t('publicCompanion.overview.dangerTitle', { defaultValue: '删除对外伙伴' })}
            </div>
            <div className='mt-2px text-12px text-t-tertiary leading-17px max-w-[520px]'>
              {t('publicCompanion.overview.dangerDesc', {
                defaultValue: '永久删除该对外伙伴及其审计记录。此操作不可撤销。',
              })}
            </div>
          </div>
          <div
            role='button'
            tabIndex={0}
            onClick={confirmDelete}
            onKeyDown={(e) => {
              if (e.key === 'Enter' || e.key === ' ') {
                e.preventDefault();
                confirmDelete();
              }
            }}
            className='inline-flex shrink-0 items-center gap-6px rd-8px px-12px h-32px text-13px font-500 cursor-pointer transition-colors text-[rgb(var(--danger-6))] border border-solid border-[rgba(var(--danger-6),0.4)] hover:bg-[rgba(var(--danger-6),0.10)]'
          >
            <Delete theme='outline' size='14' fill='currentColor' className='block' style={{ lineHeight: 0 }} />
            {t('common.delete', { defaultValue: '删除' })}
          </div>
        </div>
      </div>
    </div>
  );
};

export default OverviewSection;
