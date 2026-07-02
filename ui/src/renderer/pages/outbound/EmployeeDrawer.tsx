/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import React from 'react';
import { useTranslation } from 'react-i18next';
import { Button, Drawer, Modal, Spin, Switch } from '@arco-design/web-react';
import { BookOne, Close, Connect, Message, Shield } from '@icon-park/react';
import type { CompanionExposure, ICompanionProfile } from '@/common/adapter/ipcBridge';
import CompanionAvatar from '@renderer/pages/companion/CompanionAvatar';
import { customFigureMetaOf } from '@renderer/pages/companion/characters/customMeta';
import type { CompanionMood } from '@renderer/pages/companion/characters';
import { useCompanion } from '@renderer/pages/nomi/useNomi';
import CompanionModelControl from '@renderer/pages/nomi/CompanionModelControl';
import RemoteConnectSection from '@renderer/pages/nomi/tabs/RemoteConnectSection';
import KnowledgeControl from '@renderer/pages/conversation/components/KnowledgeControl';
import { useArcoMessage } from '@renderer/utils/ui/useArcoMessage';
import { PUBLIC_SERVICE_EXPOSURE } from './useOutbound';
import AuditLogPanel from './AuditLogPanel';

interface Props {
  companionId: string | null;
  onClose: () => void;
  setExposure: (companionId: string, exposure: CompanionExposure) => Promise<ICompanionProfile>;
  /** Called after 停用 (reverting to private) removes the employee from the roster. */
  onRetired: (companionId: string) => void;
}

/** Titled section card used throughout the drawer body. */
const Section: React.FC<{
  icon: React.ReactNode;
  title: string;
  desc?: string;
  action?: React.ReactNode;
  children: React.ReactNode;
}> = ({ icon, title, desc, action, children }) => (
  <div className='rd-14px border border-solid border-[var(--color-border-2)] bg-[var(--color-bg-1)] p-14px'>
    <div className='flex items-start justify-between gap-10px'>
      <div className='flex items-start gap-9px min-w-0'>
        <span className='mt-1px flex shrink-0 items-center justify-center w-26px h-26px rd-8px text-[rgb(var(--primary-6))] bg-[rgba(var(--primary-6),0.10)]'>
          {icon}
        </span>
        <div className='min-w-0'>
          <div className='text-14px font-600 text-t-primary'>{title}</div>
          {desc && <div className='mt-2px text-12px text-t-tertiary leading-16px'>{desc}</div>}
        </div>
      </div>
      {action && <div className='shrink-0'>{action}</div>}
    </div>
    <div className='mt-10px'>{children}</div>
  </div>
);

/**
 * 外呼员工详情抽屉：身份头 + 对话模型 + 对外服务开关（启用/停用）+ 公开知识库 +
 * 绑定社交渠道 + 审计日志。知识库/渠道复用既有绑定组件（KnowledgeControl /
 * RemoteConnectSection），保证与伙伴管理中心一致的行为与语义。
 */
