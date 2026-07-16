/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import React, { useCallback, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Input, Message, Modal } from '@arco-design/web-react';
import { IconDelete, IconPlus } from '@arco-design/web-react/icon';
import classNames from 'classnames';
import { ipcBridge } from '@/common';
import type { IFigureMeta, ICompanionProfile, ICompanionWithStatus } from '@/common/adapter/ipcBridge';
import { CUSTOM_CHARACTER_ID, DEFAULT_CHARACTER_ID } from '@renderer/pages/companion/characters';
import { customFigureMetaOf } from '@renderer/pages/companion/characters/customMeta';
import CompanionAvatar from '@renderer/pages/companion/CompanionAvatar';
import type { CompanionMood } from '@renderer/pages/companion/characters';
import CharacterPicker from './CharacterPicker';
import { figureToCustomPatch } from './useFigures';
import type { CompanionId } from '@/common/types/ids';

interface Props {
  companions: ICompanionWithStatus[];
  selectedId: CompanionId | null;
  onSelect: (id: CompanionId) => void;
  onCreated: (profile: ICompanionProfile) => void | Promise<void>;
  /** Called after a companion is deleted (quick-delete) so the page reselects. */
  onDeleted: (companionId: CompanionId) => void;
  className?: string;
}

/**
 * 桌面伙伴「会话切换栏」（竖向）—— 统一的跨伙伴会话入口（问题#2 的核心）。
 *
 * 每个桌面伙伴 = 一条全生命周期专属会话（单会话契约）；点击行即切换右侧会话面板，
 * 这是用户在多个伙伴之间快速切换聊天的唯一、统一入口（取代横向 CompanionSwitcher，
 * 把"选伙伴"从配置维度提升为"切会话"的高频主操作）。
 *
 * 每行：头像（叠加模型就绪状态点：绿=已配置可对话 / 橙=未配置模型）+ 名字 + 默认徽章 + 等级。
 * 顶部"新建桌面伙伴"卡：名字 + 形象（内建角色或自定义形象库）。
 */
