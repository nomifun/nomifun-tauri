/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import React, { useEffect, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Alert, Button, Input, Modal } from '@arco-design/web-react';
import type { IFigureMeta } from '@/common/adapter/ipcBridge';
import { figureImageUrlOf } from '@renderer/pages/companion/characters/customMeta';
import FrameStep, { clampHeadBox, type HeadBox } from './CustomFigureWizard/FrameStep';
import type { FigureUpdatePatch } from './useFigures';
import type { FigureId } from '@/common/types/ids';

const round4 = (v: number): number => Math.round(v * 10000) / 10000;

const headBoxOf = (fig: IFigureMeta): HeadBox => {
  const hb = fig.head_box;
  return clampHeadBox(hb.x, hb.y, hb.w, hb.h ?? hb.w * fig.aspect);
};

const roundHeadBox = (hb: HeadBox): HeadBox => ({
  x: round4(hb.x),
  y: round4(hb.y),
  w: round4(hb.w),
  h: round4(hb.h),
});

const FigureEditModal: React.FC<{
  open: boolean;
  fig: IFigureMeta;
  baseUrl: string;
  onClose: () => void;
  onSave: (id: FigureId, patch: FigureUpdatePatch) => Promise<IFigureMeta>;
}> = ({ open, fig, baseUrl, onClose, onSave }) => {
  const { t } = useTranslation();
  const [name, setName] = useState(fig.name);
  const [headBox, setHeadBox] = useState<HeadBox>(() => headBoxOf(fig));
  const [sizeTier, setSizeTier] = useState<'s' | 'm' | 'l'>(fig.size_tier);
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    if (!open) return;
    setName(fig.name);
    setHeadBox(headBoxOf(fig));
    setSizeTier(fig.size_tier);
    setSaving(false);
    setError(null);
  }, [fig, open]);

  const save = async (): Promise<void> => {
    if (saving) return;
    setSaving(true);
    setError(null);
    try {
      await onSave(fig.figure_id, {
        name: name.trim(),
        head_box: roundHeadBox(headBox),
        size_tier: sizeTier,
      });
      onClose();
    } catch (err) {
      setError(`${t('nomi.customFigure.errFailed')}: ${String(err)}`);
    } finally {
      setSaving(false);
    }
  };

  return (
    <Modal
      title={t('nomi.customFigure.editFigure')}
      visible={open}
      onCancel={onClose}
      footer={null}
      maskClosable={false}
      unmountOnExit
      style={{ width: 720 }}
    >
      <div className='figure-edit-modal flex flex-col gap-14px'>
        {error && <Alert type='error' content={error} />}
        <FrameStep
          imageUrl={figureImageUrlOf(baseUrl, fig.figure_id, fig.created_at)}
          aspect={fig.aspect}
          headBox={headBox}
          onHeadBoxChange={setHeadBox}
          sizeTier={sizeTier}
          onSizeTierChange={setSizeTier}
        />
        <div className='flex items-center gap-10px rd-12px bg-fill-2 px-14px py-10px'>
          <span className='text-13px text-t-secondary shrink-0'>{t('nomi.customFigure.nameLabel')}</span>
          <Input
            value={name}
            onChange={setName}
            placeholder={t('nomi.customFigure.namePlaceholder')}
            maxLength={40}
            allowClear
            style={{ flex: 1 }}
          />
        </div>
        <div className='figure-modal-footer flex items-center justify-end gap-8px pt-10px'>
          <Button type='text' status='danger' onClick={onClose} disabled={saving}>
            {t('nomi.customFigure.discard')}
          </Button>
          <Button type='primary' loading={saving} onClick={() => void save()}>
            {t('nomi.customFigure.saveChanges')}
          </Button>
        </div>
      </div>
    </Modal>
  );
};

export default FigureEditModal;
