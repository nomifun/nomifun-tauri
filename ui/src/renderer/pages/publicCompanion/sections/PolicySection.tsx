/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import React, { useEffect, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Button, Input } from '@arco-design/web-react';
import { DocDetail, FileText } from '@icon-park/react';
import type { IPublicAgent, IPublicAgentPatch } from '@/common/adapter/ipcBridge';
import type { ArcoMessageInstance } from '@renderer/utils/ui/useArcoMessage';
import { SectionCard } from '../components';

interface Props {
  agent: IPublicAgent;
  patch: (p: IPublicAgentPatch) => Promise<IPublicAgent | undefined>;
  message: ArcoMessageInstance;
}

/** 服务守则 —— 业务范围 / 禁谈话题 / 合规话术。一个大文本域 + 模板，PATCH 保存。 */
const PolicySection: React.FC<Props> = ({ agent, patch, message }) => {
  const { t } = useTranslation();
  const [policy, setPolicy] = useState(agent.service_policy);
  const [saving, setSaving] = useState(false);

  useEffect(() => {
    setPolicy(agent.service_policy);
  }, [agent.id, agent.service_policy]);

  const dirty = policy !== agent.service_policy;

  const template = t('publicCompanion.policy.template', {
    defaultValue:
      '业务范围：\n- 仅回答与本公司产品 / 服务相关的问题。\n\n禁谈话题：\n- 不讨论价格谈判、内部信息、竞品评价、政治宗教等敏感话题。\n\n合规话术：\n- 无法确定时，请引导用户联系人工客服，切勿编造答案。\n- 涉及个人隐私 / 账号操作时，提示用户通过官方渠道验证身份。',
  });

  const insertTemplate = () => {
    setPolicy((prev) => (prev.trim() ? prev : template));
  };

  const save = async () => {
    setSaving(true);
    try {
      await patch({ service_policy: policy });
      message.success(t('common.saveSuccess', { defaultValue: '已保存' }));
    } catch (e) {
      message.error(e instanceof Error ? e.message : String(e));
    } finally {
      setSaving(false);
    }
  };

  return (
    <div className='flex flex-col gap-16px'>
      <SectionCard
        icon={<DocDetail theme='outline' size='16' fill='currentColor' className='block' style={{ lineHeight: 0 }} />}
        title={t('publicCompanion.policy.title', { defaultValue: '服务守则' })}
        desc={t('publicCompanion.policy.desc', {
          defaultValue: '约定对外伙伴的业务范围、禁谈话题与合规话术；它在接待陌生用户时会严格遵循这些规则。',
        })}
        action={
          <Button size='small' onClick={insertTemplate}>
            <span className='inline-flex items-center gap-5px'>
              <FileText theme='outline' size='13' fill='currentColor' className='block' style={{ lineHeight: 0 }} />
              {t('publicCompanion.policy.insertTemplate', { defaultValue: '插入模板' })}
            </span>
          </Button>
        }
      >
        <Input.TextArea
          value={policy}
          onChange={setPolicy}
          autoSize={{ minRows: 10, maxRows: 22 }}
          maxLength={4000}
          showWordLimit
          placeholder={t('publicCompanion.policy.placeholder', {
            defaultValue: '在这里写下对外伙伴必须遵守的服务守则（业务范围 / 禁谈话题 / 合规话术）。点击右上角「插入模板」快速开始。',
          })}
        />
      </SectionCard>

      <div className='flex items-center justify-end'>
        <Button type='primary' loading={saving} disabled={!dirty} onClick={() => void save()}>
          {t('common.save', { defaultValue: '保存' })}
        </Button>
      </div>
    </div>
  );
};

export default PolicySection;
