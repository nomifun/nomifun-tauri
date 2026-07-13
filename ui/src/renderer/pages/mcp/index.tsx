/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import React from 'react';
import { useTranslation } from 'react-i18next';
import HubPageShell from '@/renderer/components/layout/HubPageShell';
import ToolsModalContent from '@/renderer/components/settings/SettingsModal/contents/ToolsModalContent';

const McpPage: React.FC = () => {
  const { t } = useTranslation();

  return (
    <HubPageShell
      title={t('settings.mcpHub.title', { defaultValue: 'MCP' })}
      subtitle={t('settings.mcpHub.subtitle', {
        defaultValue: 'Register, import, and manage MCP server configurations.',
      })}
      maxWidthClass='md:max-w-1200px'
    >
      <ToolsModalContent />
    </HubPageShell>
  );
};

export default McpPage;