const CompanionSessionRail: React.FC<Props> = ({
  companions,
  selectedId,
  onSelect,
  onCreated,
  onDeleted,
  className,
}) => {
  const { t } = useTranslation();
  const [modalVisible, setModalVisible] = useState(false);
  const [name, setName] = useState('');
  const [character, setCharacter] = useState<string>(DEFAULT_CHARACTER_ID);
  /** A library figure chosen for the new companion (overrides `character`). */
  const [selectedFigure, setSelectedFigure] = useState<IFigureMeta | null>(null);
  const [creating, setCreating] = useState(false);

  const openCreate = () => {
    setName('');
    setCharacter(DEFAULT_CHARACTER_ID);
    setSelectedFigure(null);
    setModalVisible(true);
  };

  const submitCreate = async () => {
    const trimmed = name.trim();
    if (!trimmed || creating) return;
    setCreating(true);
    try {
      const profile = await ipcBridge.companion.createCompanion.invoke({
        name: trimmed,
        character: selectedFigure ? CUSTOM_CHARACTER_ID : character,
      });
      // createCompanion only takes name + character; link the library figure via a
      // follow-up patch before onCreated triggers the roster refresh.
      if (selectedFigure) {
        await ipcBridge.companion.patchCompanion.invoke({
          companion_id: profile.id,
          patch: { appearance: { custom_figure: figureToCustomPatch(selectedFigure) } },
        });
      }
      setModalVisible(false);
      try {
        await onCreated(profile);
      } catch (refreshError) {
        Message.warning(
          `${t('nomi.companions.created', { companionName: profile.name })}: ${String(refreshError)}`
        );
        return;
      }
      Message.success(t('nomi.companions.created', { companionName: profile.name }));
    } catch (e) {
      Message.error(String(e));
    } finally {
      setCreating(false);
    }
  };

  /** Quick-delete from the rail row (same confirm + endpoint as SettingsTab). */
  const requestDelete = useCallback(
    (p: ICompanionWithStatus, e: React.MouseEvent) => {
      e.stopPropagation(); // don't also select the row
      Modal.confirm({
        title: t('nomi.settings.deleteConfirmTitle'),
        content: t('nomi.settings.deleteConfirmBody', { companionName: p.name }),
        okButtonProps: { status: 'danger' },
        onOk: async () => {
          try {
            await ipcBridge.companion.deleteCompanion.invoke({ companion_id: p.id });
            Message.success(t('nomi.settings.deleted', { companionName: p.name }));
            onDeleted(p.id);
          } catch (err) {
            Message.error(String(err));
          }
        },
      });
    },
    [onDeleted, t]
  );

  return (
    <div className={classNames('flex flex-col bg-fill-1 rd-12px box-border overflow-hidden', className)}>
      <div
        onClick={openCreate}
        className='shrink-0 m-6px mb-6px flex items-center gap-10px rd-12px px-10px py-9px cursor-pointer bg-[var(--color-bg-2)] border border-solid border-[rgba(var(--primary-6),0.35)] shadow-[0_8px_20px_rgba(var(--primary-rgb),0.10)] hover:border-[var(--color-primary)] hover:shadow-[0_10px_24px_rgba(var(--primary-rgb),0.16)] transition-all box-border'
      >
        <span
          className='shrink-0 flex items-center justify-center w-30px h-30px rd-10px text-white shadow-[0_5px_12px_rgba(var(--primary-rgb),0.22)]'
          style={{ background: 'linear-gradient(180deg, rgb(var(--primary-5)), rgb(var(--primary-6)))' }}
        >
          <IconPlus className='text-18px' />
        </span>
        <span className='flex flex-col gap-2px min-w-0'>
          <span className='text-14px leading-16px font-700 text-t-primary truncate'>{t('nomi.companions.create')}</span>
          <span className='text-11px leading-13px font-500 text-t-tertiary truncate'>
            {t('nomi.companions.createHint')}
          </span>
        </span>
      </div>

      <div className='flex-1 min-h-0 overflow-y-auto px-6px pb-6px pt-0 flex flex-col gap-3px'>
        {companions.map((p) => {
          const active = p.id === selectedId;
          const modelReady = p.model !== null;
          return (
            <div
              key={p.id}
              onClick={() => onSelect(p.id)}
              className={classNames(
                'group flex items-center gap-8px shrink-0 rd-10px px-8px py-6px cursor-pointer transition-colors box-border',
                active ? '!bg-primary-1 !text-primary-6' : 'hover:bg-fill-2 active:bg-fill-3'
              )}
            >
              <div className='relative shrink-0'>
                <CompanionAvatar
                  character={p.character}
                  companionId={p.id}
                  customFigure={customFigureMetaOf(p)}
                  mood={(p.status.mood as CompanionMood) || 'content'}
                  activity='idle'
                  size={34}
                />
                <span
                  className='absolute -right-1px -bottom-1px w-9px h-9px rd-full border-2 border-[var(--color-bg-2)]'
                  style={{ background: modelReady ? 'rgb(var(--success-6))' : 'rgb(var(--warning-6))' }}
                  title={modelReady ? undefined : t('nomi.chat.modelUnset')}
                />
              </div>
              <div className='flex flex-col gap-1px min-w-0 flex-1'>
                <span className='flex items-center gap-4px min-w-0'>
                  <span
                    className={classNames(
                      'text-13px font-600 truncate min-w-0',
                      active ? '!text-primary-6' : 'text-t-primary'
                    )}
                  >
                    {p.name}
                  </span>
                </span>
                <span className={classNames('text-11px', active ? 'text-primary-6 opacity-70' : 'text-t-tertiary')}>
                  Lv{p.status.level}
                </span>
              </div>
              {/* Quick delete — revealed on row hover; uses a <div> (no UnoCSS
                  button reset → real <button> leaks a WebView2 black border). */}
              <div
                role='button'
                aria-label={t('nomi.settings.deleteCompanion')}
                title={t('nomi.settings.deleteCompanion')}
                onClick={(e) => requestDelete(p, e)}
                className='shrink-0 flex items-center justify-center w-22px h-22px rd-6px text-t-tertiary opacity-0 group-hover:opacity-100 hover:!text-[rgb(var(--danger-6))] hover:bg-[var(--color-fill-3)] transition-all cursor-pointer'
              >
                <IconDelete className='text-13px' />
              </div>
            </div>
          );
        })}
        {companions.length === 0 && (
          <div className='flex-1 flex items-center justify-center text-12px text-t-tertiary px-8px text-center py-20px'>
            {t('nomi.companions.empty')}
          </div>
        )}
      </div>

      <Modal
        title={t('nomi.companions.createTitle')}
        visible={modalVisible}
        onOk={() => void submitCreate()}
        onCancel={() => setModalVisible(false)}
        okButtonProps={{ loading: creating, disabled: !name.trim() }}
        style={{ width: 560 }}
      >
        <div className='flex flex-col gap-14px'>
          <div className='flex flex-col gap-6px'>
            <span className='text-13px text-t-secondary'>{t('nomi.companions.nameLabel')}</span>
            <Input
              value={name}
              onChange={setName}
              placeholder={t('nomi.companions.namePlaceholder')}
              maxLength={30}
              onPressEnter={() => void submitCreate()}
            />
          </div>
          <div className='flex flex-col gap-6px'>
            <span className='text-13px text-t-secondary'>{t('nomi.companions.characterLabel')}</span>
            <CharacterPicker
              value={selectedFigure ? CUSTOM_CHARACTER_ID : character}
              figureId={selectedFigure?.id}
              onSelectCharacter={(id) => {
                setCharacter(id);
                setSelectedFigure(null);
              }}
              onSelectFigure={(fig) => setSelectedFigure(fig)}
            />
          </div>
        </div>
      </Modal>
    </div>
  );
};

export default CompanionSessionRail;
