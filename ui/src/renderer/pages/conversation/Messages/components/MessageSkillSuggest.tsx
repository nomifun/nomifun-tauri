/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import type { ISkillSuggestArtifact } from '@/common/adapter/ipcBridge';
import React from 'react';
import SkillSuggestCard from './SkillSuggestCard';

const MessageSkillSuggest: React.FC<{ artifact: ISkillSuggestArtifact }> = ({ artifact }) => {
  const { cron_job_id, name, description } = artifact.payload;
  const skillContent = artifact.payload.skillContent ?? artifact.payload.skill_content ?? '';

  return (
    <div data-testid='message-skill-suggest' className='max-w-780px w-full mx-auto'>
      <SkillSuggestCard
        conversation_artifact_id={artifact.conversation_artifact_id}
        conversation_id={artifact.conversation_id}
        suggestion={{ name, description, content: skillContent }}
        cron_job_id={cron_job_id}
      />
    </div>
  );
};

export default MessageSkillSuggest;
