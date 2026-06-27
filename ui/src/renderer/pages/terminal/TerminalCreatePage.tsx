/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import React, { useEffect, useMemo, useState } from 'react';
import { useLocation, useNavigate } from 'react-router-dom';
import { Button, Input, Message, Radio, Select } from '@arco-design/web-react';
import { useTranslation } from 'react-i18next';
import { ipcBridge } from '@/common';
import type { IIdmmConfig, IKnowledgeBase } from '@/common/adapter/ipcBridge';
import { emitter } from '@/renderer/utils/emitter';
import { WorkspaceFolderSelect } from '@/renderer/components/workspace';
import { defaultIdmmConfig } from '@/renderer/pages/conversation/components/IdmmControl';
import type { AutoWorkDraftValue } from '@/renderer/pages/conversation/components/AutoWorkControl';
import {
  buildLaunchCommand,
  formatCommandPreview,
  getPreset,
  parseCommandPreview,
  TERMINAL_PRESETS,
  type PermissionLevel,
  type TerminalPresetId,
} from './launchPresets';
import ExtendedCapabilitiesPanel from './ExtendedCapabilitiesPanel';
import LabelWithTip from './LabelWithTip';
import { isTerminalAutoworkCapable } from './detectFamily';
import { addRecentLaunchCommand, getRecentLaunchCommands } from './recentLaunchCommands';

