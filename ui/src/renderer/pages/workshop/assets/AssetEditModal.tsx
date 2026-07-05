/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

/**
 * AssetEditModal — rename, re-collection, retag, and move an asset out of the
 * library (`in_library → false`). Submits a partial `patchAsset` via the
 * library controller.
 */

import React, { useEffect, useMemo, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Input, Modal, Select, Switch } from '@arco-design/web-react';

import type { PatchAssetBody, WorkshopAsset } from '../types';
import { useArcoMessage } from '@renderer/utils/ui/useArcoMessage';

export interface AssetEditModalProps {
  asset: WorkshopAsset | null;
  /** Collection names to seed the collection picker. */
  collections: string[];
  onClose: () => void;
  onSubmit: (id: string, patch: PatchAssetBody) => Promise<WorkshopAsset>;
}

const AssetEditModal: React.FC<AssetEditModalProps> = ({ asset, collections, onClose, onSubmit }) => {
  const { t } = useTranslation();
  const [message, holder] = useArcoMessage();

  const [title, setTitle] = useState('');
  const [collection, setCollection] = useState<string | undefined>(undefined);
  const [tags, setTags] = useState<string[]>([]);
  const [removeFromLibrary, setRemoveFromLibrary] = useState(false);
  const [saving, setSaving] = useState(false);

  // Sync form state whenever a new asset is opened.
  useEffect(() => {
    if (!asset) return;
    setTitle(asset.title);
    setCollection(asset.collection ?? undefined);
    setTags(asset.tags ?? []);
    setRemoveFromLibrary(false);
    setSaving(false);
  }, [asset]);

  const collectionOptions = useMemo(() => {
    const set = new Set(collections);
    if (asset?.collection) set.add(asset.collection);
    return [...set].sort((a, b) => a.localeCompare(b)).map((c) => ({ label: c, value: c }));
  }, [collections, asset]);

  const handleSubmit = async () => {
    if (!asset) return;
    const trimmed = title.trim();
    if (!trimmed) {
      message.warning(t('workshopAssets.edit.titleRequired', { defaultValue: '请输入标题' }));
      return;
    }
    const patch: PatchAssetBody = {
      title: trimmed,
      collection: collection ? collection : null,
      tags,
      in_library: !removeFromLibrary,
    };
    setSaving(true);
    try {
      await onSubmit(asset.id, patch);
      message.success(
        removeFromLibrary
          ? t('workshopAssets.edit.removed', { defaultValue: '已移出资产库' })
          : t('workshopAssets.edit.saved', { defaultValue: '已保存' })
      );
      onClose();
    } catch (e) {
      setSaving(false);
      message.error(
        `${t('workshopAssets.edit.saveFailed', { defaultValue: '保存失败' })}: ${e instanceof Error ? e.message : String(e)}`
      );
    }
  };

  return (
    <Modal
      title={t('workshopAssets.edit.title', { defaultValue: '编辑资产' })}
      visible={asset !== null}
      onCancel={onClose}
      onOk={() => void handleSubmit()}
      confirmLoading={saving}
      okText={t('workshopAssets.edit.save', { defaultValue: '保存' })}
      cancelText={t('workshopAssets.edit.cancel', { defaultValue: '取消' })}
      autoFocus={false}
      unmountOnExit
    >
      {holder}
      <div className='flex flex-col gap-14px'>
        <label className='flex flex-col gap-6px'>
          <span className='text-13px font-500 text-[var(--color-text-1)]'>
            {t('workshopAssets.edit.titleLabel', { defaultValue: '标题' })}
          </span>
          <Input
            value={title}
            onChange={setTitle}
            maxLength={120}
            placeholder={t('workshopAssets.edit.titlePlaceholder', { defaultValue: '输入标题' })}
            onPressEnter={() => void handleSubmit()}
          />
        </label>

        <label className='flex flex-col gap-6px'>
          <span className='text-13px font-500 text-[var(--color-text-1)]'>
            {t('workshopAssets.edit.collectionLabel', { defaultValue: '集合' })}
          </span>
          <Select
            allowClear
            allowCreate
            showSearch
            value={collection}
            onChange={(v) => setCollection(v as string | undefined)}
            options={collectionOptions}
            placeholder={t('workshopAssets.edit.collectionPlaceholder', {
              defaultValue: '选择或输入集合名（如角色 / 场景）',
            })}
          />
        </label>

        <label className='flex flex-col gap-6px'>
          <span className='text-13px font-500 text-[var(--color-text-1)]'>
            {t('workshopAssets.edit.tagsLabel', { defaultValue: '标签' })}
          </span>
          <Select
            mode='multiple'
            allowCreate
            allowClear
            value={tags}
            onChange={(v) => setTags(v as string[])}
            placeholder={t('workshopAssets.edit.tagsPlaceholder', { defaultValue: '输入标签后回车' })}
          />
        </label>

        <div className='flex items-start justify-between gap-12px rounded-10px border border-solid border-[var(--color-border-2)] bg-[var(--color-fill-1)] px-12px py-10px'>
          <div className='flex flex-col gap-3px'>
            <span className='text-13px font-500 text-[var(--color-text-1)]'>
              {t('workshopAssets.edit.removeFromLibrary', { defaultValue: '移出资产库' })}
            </span>
            <span className='text-11px leading-[1.5] text-[var(--color-text-3)]'>
              {t('workshopAssets.edit.removeFromLibraryHint', {
                defaultValue: '移出后资产仍会保留在画布中，但不再显示于资产库。',
              })}
            </span>
          </div>
          <Switch checked={removeFromLibrary} onChange={setRemoveFromLibrary} className='mt-2px shrink-0' />
        </div>
      </div>
    </Modal>
  );
};

export default AssetEditModal;
