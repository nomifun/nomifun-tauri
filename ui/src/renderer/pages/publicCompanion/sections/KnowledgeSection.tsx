/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import React, { useEffect, useMemo, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Button, Select, Spin, Switch } from '@arco-design/web-react';
import { BookOne, SafeRetrieval } from '@icon-park/react';
import { ipcBridge } from '@/common';
import type { IKnowledgeBase, IPublicAgent, IPublicAgentPatch } from '@/common/adapter/ipcBridge';
import type { ArcoMessageInstance } from '@renderer/utils/ui/useArcoMessage';
import { SectionCard } from '../components';

interface Props {
  agent: IPublicAgent;
  patch: (p: IPublicAgentPatch) => Promise<IPublicAgent | undefined>;
  message: ArcoMessageInstance;
}

/**
 * 知识库 —— 多选平台知识库（复用既有 knowledge.listBases），保存到 knowledge_base_ids；
 * 外加「严格模式（grounded_mode）」开关：只答知识库内内容。
 */
const KnowledgeSection: React.FC<Props> = ({ agent, patch, message }) => {
  const { t } = useTranslation();
  const [bases, setBases] = useState<IKnowledgeBase[]>([]);
  const [loadingBases, setLoadingBases] = useState(true);
  const [selected, setSelected] = useState<string[]>(agent.knowledge_base_ids);
  const [saving, setSaving] = useState(false);
  const [groundedSaving, setGroundedSaving] = useState(false);

  useEffect(() => {
    setSelected(agent.knowledge_base_ids);
  }, [agent.id, agent.knowledge_base_ids]);

  useEffect(() => {
    let alive = true;
    void (async () => {
      setLoadingBases(true);
      try {
        const list = (await ipcBridge.knowledge.listBases.invoke()) ?? [];
        if (alive) setBases(list);
      } catch {
        if (alive) setBases([]);
      } finally {
        if (alive) setLoadingBases(false);
      }
    })();
    return () => {
      alive = false;
    };
  }, []);

  const dirty = useMemo(() => {
    const a = [...selected].sort().join(',');
    const b = [...agent.knowledge_base_ids].sort().join(',');
    return a !== b;
  }, [selected, agent.knowledge_base_ids]);

  const saveBases = async () => {
    setSaving(true);
    try {
      await patch({ knowledge_base_ids: selected });
      message.success(t('common.saveSuccess', { defaultValue: '已保存' }));
    } catch (e) {
      message.error(e instanceof Error ? e.message : String(e));
    } finally {
      setSaving(false);
    }
  };

  const toggleGrounded = async (checked: boolean) => {
    setGroundedSaving(true);
    try {
      await patch({ grounded_mode: checked });
      message.success(
        checked
          ? t('publicCompanion.knowledge.groundedOnOk', { defaultValue: '已开启严格模式' })
          : t('publicCompanion.knowledge.groundedOffOk', { defaultValue: '已关闭严格模式' })
      );
    } catch (e) {
      message.error(e instanceof Error ? e.message : String(e));
    } finally {
      setGroundedSaving(false);
    }
  };

  return (
    <div className='flex flex-col gap-16px'>
      <SectionCard
        icon={<BookOne theme='outline' size='16' fill='currentColor' className='block' style={{ lineHeight: 0 }} />}
        title={t('publicCompanion.knowledge.title', { defaultValue: '知识库' })}
        desc={t('publicCompanion.knowledge.desc', {
          defaultValue: '选择对外伙伴可检索的平台知识库；它只会引用这些知识库中的内容回答陌生用户。',
        })}
        action={
          <Button type='primary' size='small' loading={saving} disabled={!dirty} onClick={() => void saveBases()}>
            {t('common.save', { defaultValue: '保存' })}
          </Button>
        }
      >
        {loadingBases ? (
          <div className='flex justify-center py-24px'>
            <Spin />
          </div>
        ) : (
          <Select
            mode='multiple'
            allowClear
            value={selected}
            onChange={(v: string[]) => setSelected(v)}
            placeholder={t('publicCompanion.knowledge.selectPlaceholder', { defaultValue: '选择公开知识库（可多选）' })}
            style={{ width: '100%' }}
            notFoundContent={
              <span className='text-12px text-t-tertiary'>
                {t('publicCompanion.knowledge.noBases', { defaultValue: '暂无知识库，请先在「知识库」中创建。' })}
              </span>
            }
          >
            {bases.map((b) => (
              <Select.Option key={b.id} value={b.id}>
                {b.name}
              </Select.Option>
            ))}
          </Select>
        )}
      </SectionCard>

      {/* Grounded mode */}
      <SectionCard
        icon={<SafeRetrieval theme='outline' size='16' fill='currentColor' className='block' style={{ lineHeight: 0 }} />}
        title={t('publicCompanion.knowledge.groundedTitle', { defaultValue: '严格模式' })}
        desc={t('publicCompanion.knowledge.groundedHint', {
          defaultValue: '开启后，对外伙伴只会依据所绑定知识库中的内容作答；找不到依据时会明确说明，而不会自由发挥。适合合规要求高的对外场景。',
        })}
        action={
          <Switch checked={agent.grounded_mode} loading={groundedSaving} onChange={(c) => void toggleGrounded(c)} />
        }
      >
        <div className='flex items-center gap-8px text-12px'>
          <span
            className='inline-flex items-center gap-5px rd-full px-9px py-2px font-600 leading-none'
            style={
              agent.grounded_mode
                ? { color: 'rgb(var(--primary-6))', background: 'rgba(var(--primary-6),0.10)' }
                : { color: 'var(--color-text-3)', background: 'var(--color-fill-2)' }
            }
          >
            {agent.grounded_mode
              ? t('publicCompanion.knowledge.groundedStateOn', { defaultValue: '只答知识库内内容' })
              : t('publicCompanion.knowledge.groundedStateOff', { defaultValue: '允许结合通用知识作答' })}
          </span>
        </div>
      </SectionCard>
    </div>
  );
};

export default KnowledgeSection;
