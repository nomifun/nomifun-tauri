/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

/**
 * CreateTextAssetModal — author a `kind = "text"` asset in the library. Handy
 * for distilling reusable prompts. Title + content are required; an optional
 * collection can be picked or typed.
 */

import React, { useEffect, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Input, Modal, Select } from '@arco-design/web-react';

import type { CreateTextAssetBody, WorkshopAsset } from '../types';
import { useArcoMessage } from '@renderer/utils/ui/useArcoMessage';

export interface CreateTextAssetModalProps {
  visible: boolean;
  collections: string[];
  onClose: () => void;
  onCreate: (body: Omit<CreateTextAssetBody, 'kind'>) => Promise<WorkshopAsset>;
}

const CreateTextAssetModal: React.FC<CreateTextAssetModalProps> = ({ visible, collections, onClose, onCreate }) => {
  const { t } = useTranslation();
  const [message, holder] = useArcoMessage();

  const [title, setTitle] = useState('');
  const [content, setContent] = useState('');
  const [collection, setCollection] = useState<string | undefined>(undefined);
  const [saving, setSaving] = useState(false);

  useEffect(() => {
    if (visible) {
      setTitle('');
      setContent('');
      setCollection(undefined);
      setSaving(false);
    }
  }, [visible]);

  const collectionOptions = collections.map((c) => ({ label: c, value: c }));

  const handleSubmit = async () => {
    const trimmedTitle = title.trim();
    const trimmedContent = content.trim();
    if (!trimmedTitle) {
      message.warning(t('workshopAssets.newText.titleRequired', { defaultValue: '请输入标题' }));
      return;
    }
    if (!trimmedContent) {
      message.warning(t('workshopAssets.newText.contentRequired', { defaultValue: '请输入内容' }));
      return;
    }
    setSaving(true);
    try {
      await onCreate({
        title: trimmedTitle,
        text_content: trimmedContent,
        ...(collection ? { collection } : {}),
      });
      message.success(t('workshopAssets.newText.created', { defaultValue: '文本资产已创建' }));
      onClose();
    } catch (e) {
      setSaving(false);
      message.error(
        `${t('workshopAssets.newText.createFailed', { defaultValue: '创建失败' })}: ${e instanceof Error ? e.message : String(e)}`
      );
    }
  };

  return (
    <Modal
      title={t('workshopAssets.newText.title', { defaultValue: '新建文本资产' })}
      visible={visible}
      onCancel={onClose}
      onOk={() => void handleSubmit()}
      confirmLoading={saving}
      okText={t('workshopAssets.newText.submit', { defaultValue: '创建' })}
      cancelText={t('workshopAssets.newText.cancel', { defaultValue: '取消' })}
      autoFocus={false}
      unmountOnExit
    >
      {holder}
      <div className='flex flex-col gap-14px'>
        <label className='flex flex-col gap-6px'>
          <span className='text-13px font-500 text-[var(--color-text-1)]'>
            {t('workshopAssets.newText.titleLabel', { defaultValue: '标题' })}
          </span>
          <Input
            value={title}
            onChange={setTitle}
            maxLength={120}
            placeholder={t('workshopAssets.newText.titlePlaceholder', { defaultValue: '给这段文本起个名字' })}
          />
        </label>

        <label className='flex flex-col gap-6px'>
          <span className='text-13px font-500 text-[var(--color-text-1)]'>
            {t('workshopAssets.newText.contentLabel', { defaultValue: '内容' })}
          </span>
          <Input.TextArea
            value={content}
            onChange={setContent}
            autoSize={{ minRows: 5, maxRows: 14 }}
            placeholder={t('workshopAssets.newText.contentPlaceholder', {
              defaultValue: '输入文本内容，可用于沉淀常用提示词',
            })}
          />
        </label>

        <label className='flex flex-col gap-6px'>
          <span className='text-13px font-500 text-[var(--color-text-1)]'>
            {t('workshopAssets.newText.collectionLabel', { defaultValue: '集合' })}
          </span>
          <Select
            allowClear
            allowCreate
            showSearch
            value={collection}
            onChange={(v) => setCollection(v as string | undefined)}
            options={collectionOptions}
            placeholder={t('workshopAssets.newText.collectionPlaceholder', {
              defaultValue: '选择或输入集合名（可选）',
            })}
          />
        </label>
      </div>
    </Modal>
  );
};

export default CreateTextAssetModal;
