/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import React, { useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Button, Message, Modal, Spin } from '@arco-design/web-react';
import { IconDelete, IconEdit, IconPlus } from '@arco-design/web-react/icon';
import { getBaseUrl, isBackendHttpError } from '@/common/adapter/httpBridge';
import type { IFigureMeta } from '@/common/adapter/ipcBridge';
import { figureImageUrlOf } from '@renderer/pages/companion/characters/customMeta';
import { CHECKER_BG } from './CustomFigureWizard/FrameStep';
import CustomFigureWizard from './CustomFigureWizard';
import { FigureActionButton, FigureActionSurface, FigureActionVeil } from './FigureCardActions';
import FigureEditModal from './FigureEditModal';
import { useFigures, useFiguresInUse, type FigureUpdatePatch } from './useFigures';
import type { FigureId } from '@/common/types/ids';

/** One figure tile — thumbnail on a checker ground, inline rename, delete-on-hover. */
const FigureTile: React.FC<{
  fig: IFigureMeta;
  baseUrl: string;
  /** A companion currently uses this figure — deletion is blocked (would dangle). */
  inUse: boolean;
  onUpdate: (id: FigureId, patch: FigureUpdatePatch) => Promise<IFigureMeta>;
  onDelete: (fig: IFigureMeta) => void;
}> = ({ fig, baseUrl, inUse, onUpdate, onDelete }) => {
  const { t } = useTranslation();
  const [editOpen, setEditOpen] = useState(false);

  return (
    <>
      <div className='figure-library-card w-184px h-234px group relative flex shrink-0 flex-col overflow-hidden rd-16px bg-fill-2 border border-solid border-[var(--color-border-2)] shadow-[0_1px_2px_rgba(15,23,42,0.04)] transition-all duration-200 hover:-translate-y-2px hover:shadow-[0_10px_28px_rgba(var(--primary-rgb),0.16)] hover:border-[var(--color-primary)]'>
        {/* "in use" badge — always visible so the blocked delete reads as intentional */}
        {inUse && (
          <span className='absolute left-8px top-8px z-2 inline-flex items-center h-18px px-7px rd-full text-10px font-600 !bg-primary-1 !text-primary-6'>
            {t('nomi.customFigure.inUse')}
          </span>
        )}

        <FigureActionVeil />
        <FigureActionSurface>
          <FigureActionButton
            tone='primary'
            title={t('nomi.customFigure.editFigure')}
            ariaLabel={t('nomi.customFigure.editFigure')}
            onClick={() => setEditOpen(true)}
          >
            <IconEdit className='text-13px' />
          </FigureActionButton>
          <FigureActionButton
            tone='danger'
            disabled={inUse}
            title={inUse ? t('nomi.customFigure.inUseCannotDelete') : t('nomi.customFigure.delete')}
            ariaLabel={inUse ? t('nomi.customFigure.inUseCannotDelete') : t('nomi.customFigure.delete')}
            onClick={() => { if (!inUse) onDelete(fig); }}
          >
            <IconDelete className='text-13px' />
          </FigureActionButton>
        </FigureActionSurface>

        <div className='figure-library-card-preview h-190px flex shrink-0 items-center justify-center overflow-hidden' style={CHECKER_BG}>
          <img
            src={figureImageUrlOf(baseUrl, fig.figure_id, fig.created_at)}
            alt={fig.name}
            draggable={false}
            className='max-h-[88%] max-w-[88%] object-contain drop-shadow-[0_6px_14px_rgba(0,0,0,0.18)]'
          />
        </div>

        <div className='figure-library-card-footer h-44px flex shrink-0 items-center gap-6px px-12px bg-fill-2'>
          <span className='text-13px font-600 text-t-primary truncate' title={fig.name}>
            {fig.name}
          </span>
        </div>
      </div>
      <FigureEditModal open={editOpen} fig={fig} baseUrl={baseUrl} onClose={() => setEditOpen(false)} onSave={onUpdate} />
    </>
  );
};

