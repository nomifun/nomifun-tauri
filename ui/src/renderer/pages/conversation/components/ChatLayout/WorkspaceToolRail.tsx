/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import type { WorkspaceExtraTab, WorkspaceTab } from '@/renderer/pages/conversation/Workspace/types';
import { Tooltip } from '@arco-design/web-react';
import { Branch, Change, ChartHistogram, FolderOpen } from '@icon-park/react';
import classNames from 'classnames';
import type { TFunction } from 'i18next';
import React from 'react';

export const WORKSPACE_PANEL_TAB_EVENT = 'nomifun-workspace-panel-tab';
export const WORKSPACE_PANEL_META_EVENT = 'nomifun-workspace-panel-meta';

export interface WorkspacePanelTabDetail {
  tab: WorkspaceTab;
  sourceKey?: string;
}

export interface WorkspacePanelMetaDetail {
  sourceKey: string;
  changeCount: number;
}

export function dispatchWorkspacePanelTabEvent(tab: WorkspaceTab, sourceKey?: string) {
  if (typeof window === 'undefined') return;
  window.dispatchEvent(
    new CustomEvent<WorkspacePanelTabDetail>(WORKSPACE_PANEL_TAB_EVENT, { detail: { tab, sourceKey } })
  );
}

export function dispatchWorkspacePanelMetaEvent(detail: WorkspacePanelMetaDetail) {
  if (typeof window === 'undefined') return;
  window.dispatchEvent(new CustomEvent<WorkspacePanelMetaDetail>(WORKSPACE_PANEL_META_EVENT, { detail }));
}

export type WorkspaceToolRailOrchestration = {
  active: boolean;
  available: boolean;
  statusColor?: string;
  onClick: () => void;
};

type WorkspaceToolRailProps = {
  t: TFunction;
  activeTab: WorkspaceTab;
  expanded: boolean;
  onSelect: (tab: WorkspaceTab) => void;
  changeCount?: number;
  extraTabs?: WorkspaceExtraTab[];
  orchestration?: WorkspaceToolRailOrchestration;
  footer?: React.ReactNode;
};

type ToolRailItemProps = {
  active: boolean;
  label: React.ReactNode;
  icon: React.ReactNode;
  badge?: React.ReactNode;
  statusColor?: string;
  onClick: () => void;
};

const ToolRailItem: React.FC<ToolRailItemProps> = ({ active, label, icon, badge, statusColor, onClick }) => (
  <Tooltip position='left' content={label} mini className='workspace-tool-rail__tooltip'>
    <button
      type='button'
      className={classNames('workspace-tool-rail__item', {
        'workspace-tool-rail__item--active': active,
      })}
      aria-pressed={active}
      onClick={onClick}
    >
      <span className='workspace-tool-rail__icon'>{icon}</span>
      <span className='workspace-tool-rail__label'>{label}</span>
      {statusColor && <span className='workspace-tool-rail__status' style={{ background: statusColor }} />}
      {badge}
    </button>
  </Tooltip>
);

const WorkspaceToolRail: React.FC<WorkspaceToolRailProps> = ({
  t,
  activeTab,
  expanded,
  onSelect,
  changeCount = 0,
  extraTabs,
  orchestration,
  footer,
}) => (
  <aside
    className='workspace-tool-rail'
    aria-label={t('conversation.workspace.toolsLabel', { defaultValue: '侧边工具' })}
  >
    <ToolRailItem
      active={expanded && activeTab === 'files'}
      label={t('conversation.workspace.changes.filesTab')}
      icon={<FolderOpen size={18} />}
      onClick={() => onSelect('files')}
    />
    <ToolRailItem
      active={expanded && activeTab === 'changes'}
      label={t('conversation.workspace.changes.tab')}
      icon={<Change size={18} />}
      badge={
        changeCount > 0 ? (
          <span className='workspace-tool-rail__badge'>{changeCount > 99 ? '99+' : changeCount}</span>
        ) : undefined
      }
      onClick={() => onSelect('changes')}
    />
    {extraTabs?.map((tab) => (
      <ToolRailItem
        key={tab.key}
        active={expanded && activeTab === tab.key}
        label={tab.title}
        icon={tab.icon ?? <ChartHistogram size={18} />}
        onClick={() => onSelect(tab.key)}
      />
    ))}
    {orchestration?.available && (
      <>
        <span className='workspace-tool-rail__divider' />
        <ToolRailItem
          active={orchestration.active}
          label={t('conversation.orchestration.panelTitle', { defaultValue: '编排画布' })}
          icon={<Branch size={18} />}
          statusColor={orchestration.statusColor}
          onClick={orchestration.onClick}
        />
      </>
    )}
    {footer && <div className='workspace-tool-rail__footer'>{footer}</div>}
  </aside>
);

export default WorkspaceToolRail;
