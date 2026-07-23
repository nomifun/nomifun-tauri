/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import React from 'react';
import { useTranslation } from 'react-i18next';
import { Spin } from '@arco-design/web-react';
import KnowledgeControl from '@/renderer/pages/conversation/components/KnowledgeControl';
import type { useCompanion } from '../useNomi';

interface Props {
  companion: ReturnType<typeof useCompanion>;
}

/**
 * 伙伴「专属知识库」Tab —— 只挂该伙伴的私有知识库（KnowledgeControl kind:'companion'）。
 * 模型配置已迁出本 Tab（见 ChatTab 顶部，唯一事实源 = profile.model）。
 */
const KnowledgeTab: React.FC<Props> = ({ companion }) => {
  const { t } = useTranslation();
  const { profile } = companion;

  if (!profile) {
    return (
      <div className='flex justify-center py-40px'>
        <Spin />
      </div>
    );
  }

  const companionName = profile.name;

  return (
    <div className='flex flex-col gap-10px py-8px'>
      <div className='flex items-start gap-16px bg-fill-2 rd-10px px-14px py-12px'>
        <div className='w-200px shrink-0'>
          <div className='text-14px text-t-primary font-500'>{t('nomi.settings.knowledge')}</div>
          <div className='text-12px text-t-tertiary mt-2px'>{t('nomi.settings.knowledgeHint', { companionName })}</div>
        </div>
        <div className='flex-1 min-w-0'>
          <KnowledgeControl target={{ kind: 'companion', id: profile.companion_id }} />
        </div>
      </div>
    </div>
  );
};

export default KnowledgeTab;