/**
 * 形象库 — the dedicated, discoverable home for reusable custom figures.
 * A gallery of figure assets (rename / delete) plus a prominent creation entry
 * that opens the DIY workspace. Decoupled from companions: figures live here and are
 * picked when creating/editing a companion.
 */
const FigureLibraryPage: React.FC = () => {
  const { t } = useTranslation();
  const { figures, loading, loaded, remove, update, add } = useFigures();
  const inUse = useFiguresInUse();
  const [wizardOpen, setWizardOpen] = useState(false);
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
          // The backend refuses an in-use figure (409 Conflict) — covers the
          // rare case the roster was momentarily stale when the tile rendered.
          const msg =
            isBackendHttpError(e) && e.code === 'CONFLICT'
              ? t('nomi.customFigure.inUseCannotDelete')
              : `${t('nomi.customFigure.deleteFailed')}: ${String(e)}`;
          Message.error(msg);
        }
      },
    });
  };

  const isEmpty = loaded && figures.length === 0;

  return (
    <div className='flex flex-col gap-16px py-8px'>
      {/* header */}
      <div className='flex items-end justify-between gap-12px flex-wrap'>
        <div className='flex flex-col gap-2px'>
          <h2 className='m-0 text-17px font-700 text-t-primary'>{t('nomi.customFigure.libraryTitle')}</h2>
          <span className='text-12px text-t-tertiary'>{t('nomi.customFigure.homeEntryHint')}</span>
        </div>
        <Button type='primary' icon={<IconPlus />} onClick={() => setWizardOpen(true)}>
          {t('nomi.customFigure.createNew')}
        </Button>
      </div>

      {loading && !loaded ? (
        <div className='flex justify-center py-60px'>
          <Spin />
        </div>
      ) : isEmpty ? (
        <div className='flex flex-col items-center justify-center gap-14px py-56px rd-16px bg-fill-1 border border-dashed border-[var(--color-border-2)]'>
          <div className='flex items-center justify-center w-72px h-72px rd-full bg-primary-1 text-32px text-primary-6'>
            <IconPlus />
          </div>
          <div className='flex flex-col items-center gap-4px'>
            <span className='text-14px font-600 text-t-primary'>{t('nomi.customFigure.libraryEmpty')}</span>
            <span className='text-12px text-t-tertiary'>{t('nomi.customFigure.copyrightHint')}</span>
          </div>
          <Button type='primary' icon={<IconPlus />} onClick={() => setWizardOpen(true)}>
            {t('nomi.customFigure.createNew')}
          </Button>
        </div>
      ) : (
        <div className='grid justify-start gap-14px' style={{ gridTemplateColumns: 'repeat(auto-fill, 184px)' }}>
          {figures.map((fig) => (
            <FigureTile
              key={fig.figure_id}
              fig={fig}
              baseUrl={base}
              inUse={inUse.has(fig.figure_id)}
              onUpdate={update}
              onDelete={confirmDelete}
            />
          ))}
          {/* trailing create tile keeps creation in-reach next to the assets */}
          <button
            type='button'
            onClick={() => setWizardOpen(true)}
            className='figure-library-card w-184px h-234px figure-library-create-card flex shrink-0 flex-col items-center justify-center gap-8px rd-16px bg-fill-1 text-t-tertiary cursor-pointer transition-all duration-200 hover:-translate-y-2px hover:bg-fill-2 hover:text-[var(--color-primary)]'
          >
            <span className='figure-library-create-content flex flex-col items-center gap-8px'>
              <span className='flex items-center justify-center w-44px h-44px rd-full bg-fill-3 text-26px'>
                <IconPlus />
              </span>
              <span className='text-12px font-600'>{t('nomi.customFigure.createNew')}</span>
            </span>
          </button>
        </div>
      )}

      <CustomFigureWizard
        open={wizardOpen}
        onClose={() => setWizardOpen(false)}
        onDone={(figure) => {
          setWizardOpen(false);
          add(figure);
          Message.success(t('nomi.customFigure.savedToLibrary'));
        }}
      />
    </div>
  );
};

export default FigureLibraryPage;
