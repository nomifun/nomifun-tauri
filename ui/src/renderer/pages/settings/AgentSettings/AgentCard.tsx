/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 * Based on AionUi (https://github.com/iOfficeAI/AionUi)
 */

import React from 'react';
import { Avatar, Button, Switch, Tooltip, Typography } from '@arco-design/web-react';
import { Delete, EditTwo, Help, Robot } from '@icon-park/react';
import { useTranslation } from 'react-i18next';
import { resolveAgentLogo } from '@/renderer/utils/model/agentLogo';
import { resolveExtensionAssetUrl } from '@/renderer/utils/platform';

type DetectedAgent = {
  agent_type: string;
  backend?: string;
  icon?: string;
  name: string;
  custom_agent_id?: string;
  isExtension?: boolean;
  avatar?: string;
};

/** Catalog fields consumed by the 'installable' (not-installed) card variant. */
type InstallableAgentCardData = {
  /** Backend key — used to resolve the built-in logo. */
  backend: string;
  name: string;
  /** Official site for manual installation; when absent the manual button is hidden. */
  website?: string;
};

/** Minimal custom-agent fields consumed by the 'custom' card variant. */
type CustomAgentCardData = {
  id: string;
  name: string;
  /** User-picked emoji or avatar URL (maps to `AgentMetadata.icon`). */
  icon?: string;
  /** Spawn command for the CLI. */
  command?: string;
  /** Launch arguments for the CLI. */
  args?: string[];
  enabled: boolean;
};

type AgentCardProps =
  | {
      type: 'detected';
      agent: DetectedAgent;
      onGoToChat: () => void;
      /** Multi-agent team eligibility (MCP stdio capable). Renders the team toggle when provided. */
      teamCapable?: boolean;
      /** Whether the agent declared MCP capability in its ACP handshake. When false, opting it into a team is risky. */
      mcpDeclared?: boolean;
      /** Toggles the manual team-capable override; absent = no toggle rendered. */
      onToggleTeam?: (supportsTeam: boolean) => void;
      /** Disables the toggle (e.g. while the override request is in flight). */
      teamToggleLoading?: boolean;
      /**
       * Locks the team toggle ON and disables it. Used for the built-in Nomi
       * agent, which the backend hard-whitelists as team-capable
       * (`TEAM_CAPABLE_BACKENDS`) — toggling it off would silently revert, so we
       * surface it as a fixed, non-editable state instead.
       */
      teamLocked?: boolean;
    }
  | {
      type: 'installable';
      agent: InstallableAgentCardData;
      onOneClickInstall?: () => void;
      onManualInstall?: () => void;
      installing?: boolean;
    }
  | {
      type: 'custom';
      agent: CustomAgentCardData;
      onGoToChat: () => void;
      onEdit: () => void;
      onDelete: () => void;
      onToggle: (enabled: boolean) => void;
    };

const AgentCard: React.FC<AgentCardProps> = (props) => {
  const { t } = useTranslation();
  const goToChatButtonClassName = '!w-full !justify-center !rounded-10px !text-12px';

  if (props.type === 'detected') {
    const { agent, onGoToChat, teamCapable, mcpDeclared, onToggleTeam, teamToggleLoading, teamLocked } = props;
    const extensionAvatar = resolveExtensionAssetUrl(agent.isExtension ? agent.avatar : undefined);
    const logo =
      extensionAvatar ||
      resolveAgentLogo({
        icon: agent.icon,
        backend: agent.backend || agent.agent_type,
        custom_agent_id: agent.custom_agent_id,
        isExtension: agent.isExtension,
      });

    // The risk hint is only meaningful before opting in: an agent that hasn't
    // declared MCP capability may fail to spawn its MCP stdio bridge in a team.
    const showTeamRisk = Boolean(onToggleTeam) && !teamCapable && mcpDeclared === false;

    return (
      <div className='flex min-h-[154px] flex-col rounded-12px border border-solid border-[var(--color-border-2)] bg-[var(--color-bg-2)] p-12px transition-colors hover:border-[var(--color-border-3)]'>
        <div className='mb-10px flex justify-center'>
          <Avatar size={40} shape='square' style={{ flexShrink: 0, backgroundColor: 'transparent' }}>
            {logo ? <img src={logo} alt={agent.name} className='h-full w-full object-contain' /> : '🤖'}
          </Avatar>
        </div>

        <div className='mb-10px flex-1 text-center'>
          <Typography.Text className='block text-13px font-medium leading-18px line-clamp-2'>
            {agent.name}
          </Typography.Text>
          <Typography.Text className='mt-4px block text-11px text-t-secondary'>
            {t('settings.agentManagement.installed')}
          </Typography.Text>
        </div>

        {onToggleTeam && (
          <div className='mb-8px flex items-center justify-center gap-6px'>
            <Typography.Text className='text-11px text-t-secondary'>
              {t('settings.agentManagement.allowTeam')}
            </Typography.Text>
            {showTeamRisk && (
              <Tooltip content={t('settings.agentManagement.teamRiskHint')} position='top'>
                <span className='inline-flex shrink-0 cursor-help'>
                  <Help theme='outline' size='12' className='text-warning-6' />
                </span>
              </Tooltip>
            )}
            <Switch
              size='small'
              checked={teamLocked || Boolean(teamCapable)}
              disabled={teamLocked}
              loading={teamToggleLoading}
              onChange={(v) => onToggleTeam(v)}
            />
          </div>
        )}

        <Button size='small' type='secondary' onClick={onGoToChat} className={goToChatButtonClassName}>
          {t('settings.agentManagement.goToChat')}
        </Button>
      </div>
    );
  }

  if (props.type === 'installable') {
    const { agent, onOneClickInstall, onManualInstall, installing } = props;
    const logo = resolveAgentLogo({ backend: agent.backend });
    const hasOneClickInstall = Boolean(onOneClickInstall);

    return (
      <div className='flex min-h-[154px] flex-col rounded-12px border border-dashed border-[var(--color-border-2)] bg-[var(--color-bg-2)] p-12px transition-colors hover:border-[var(--color-border-3)]'>
        <div className='mb-10px flex justify-center'>
          <Avatar size={40} shape='square' style={{ flexShrink: 0, backgroundColor: 'transparent' }}>
            {logo ? <img src={logo} alt={agent.name} className='h-full w-full object-contain opacity-70' /> : '🤖'}
          </Avatar>
        </div>

        <div className='mb-10px flex-1 text-center'>
          <Typography.Text className='block text-13px font-medium leading-18px line-clamp-2'>
            {agent.name}
          </Typography.Text>
          <Typography.Text className='mt-4px block text-11px text-t-tertiary'>
            {t('settings.agentManagement.notInstalled')}
          </Typography.Text>
        </div>

        <div className='flex flex-col gap-6px'>
          {onOneClickInstall && (
            <Button
              size='small'
              type='primary'
              loading={installing}
              onClick={onOneClickInstall}
              className='!w-full !justify-center !rounded-10px !text-12px'
            >
              {t('settings.agentManagement.oneClickInstall')}
            </Button>
          )}
          {onManualInstall && (
            <Button
              size='small'
              type={hasOneClickInstall ? 'text' : 'primary'}
              onClick={onManualInstall}
              className='!w-full !justify-center !rounded-10px !text-12px'
            >
              {t('settings.agentManagement.manualInstall')}
            </Button>
          )}
        </div>
      </div>
    );
  }

  const { agent, onGoToChat, onEdit, onDelete, onToggle } = props;

  return (
    <div className='flex items-center justify-between px-16px py-10px rd-8px bg-aou-1 hover:bg-aou-2'>
      <div className='flex items-center gap-12px min-w-0 flex-1'>
        <Avatar
          size={32}
          shape='square'
          style={{ flexShrink: 0, backgroundColor: agent.icon ? 'var(--color-fill-2)' : 'transparent', fontSize: 18 }}
        >
          {agent.icon || <Robot theme='outline' size='20' />}
        </Avatar>
        <div className='min-w-0 flex-1'>
          <Typography.Text className='block truncate font-medium text-14px'>{agent.name || 'Custom Agent'}</Typography.Text>
          <div className='text-12px text-t-secondary truncate'>
            {agent.command}
            {agent.args && agent.args.length > 0 ? ` ${agent.args.join(' ')}` : ''}
          </div>
        </div>
      </div>
      <div className='flex shrink-0 items-center gap-8px'>
        <Switch size='small' checked={agent.enabled !== false} onChange={onToggle} />
        <Button size='small' type='text' onClick={onGoToChat} disabled={agent.enabled === false}>
          {t('settings.agentManagement.goToChat')}
        </Button>
        <Button size='small' type='text' icon={<EditTwo theme='outline' size='14' />} onClick={onEdit} />
        <Button
          size='small'
          type='text'
          status='danger'
          icon={<Delete theme='outline' size='14' />}
          onClick={onDelete}
        />
      </div>
    </div>
  );
};

export default AgentCard;
