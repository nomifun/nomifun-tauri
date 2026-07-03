/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import React, { useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Input, Modal } from '@arco-design/web-react';
import type { IPublicAgent } from '@/common/adapter/ipcBridge';
import { useArcoMessage } from '@renderer/utils/ui/useArcoMessage';
import { AgentSeal } from './components';

interface Props {
  visible: boolean;
  onClose: () => void;
  onCreated: (agent: IPublicAgent) => void;
  create: (name: string) => Promise<IPublicAgent>;
}

/** 创建对外伙伴 —— 只需名称，其余（话术/知识库/守则/模型）在专属管理页配置。 */
const CreatePublicAgentModal: React.FC<Props> = ({ visible, onClose, onCreated, create }) => {
  const { t } = useTranslation();
  const [message, holder] = useArcoMessage();
  const [name, setName] = useState('');
  const [submitting, setSubmitting] = useState(false);

  const reset = () => {
    setName('');
    setSubmitting(false);
  };

  const handleSubmit = async () => {
    const trimmed = name.trim();
    if (!trimmed) {
      message.warning(t('publicCompanion.create.nameRequired', { defaultValue: '请填写对外伙伴名称' }));
      return;
    }
    setSubmitting(true);
    try {
      const created = await create(trimmed);
      message.success(t('publicCompanion.create.ok', { defaultValue: '已创建对外伙伴' }));
      onCreated(created);
      onClose();
      reset();
    } catch (e) {
      message.error(e instanceof Error ? e.message : String(e));
      setSubmitting(false);
    }
  };

  return (
    <Modal
      title={
        <span className='flex items-center gap-10px'>
          <AgentSeal size={28} />
          {t('publicCompanion.create.title', { defaultValue: '创建对外伙伴' })}
        </span>
      }
      visible={visible}
      onCancel={() => {
        onClose();
        reset();
      }}
      onOk={() => void handleSubmit()}
      confirmLoading={submitting}
      okText={t('publicCompanion.create.submit', { defaultValue: '创建' })}
      cancelText={t('common.cancel', { defaultValue: '取消' })}
      autoFocus
      unmountOnExit
    >
      {holder}
      <div className='flex flex-col gap-10px'>
        <p className='m-0 text-13px text-t-secondary leading-19px'>
          {t('publicCompanion.create.desc', {
            defaultValue: '为面向陌生人的客服起个名字。创建后可在专属管理页配置话术、模型、知识库与服务守则。',
          })}
        </p>
        <div className='flex flex-col gap-6px'>
          <span className='text-13px font-500 text-t-primary'>
            {t('publicCompanion.create.nameLabel', { defaultValue: '名称' })}
          </span>
          <Input
            value={name}
            onChange={setName}
            maxLength={40}
            showWordLimit
            placeholder={t('publicCompanion.create.namePlaceholder', { defaultValue: '例如：官网售前客服' })}
            onPressEnter={() => void handleSubmit()}
          />
        </div>
      </div>
    </Modal>
  );
};

export default CreatePublicAgentModal;
