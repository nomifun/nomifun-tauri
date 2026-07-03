/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import React, { useEffect, useMemo, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Button, Input } from '@arco-design/web-react';
import { Message, SpeakerOne, User } from '@icon-park/react';
import type { IPublicAgent, IPublicAgentPatch } from '@/common/adapter/ipcBridge';
import type { ArcoMessageInstance } from '@renderer/utils/ui/useArcoMessage';
import { SectionCard, FieldRow } from '../components';

interface Props {
  agent: IPublicAgent;
  patch: (p: IPublicAgentPatch) => Promise<IPublicAgent | undefined>;
  message: ArcoMessageInstance;
}

/** 身份 & 话术 —— 名称、开场白、语气规范。改动累积到草稿，统一保存。
 *  对话模型不在此处：它是一切回复的前置条件，已上移到「概览」页就地可配。 */
const IdentitySection: React.FC<Props> = ({ agent, patch, message }) => {
  const { t } = useTranslation();

  const [name, setName] = useState(agent.name);
  const [greeting, setGreeting] = useState(agent.greeting);
  const [tone, setTone] = useState(agent.tone);
  const [saving, setSaving] = useState(false);

  // Re-seed the draft whenever the authoritative agent changes (e.g. after a reload).
  useEffect(() => {
    setName(agent.name);
    setGreeting(agent.greeting);
    setTone(agent.tone);
  }, [agent.id, agent.name, agent.greeting, agent.tone]);

  const dirty = useMemo(
    () => name !== agent.name || greeting !== agent.greeting || tone !== agent.tone,
    [name, greeting, tone, agent]
  );

  const discard = () => {
    setName(agent.name);
    setGreeting(agent.greeting);
    setTone(agent.tone);
  };

  const save = async () => {
    const trimmed = name.trim();
    if (!trimmed) {
      message.warning(t('publicCompanion.identity.nameRequired', { defaultValue: '名称不能为空' }));
      return;
    }
    setSaving(true);
    try {
      await patch({ name: trimmed, greeting, tone });
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
        icon={<User theme='outline' size='16' fill='currentColor' className='block' style={{ lineHeight: 0 }} />}
        title={t('publicCompanion.identity.title', { defaultValue: '身份 & 话术' })}
        desc={t('publicCompanion.identity.desc', {
          defaultValue: '设定对外伙伴的名称、开场问候，以及回答陌生人时应遵循的语气。',
        })}
      >
        <div className='flex flex-col gap-6px'>
          <FieldRow
            label={t('publicCompanion.identity.nameLabel', { defaultValue: '名称' })}
            hint={t('publicCompanion.identity.nameHint', { defaultValue: '展示给内部与访客的名字。' })}
          >
            <Input value={name} onChange={setName} maxLength={40} showWordLimit style={{ maxWidth: 360 }} />
          </FieldRow>

          <FieldRow
            label={t('publicCompanion.identity.greetingLabel', { defaultValue: '开场白' })}
            hint={t('publicCompanion.identity.greetingHint', { defaultValue: '访客开启对话时看到的第一句欢迎语。' })}
          >
            <Input.TextArea
              value={greeting}
              onChange={setGreeting}
              autoSize={{ minRows: 2, maxRows: 5 }}
              maxLength={500}
              showWordLimit
              placeholder={t('publicCompanion.identity.greetingPlaceholder', {
                defaultValue: '例如：您好，我是官网客服小助手，很高兴为您解答产品相关问题～',
              })}
            />
          </FieldRow>

          <FieldRow
            label={t('publicCompanion.identity.toneLabel', { defaultValue: '语气规范' })}
            hint={t('publicCompanion.identity.toneHint', { defaultValue: '希望它以怎样的口吻回复（专业 / 亲切 / 简洁…）。' })}
          >
            <Input.TextArea
              value={tone}
              onChange={setTone}
              autoSize={{ minRows: 2, maxRows: 5 }}
              maxLength={500}
              showWordLimit
              placeholder={t('publicCompanion.identity.tonePlaceholder', {
                defaultValue: '例如：始终礼貌、专业、简洁；多用「您」；不确定时坦诚说明并引导留下联系方式。',
              })}
            />
          </FieldRow>
        </div>
      </SectionCard>

      {/* Sticky-ish save bar */}
      <div className='flex items-center justify-end gap-10px'>
        {dirty && (
          <span className='mr-auto inline-flex items-center gap-6px text-12px text-t-tertiary'>
            <SpeakerOne theme='outline' size='13' fill='currentColor' className='block' style={{ lineHeight: 0 }} />
            {t('publicCompanion.identity.unsaved', { defaultValue: '有未保存的改动' })}
          </span>
        )}
        <Button disabled={!dirty || saving} onClick={discard}>
          {t('publicCompanion.identity.discard', { defaultValue: '放弃' })}
        </Button>
        <Button type='primary' loading={saving} disabled={!dirty} onClick={() => void save()}>
          <span className='inline-flex items-center gap-6px'>
            <Message theme='outline' size='14' fill='currentColor' className='block' style={{ lineHeight: 0 }} />
            {t('common.save', { defaultValue: '保存' })}
          </span>
        </Button>
      </div>
    </div>
  );
};

export default IdentitySection;
