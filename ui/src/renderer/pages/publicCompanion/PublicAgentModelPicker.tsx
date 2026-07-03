/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import React, { useMemo } from 'react';
import { useTranslation } from 'react-i18next';
import { Select } from '@arco-design/web-react';
import { useModelProviderList } from '@renderer/hooks/agent/useModelProviderList';
import type { IPublicAgentModel } from '@/common/adapter/ipcBridge';

interface Props {
  value: IPublicAgentModel;
  /** Emits the FULL model ref on any change (provider reset clears the model). */
  onChange: (model: IPublicAgentModel) => void;
}

/**
 * 对外伙伴回答陌生人所用模型的配置控件 —— 两枚内联 Select（provider + model）。
 * 换 provider 时清空 model，保证不会遗留跨 provider 的无效组合。
 */
const PublicAgentModelPicker: React.FC<Props> = ({ value, onChange }) => {
  const { t } = useTranslation();
  const { providers, getAvailableModels } = useModelProviderList();

  const currentProvider = useMemo(
    () => providers.find((p) => p.id === value.provider_id),
    [providers, value.provider_id]
  );

  return (
    <div className='flex items-center gap-8px flex-wrap'>
      <Select
        size='default'
        style={{ width: 200 }}
        placeholder={t('publicCompanion.identity.modelProvider', { defaultValue: '选择模型提供商' })}
        value={value.provider_id || undefined}
        onChange={(provider_id: string) => onChange({ provider_id, model: '' })}
      >
        {providers.map((p) => (
          <Select.Option key={p.id} value={p.id}>
            {p.name}
          </Select.Option>
        ))}
      </Select>
      <Select
        size='default'
        style={{ width: 240 }}
        placeholder={t('publicCompanion.identity.modelName', { defaultValue: '选择模型' })}
        value={value.model || undefined}
        disabled={!currentProvider}
        onChange={(model: string) => onChange({ provider_id: value.provider_id, model })}
      >
        {(currentProvider ? getAvailableModels(currentProvider) : []).map((m) => (
          <Select.Option key={m} value={m}>
            {m}
          </Select.Option>
        ))}
      </Select>
    </div>
  );
};

export default PublicAgentModelPicker;
