/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import React from 'react';
import { Spin } from '@arco-design/web-react';
import RemoteConnectSection from './RemoteConnectSection';
import type { useCompanion } from '../useNomi';

interface Props {
  companion: ReturnType<typeof useCompanion>;
}

const RemoteTab: React.FC<Props> = ({ companion }) => {
  const { profile } = companion;

  if (!profile) {
    return (
      <div className='flex justify-center py-40px'>
        <Spin />
      </div>
    );
  }

  return (
    <div className='flex flex-col gap-10px py-8px'>
      {/* 远程连接：IM 渠道按伙伴接待（platform→companionId 反向视图）/ Remote connect: per-companion IM channel greeting */}
      <RemoteConnectSection companionId={profile.companion_id} companionName={profile.name} />
    </div>
  );
};

export default RemoteTab;