const EmployeeDrawer: React.FC<Props> = ({ companionId, onClose, setExposure, onRetired }) => {
  const { t } = useTranslation();
  const [message, holder] = useArcoMessage();
  const companion = useCompanion(companionId);
  const { profile, loading } = companion;

  const isServing = profile ? (profile.exposure ?? 'private') === PUBLIC_SERVICE_EXPOSURE : false;

  const confirmRetire = () => {
    if (!companionId || !profile) return;
    Modal.confirm({
      title: t('outbound.drawer.retireTitle', { defaultValue: '停用外呼员工？' }),
      content: t('outbound.drawer.retireBody', {
        defaultValue:
          '停用后该伙伴将退回为普通私有伙伴，并从外呼员工列表移除。伙伴本身及其记忆 / 知识库不会被删除，可随时重新招聘。',
      }),
      okButtonProps: { status: 'warning' },
      okText: t('outbound.drawer.retireConfirm', { defaultValue: '停用' }),
      cancelText: t('common.cancel', { defaultValue: '取消' }),
      onOk: async () => {
        try {
          await setExposure(companionId, 'private');
          message.success(t('outbound.drawer.retired', { defaultValue: '已停用，已退回私有伙伴' }));
          onRetired(companionId);
          onClose();
        } catch (e) {
          message.error(e instanceof Error ? e.message : String(e));
        }
      },
    });
  };

  const handleToggleServing = (checked: boolean) => {
    if (!checked) confirmRetire();
    // Turning it back "on" is a no-op here — a drawer only ever opens an employee
    // that is already serving; re-hiring a retired one goes through 招聘.
  };

  return (
    <Drawer
      title={
        <>
          <span className='flex items-center gap-8px'>
            <span
              className='flex items-center justify-center w-24px h-24px rd-7px text-white'
              style={{ background: 'linear-gradient(160deg, rgb(var(--success-5)), rgb(var(--success-6)))' }}
            >
              <Shield theme='filled' size='14' fill='currentColor' className='block' style={{ lineHeight: 0 }} />
            </span>
            {t('outbound.drawer.title', { defaultValue: '外呼员工详情' })}
          </span>
          <div
            role='button'
            tabIndex={0}
            aria-label={t('common.close', { defaultValue: '关闭' })}
            onClick={onClose}
            onKeyDown={(e) => {
              if (e.key === 'Enter' || e.key === ' ') {
                e.preventDefault();
                onClose();
              }
            }}
            className='absolute right-3 top-3 flex items-center justify-center w-28px h-28px rd-7px text-t-secondary cursor-pointer hover:bg-fill-2 hover:text-t-primary transition-colors'
            style={{ zIndex: 10 } as React.CSSProperties}
          >
            <Close theme='outline' size='16' fill='currentColor' className='block' style={{ lineHeight: 0 }} />
          </div>
        </>
      }
      closable={false}
      visible={!!companionId}
      placement='right'
      width={560}
      getPopupContainer={() => document.body}
      autoFocus={false}
      onCancel={onClose}
      headerStyle={{ background: 'var(--color-bg-1)' }}
      bodyStyle={{ background: 'var(--color-bg-1)', padding: 0 }}
      footer={null}
    >
      {holder}
      {loading || !profile ? (
        <div className='flex justify-center py-60px'>
          <Spin />
        </div>
      ) : (
        <div className='flex flex-col gap-14px overflow-y-auto p-18px'>
          {/* Identity + serving toggle */}
          <div
            className='flex items-center gap-14px rd-14px px-16px py-14px border border-solid border-[rgba(var(--success-6),0.24)]'
            style={{
              background:
                'linear-gradient(135deg, rgba(var(--success-6),0.10) 0%, rgba(var(--primary-6),0.05) 100%)',
            }}
          >
            <div className='relative shrink-0'>
              <CompanionAvatar
                character={profile.character}
                companionId={profile.id}
                customFigure={customFigureMetaOf(profile)}
                mood={(companion.status?.mood as CompanionMood) || 'content'}
                activity='idle'
                size={56}
              />
              <span
                className='absolute -right-2px -bottom-2px flex items-center justify-center w-18px h-18px rd-full text-white border-2 border-[var(--color-bg-2)]'
                style={{ background: 'rgb(var(--success-6))' }}
              >
                <Shield theme='filled' size='10' fill='currentColor' className='block' style={{ lineHeight: 0 }} />
              </span>
            </div>
            <div className='min-w-0 flex-1'>
              <div className='text-16px font-700 text-t-primary truncate'>{profile.name}</div>
              <div className='mt-3px text-12px text-t-tertiary'>
                {t('outbound.drawer.exposureNote', { defaultValue: '公开服务 · 只读问答与知识检索，高危能力已关闭' })}
              </div>
            </div>
            <div className='shrink-0 flex flex-col items-end gap-3px'>
              <Switch checked={isServing} onChange={handleToggleServing} />
              <span className='text-11px text-t-tertiary'>
                {isServing
                  ? t('outbound.drawer.serving', { defaultValue: '对外服务中' })
                  : t('outbound.drawer.retired', { defaultValue: '已停用' })}
              </span>
            </div>
          </div>

          {/* Model */}
          <Section
            icon={<Message theme='outline' size='15' fill='currentColor' className='block' style={{ lineHeight: 0 }} />}
            title={t('outbound.drawer.modelTitle', { defaultValue: '对话模型' })}
            desc={t('outbound.drawer.modelDesc', { defaultValue: '外呼员工回答陌生用户所使用的模型（本地与渠道统一跟随）。' })}
          >
            <CompanionModelControl companion={companion} />
          </Section>

          {/* Public knowledge bases */}
          <Section
            icon={<BookOne theme='outline' size='15' fill='currentColor' className='block' style={{ lineHeight: 0 }} />}
            title={t('outbound.drawer.knowledgeTitle', { defaultValue: '公开知识库' })}
            desc={t('outbound.drawer.knowledgeDesc', { defaultValue: '选择该员工可检索的知识库；只用于对外问答检索。' })}
            action={
              <KnowledgeControl
                target={{ kind: 'companion', id: profile.id }}
                applyNote={t('outbound.drawer.knowledgeApplyNote', {
                  defaultValue: '外呼员工仅做检索问答，请谨慎开启「回血」写入。',
                })}
              />
            }
          >
            <div className='text-12px text-t-tertiary leading-17px'>
              {t('outbound.drawer.knowledgeInline', {
                defaultValue: '点击右上角「知识库」挑选公开知识库，员工即可在对话中检索其中内容。',
              })}
            </div>
          </Section>

          {/* Channels */}
          <Section
            icon={<Connect theme='outline' size='15' fill='currentColor' className='block' style={{ lineHeight: 0 }} />}
            title={t('outbound.drawer.channelsTitle', { defaultValue: '绑定社交渠道' })}
            desc={t('outbound.drawer.channelsDesc', { defaultValue: '把员工接入 IM 渠道，代表你接待陌生用户。' })}
          >
            <div>
              <RemoteConnectSection companionId={profile.id} companionName={profile.name} />
            </div>
          </Section>

          {/* Audit */}
          <div className='rd-14px border border-solid border-[var(--color-border-2)] bg-[var(--color-bg-1)] p-14px'>
            <AuditLogPanel companionId={profile.id} />
          </div>
        </div>
      )}
    </Drawer>
  );
};

export default EmployeeDrawer;
