/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import React, { useState } from 'react';
import { Select } from '@arco-design/web-react';
import { IconDown } from '@arco-design/web-react/icon';
import { useTranslation } from 'react-i18next';
import type { IIdmmConfig, IKnowledgeBase } from '@/common/adapter/ipcBridge';
import AutoWorkControl from '@/renderer/pages/conversation/components/AutoWorkControl';
import type { AutoWorkDraftValue } from '@/renderer/pages/conversation/components/AutoWorkControl';
import IdmmControl from '@/renderer/pages/conversation/components/IdmmControl';
import PlatformMcpRegisterPanel from './PlatformMcpRegisterPanel';
import RegisterKnowledgeButton from './RegisterKnowledgeButton';
import { isTerminalAutoworkCapable } from './detectFamily';

interface ExtendedCapabilitiesPanelProps {
  cwd: string;
  command: string;
  backend?: string;
  /** Knowledge bases available to mount (empty → knowledge platform unavailable). */
  knowledgeBases: IKnowledgeBase[];
  /** Currently-selected (mounted) knowledge base ids. */
  kbIds: string[];
  onKbIdsChange: (ids: string[]) => void;
  idmm: IIdmmConfig;
  onIdmmChange: (next: IIdmmConfig) => void;
  autowork: AutoWorkDraftValue;
  onAutoworkChange: (next: AutoWorkDraftValue) => void;
}

/**
 * Unified "Extended Capabilities" section on the terminal create page — the
 * single home for the platform's signature terminal superpowers (knowledge
 * mount, IDMM, AutoWork, and secret-free external CLI registration).
 */
const ExtendedCapabilitiesPanel: React.FC<ExtendedCapabilitiesPanelProps> = ({
  cwd,
  command,
  backend,
  knowledgeBases,
  kbIds,
  onKbIdsChange,
  idmm,
  onIdmmChange,
  autowork,
  onAutoworkChange,
}) => {
  const { t } = useTranslation();
  const [expanded, setExpanded] = useState(false);

  // Resolve AutoWork capability from the launch command + declared backend the
  // SAME way the backend gate does — so a wrapper (`stepcode claude`) or a bare
  // custom command typed into the shell preset also qualifies, not just the
  // claude/codex presets. `command` here is the full editable preview string;
  // detectFamily tokenizes it, so passing it with empty args is correct.
  const autoworkDisabledReason = isTerminalAutoworkCapable(command, [], backend)
    ? undefined
    : t('terminal.extended.autoworkRequiresAgent', { defaultValue: '自动工作需要 Claude / Codex 终端（含 stepcode claude 等代理形式）' });

  const hasKnowledge = knowledgeBases.length > 0;

  return (
    <div className='mt-20px rounded-12px b-1 b-solid b-color-border-2 bg-fill-1'>
      {/* Collapsible header — this is an optional drawer, collapsed by default */}
      <button
        type='button'
        aria-expanded={expanded}
        onClick={() => setExpanded((e) => !e)}
        className='flex w-full cursor-pointer appearance-none items-center justify-between gap-12px b-none bg-transparent px-16px py-12px text-left'
      >
        <div className='min-w-0'>
          <div className='text-14px font-semibold text-t-primary'>
            {t('terminal.extended.title', { defaultValue: '扩展能力' })}
          </div>
          <div className='mt-2px text-12px text-t-tertiary'>
            {t('terminal.extended.subtitle', { defaultValue: '把平台能力开放给该终端启动的 Agent CLI' })}
          </div>
        </div>
        <IconDown
          className={`shrink-0 text-14px text-t-tertiary transition-transform ${expanded ? 'rotate-180' : ''}`}
        />
      </button>

      {expanded && (
        <div className='px-16px pb-16px'>
          {/* 平台知识库 — mounted into the platform-managed terminal session. */}
          {hasKnowledge && (
            <div className='rounded-8px bg-fill-0 px-12px py-10px'>
              <div className='text-13px font-medium text-t-primary'>
                {t('terminal.extended.knowledgeLabel', { defaultValue: '平台知识库' })}
              </div>
              <div className='mt-2px text-12px leading-16px text-t-tertiary'>
                {t('terminal.extended.knowledgeDesc', {
                  defaultValue: '挂载知识库到工作路径，供该终端的 Agent 检索。',
                })}
              </div>

              <Select
                className='mt-8px w-full'
                mode='multiple'
                allowClear
                placeholder={t('terminal.create.knowledgePlaceholder')}
                value={kbIds}
                maxTagCount={3}
                options={knowledgeBases.map((b) => ({ label: b.name, value: b.knowledge_base_id }))}
                onChange={(v) => onKbIdsChange(v as string[])}
              />
              <div className='mt-8px flex items-start justify-between gap-12px'>
                <div className='min-w-0 flex-1 text-12px leading-16px text-t-tertiary'>
                  {t('terminal.extended.knowledgeConnectNote', {
                    defaultValue:
                      '平台终端会自动注入；包装命令、自定义或外置终端可把无密钥命令注册到工作路径。',
                  })}
                </div>
                <div className='shrink-0'>
                  <RegisterKnowledgeButton cwd={cwd} command={command} />
                </div>
              </div>
              <div className='mt-8px'>
                <PlatformMcpRegisterPanel />
              </div>
            </div>
          )}

          {/* 智能决策 */}
          <div
            className={`${hasKnowledge ? 'mt-8px ' : ''}flex items-center justify-between gap-12px rounded-8px bg-fill-0 px-12px py-10px`}
          >
            <div className='min-w-0 flex-1'>
              <div className='text-13px font-medium text-t-primary'>{t('idmm.label')}</div>
              <div className='mt-2px text-12px leading-16px text-t-tertiary'>{t('idmm.hint')}</div>
            </div>
            <div className='shrink-0'>
              <IdmmControl
                draft={{ value: idmm, onChange: onIdmmChange }}
                applyNote={t('terminal.extended.idmmApplyNote', {
                  defaultValue: '创建终端后立即生效，可在终端内继续调整。',
                })}
              />
            </div>
          </div>

          {/* 自动工作 */}
          <div
            className='mt-8px flex items-center justify-between gap-12px rounded-8px bg-fill-0 px-12px py-10px'
          >
            <div className='min-w-0 flex-1'>
              <div className='text-13px font-medium text-t-primary'>
                {t('terminal.extended.autoworkLabel', { defaultValue: '自动工作' })}
              </div>
              <div className='mt-2px text-12px leading-16px text-t-tertiary'>
                {t('terminal.extended.autoworkDesc', {
                  defaultValue: '让自动任务按需求标签驱动该终端并确认完成（仅 Claude / Codex）',
                })}
              </div>
            </div>
            <div className='shrink-0'>
              <AutoWorkControl
                draft={{ value: autowork, onChange: onAutoworkChange }}
                disabledReason={autoworkDisabledReason}
              />
            </div>
          </div>
        </div>
      )}
    </div>
  );
};

export default ExtendedCapabilitiesPanel;