const TerminalCreatePage: React.FC = () => {
  const navigate = useNavigate();
  const location = useLocation();
  const { t } = useTranslation();
  const [presetId, setPresetId] = useState<TerminalPresetId>('shell');
  const [permission, setPermission] = useState<PermissionLevel>('full-auto');
  const [cwd, setCwd] = useState('');
  const [commandPreview, setCommandPreview] = useState('');
  const [creating, setCreating] = useState(false);
  // Recent custom launch commands (read once on mount; the page unmounts on launch).
  const [recentCommands] = useState<string[]>(() => getRecentLaunchCommands());
  // Optional knowledge bases bound at creation (mounted into {cwd}/.nomi/knowledge/).
  const [knowledgeBases, setKnowledgeBases] = useState<IKnowledgeBase[]>([]);
  const [kbIds, setKbIds] = useState<string[]>([]);
  // Draft IDMM config — applied after session creation and before AutoWork.
  const [idmm, setIdmm] = useState<IIdmmConfig>(defaultIdmmConfig);
  // Draft AutoWork config — applied after session creation (best-effort).
  const [autowork, setAutowork] = useState<AutoWorkDraftValue>({ enabled: false });

  // Preset working directory passed via navigation state (sidebar workpath
  // drawer → "new terminal session"). One-shot per navigation: the effect only
  // re-runs when location.state changes, so it never clobbers a manual pick.
  useEffect(() => {
    const presetCwd = (location.state as { cwd?: string } | null)?.cwd;
    if (typeof presetCwd === 'string' && presetCwd) setCwd(presetCwd);
  }, [location.state]);

  useEffect(() => {
    let cancelled = false;
    void ipcBridge.knowledge.listBases
      .invoke()
      .then((list) => {
        if (!cancelled) setKnowledgeBases(list);
      })
      .catch(() => {
        /* knowledge platform unavailable → hide the picker */
      });
    return () => {
      cancelled = true;
    };
  }, []);

  const preset = useMemo(() => getPreset(presetId), [presetId]);

  // Keep the editable command preview in sync with preset + permission choices.
  useEffect(() => {
    setCommandPreview(formatCommandPreview(buildLaunchCommand(presetId, permission)));
  }, [presetId, permission]);

  const handleLaunch = async () => {
    const { command, args } = parseCommandPreview(commandPreview);
    if (!command) {
      Message.warning(t('terminal.create.commandRequired'));
      return;
    }
    setCreating(true);
    try {
      const session = await ipcBridge.terminal.create.invoke({
        cwd,
        command,
        args,
        backend: preset.backend,
        mode: preset.supportsPermission ? permission : undefined,
        // Defer the PTY spawn until XtermView mounts and sends the first resize
        // with the real fitted size, so a full-screen TUI (claude) draws at the
        // correct dimensions from frame one — no garble-until-you-resize.
        defer_spawn: true,
        knowledge_base_ids: kbIds.length > 0 ? kbIds : undefined,
      });
      // Remember the launched command for quick reuse — only for the custom preset.
      if (presetId === 'shell') addRecentLaunchCommand(commandPreview);
      // Apply smart-decision before AutoWork starts driving requirements.
      if (idmm.fault_watch.enabled || idmm.decision_watch.enabled) {
        try {
          await ipcBridge.idmm.set.invoke({
            kind: 'terminal',
            target_id: session.id,
            ...idmm,
          });
        } catch {
          Message.warning(
            t('terminal.extended.idmmApplyFailed', {
              defaultValue: '终端已创建，但智能决策启用失败，可在终端内重试',
            }),
          );
        }
      }
      // Best-effort: apply AutoWork draft. Capability is resolved from the
      // command/args/backend the same way the backend gate does — so a wrapper
      // (`stepcode claude`) or a bare custom command also qualifies.
      if (autowork.enabled && autowork.tag && isTerminalAutoworkCapable(command, args, preset.backend)) {
        try {
          await ipcBridge.requirements.setAutoWork.invoke({
            kind: 'terminal',
            target_id: session.id,
            enabled: true,
            tag: autowork.tag,
          });
        } catch {
          Message.warning(
            t('terminal.extended.autoworkApplyFailed', {
              defaultValue: '终端已创建，但自动工作启用失败，可在终端内重试',
            }),
          );
        }
      }
      emitter.emit('terminal.list.refresh');
      navigate(`/terminal/${session.id}`);
    } catch (err) {
      Message.error(err instanceof Error ? err.message : String(err));
    } finally {
      setCreating(false);
    }
  };

  return (
    <div className='flex h-full min-h-0 items-start justify-center overflow-y-auto bg-fill-1 p-24px'>
      <div className='w-[min(640px,100%)] rounded-16px bg-fill-0 p-24px shadow-sm'>
        <h2 className='mb-4px text-18px font-semibold text-t-primary'>{t('terminal.create.title')}</h2>
        <p className='mb-20px text-13px text-t-secondary'>{t('terminal.create.subtitle')}</p>

        {/* Workspace path → cd */}
        <label className='mb-6px block text-14px font-medium text-t-primary'>{t('terminal.create.workspace')}</label>
        <div className='mb-16px'>
          <WorkspaceFolderSelect
            value={cwd}
            onChange={setCwd}
            onClear={() => setCwd('')}
            placeholder={t('terminal.create.workspacePlaceholder')}
            recentLabel={t('terminal.create.recent')}
            chooseDifferentLabel={t('terminal.create.chooseFolder')}
          />
        </div>

        {/* Preset */}
        <LabelWithTip label={t('terminal.create.preset')} tip={t('terminal.create.presetHint')} />
        <Select className='mb-16px' value={presetId} onChange={(v) => setPresetId(v as TerminalPresetId)}>
          {TERMINAL_PRESETS.map((p) => (
            <Select.Option key={p.id} value={p.id}>
              {t(p.labelKey)}
            </Select.Option>
          ))}
        </Select>

        {/* Permission mode (agent presets only) */}
        {preset.supportsPermission && (
          <>
            <label className='mb-6px block text-14px font-medium text-t-primary'>
              {t('terminal.create.permission')}
            </label>
            <Radio.Group
              className='mb-16px'
              type='button'
              value={permission}
              onChange={(v) => setPermission(v as PermissionLevel)}
            >
              <Radio value='default'>{t('terminal.create.permissionDefault')}</Radio>
              <Radio value='full-auto'>{t('terminal.create.permissionFullAuto')}</Radio>
            </Radio.Group>
          </>
        )}

        {/* Editable launch command preview */}
        <LabelWithTip label={t('terminal.create.command')} tip={t('terminal.create.commandHint')} />
        <Input className={`font-mono ${presetId === 'shell' && recentCommands.length > 0 ? 'mb-8px' : 'mb-20px'}`} value={commandPreview} onChange={setCommandPreview} placeholder='$SHELL' />

        {/* Recent launch commands — custom preset only; click to fill the command field */}
        {presetId === 'shell' && recentCommands.length > 0 && (
          <div className='mb-20px'>
            <div className='mb-6px text-12px text-t-tertiary'>{t('terminal.create.recentCommands')}</div>
            <div className='flex flex-col gap-2px'>
              {recentCommands.map((cmd) => (
                <button
                  key={cmd}
                  type='button'
                  title={cmd}
                  onClick={() => setCommandPreview(cmd)}
                  className='block w-full cursor-pointer truncate rounded-6px b-none bg-fill-1 px-8px py-4px text-left font-mono text-12px text-t-secondary appearance-none hover:bg-fill-2'
                >
                  {cmd}
                </button>
              ))}
            </div>
          </div>
        )}

        <div className='flex justify-end gap-8px'>
          <Button onClick={() => navigate(-1)}>{t('common.cancel')}</Button>
          <Button type='primary' loading={creating} onClick={handleLaunch}>
            {t('terminal.create.launch')}
          </Button>
        </div>

        {/* Extended capabilities (knowledge mount + connect / AutoWork / advanced MCP) —
            an optional drawer below the primary action; collapsed by default. */}
        <ExtendedCapabilitiesPanel
          cwd={cwd}
          command={commandPreview}
          backend={preset.backend}
          knowledgeBases={knowledgeBases}
          kbIds={kbIds}
          onKbIdsChange={setKbIds}
          idmm={idmm}
          onIdmmChange={setIdmm}
          autowork={autowork}
          onAutoworkChange={setAutowork}
        />
      </div>
    </div>
  );
};

export default TerminalCreatePage;
