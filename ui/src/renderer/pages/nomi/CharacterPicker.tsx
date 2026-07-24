/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import React, { useState } from 'react';
import { useTranslation } from 'react-i18next';
import classNames from 'classnames';
import { Message, Modal } from '@arco-design/web-react';
import { IconDelete, IconPlus } from '@arco-design/web-react/icon';
import { getBaseUrl, isBackendHttpError } from '@/common/adapter/httpBridge';
import type { IFigureMeta } from '@/common/adapter/ipcBridge';
import type { FigureId } from '@/common/types/ids';
import { CHARACTERS, CUSTOM_CHARACTER_ID } from '@renderer/pages/companion/characters';
import type { CompanionMood } from '@renderer/pages/companion/characters';
import { figureImageUrlOf } from '@renderer/pages/companion/characters/customMeta';
import { CHECKER_BG } from './CustomFigureWizard/FrameStep';
import CustomFigureWizard from './CustomFigureWizard';
import { FigureActionButton, FigureActionSurface, FigureActionVeil } from './FigureCardActions';
import { useFigures, useFiguresInUse } from './useFigures';

/**
 * Character + figure picker. Shows the built-in roster, the user's reusable
 * **figure library** (each selectable / deletable), and a "new custom figure"
 * card that opens the DIY wizard immediately (no confirm-first detour). The
 * wizard creates a library figure and auto-selects it on completion.
 */
const CharacterPicker: React.FC<{
  /** Selected character id ('mochi'…'custom'). */
  value: string;
  /** Selected library figure id when `value === 'custom'`. */
  figureId?: FigureId;
  /** A built-in roster character was chosen. */
  onSelectCharacter: (id: string) => void;
  /** A library figure was chosen (or just created). */
  onSelectFigure: (figure: IFigureMeta) => void;
}> = ({ value, figureId, onSelectCharacter, onSelectFigure }) => {
  const { t } = useTranslation();
  const [hovered, setHovered] = useState<string | null>(null);
  const [wizardOpen, setWizardOpen] = useState(false);
  const { figures, remove, add } = useFigures();
  const inUse = useFiguresInUse();
  const base = getBaseUrl();

  const confirmDelete = (fig: IFigureMeta): void => {
    Modal.confirm({
      title: t('nomi.customFigure.libraryTitle'),
      content: t('nomi.customFigure.deleteConfirm'),
      okButtonProps: { status: 'danger' },
      onOk: async () => {
        try {
          await remove(fig.figure_id);
        } catch (e) {
          const msg =
            isBackendHttpError(e) && e.code === 'CONFLICT'
              ? t('nomi.customFigure.inUseCannotDelete')
              : `${t('nomi.customFigure.deleteFailed')}: ${String(e)}`;
          Message.error(msg);
        }
      },
    });
  };

  return (
    <>
      <div className='grid grid-cols-3 gap-10px max-[720px]:grid-cols-2'>
        {CHARACTERS.map((c) => {
          const active = c.id === value && value !== CUSTOM_CHARACTER_ID;
          const mood: CompanionMood = hovered === c.id ? 'excited' : 'content';
          return (
            <div
              key={c.id}
              onClick={() => onSelectCharacter(c.id)}
              onMouseEnter={() => setHovered(c.id)}
              onMouseLeave={() => setHovered((h) => (h === c.id ? null : h))}
              className={classNames(
                'flex flex-col items-center gap-6px rd-12px px-10px pt-12px pb-10px cursor-pointer transition-all border-2 border-solid',
                active ? 'border-[var(--color-primary)] !bg-primary-1 shadow-[0_4px_14px_rgba(var(--primary-rgb),0.25)]' : 'border-transparent bg-fill-2 hover:bg-fill-3'
              )}
            >
              <c.Component mood={mood} activity='idle' size={84} />
              <div className='flex items-center gap-6px'>
                <span className='flex shrink-0 overflow-hidden rd-full w-14px h-14px border border-solid border-[var(--color-border-2)]'>
                  <span className='w-1/2 h-full' style={{ background: c.palette[0] }} />
                  <span className='w-1/2 h-full' style={{ background: c.palette[1] }} />
                </span>
                <span className={classNames('text-13px font-600', active ? 'text-[var(--color-primary)]' : 'text-t-primary')}>
                  {t(`nomi.characters.${c.nameKey}.name`)}
                </span>
              </div>
              <span className='text-11px text-t-tertiary text-center leading-snug'>
                {t(`nomi.characters.${c.nameKey}.style`)}
              </span>
            </div>
          );
        })}

        {/* Library figures — each selectable, delete on hover. */}
        {figures.map((fig) => {
          const active = value === CUSTOM_CHARACTER_ID && figureId === fig.figure_id;
          const used = inUse.has(fig.figure_id);
          return (
            <div
              key={fig.figure_id}
              onClick={() => onSelectFigure(fig)}
              className={classNames(
                'group relative flex flex-col items-center gap-6px rd-12px px-10px pt-12px pb-10px cursor-pointer overflow-hidden transition-all border-2 border-solid',
                active ? 'border-[var(--color-primary)] !bg-primary-1 shadow-[0_4px_14px_rgba(var(--primary-rgb),0.25)]' : 'border-transparent bg-fill-2 hover:bg-fill-3'
              )}
            >
              <FigureActionVeil className='h-44px' />
              <FigureActionSurface>
                <FigureActionButton
                  tone='danger'
                  disabled={used}
                  title={used ? t('nomi.customFigure.inUseCannotDelete') : t('nomi.customFigure.delete')}
                  ariaLabel={used ? t('nomi.customFigure.inUseCannotDelete') : t('nomi.customFigure.delete')}
                  onClick={(e) => {
                    e.stopPropagation();
                    if (!used) confirmDelete(fig);
                  }}
                >
                  <IconDelete className='text-13px' />
                </FigureActionButton>
              </FigureActionSurface>
              <span className='flex items-center justify-center h-84px w-full rd-8px overflow-hidden' style={CHECKER_BG}>
                <img
                  src={figureImageUrlOf(base, fig.figure_id, fig.created_at)}
                  alt={fig.name}
                  draggable={false}
                  className='max-h-84px max-w-full object-contain'
                />
              </span>
              <span className={classNames('text-13px font-600 truncate max-w-full', active ? 'text-[var(--color-primary)]' : 'text-t-primary')}>
                {fig.name}
              </span>
              <span className='text-11px text-t-tertiary'>{t('nomi.customFigure.cardLabel')}</span>
            </div>
          );
        })}

        {/* New / import custom figure — opens the wizard immediately. */}
        <div
          onClick={() => setWizardOpen(true)}
          className='flex flex-col items-center justify-center gap-6px rd-12px px-10px pt-12px pb-10px cursor-pointer transition-all border-2 border-dashed border-[var(--color-border-2)] bg-fill-2 hover:bg-fill-3 hover:border-[var(--color-primary)]'
        >
          <span className='flex items-center justify-center h-84px text-32px text-t-tertiary'>
            <IconPlus />
          </span>
          <span className='text-13px font-600 text-t-primary'>{t('nomi.customFigure.createNew')}</span>
          <span className='text-11px text-t-tertiary text-center leading-snug'>{t('nomi.customFigure.cardHint')}</span>
        </div>
      </div>

      <CustomFigureWizard
        open={wizardOpen}
        onClose={() => setWizardOpen(false)}
        onDone={(figure) => {
          setWizardOpen(false);
          add(figure);
          onSelectFigure(figure);
        }}
      />
    </>
  );
};

export default CharacterPicker;
