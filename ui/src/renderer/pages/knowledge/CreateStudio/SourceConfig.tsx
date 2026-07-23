/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

/**
 * SourceConfig — Right-side configuration panel that switches by `sourceType`.
 *
 * Renders the appropriate sub-block for each source kind:
 * - blank: informational note (no config needed)
 * - local: folder path selector
 * - web: snapshot/realtime segment + dynamic URL rows
 * - feishu: credential select + space ID + sync interval
 * - import: zip file selector
 *
 * Controlled: accepts `value` / `onChange` from parent (index.tsx holds state).
 * Theme variables only; no hard-coded semantic colors.
 */
import React, { useCallback, useEffect, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Button, Input, Message, Select, Switch } from '@arco-design/web-react';
import { Close, FolderOpen, Info } from '@icon-park/react';
import { ipcBridge } from '@/common';
import type { IConnectorCredentialSummary } from '@/common/adapter/ipcBridge';
import { isDesktopShell } from '@renderer/utils/platform';
import type { StudioSourceType } from './sourceTypes';
import type { ConnectorCredentialId } from '@/common/types/ids';

// ─── Value Shape ────────────────────────────────────────────────────────────

export type UrlMode = 'snapshot' | 'live';
export type SyncInterval = 'manual' | 'hourly' | 'daily';

export interface UrlEntry {
  url: string;
  title: string;
}

export interface SourceConfigValue {
  /** local */
  rootPath?: string;
  /** web */
  urlMode?: UrlMode;
  urlEntries?: UrlEntry[];
  browserRender?: boolean;
  /** feishu */
  credentialId?: ConnectorCredentialId;
  spaceId?: string;
  syncInterval?: SyncInterval;
  /** import */
  importPath?: string;
}

// ─── Props ──────────────────────────────────────────────────────────────────

export interface SourceConfigProps {
  sourceType: StudioSourceType;
  value: SourceConfigValue;
  onChange: (value: SourceConfigValue) => void;
}

// ─── Max URL entries ────────────────────────────────────────────────────────

const MAX_URLS = 16;

const sourcePanelClass =
  'knowledge-source-panel space-y-16px rounded-16px bg-[var(--color-bg-2)] p-16px shadow-[0_10px_30px_rgba(15,23,42,0.035)]';

const sourceTitleClass = 'text-13px font-700 text-[var(--color-text-1)]';

const sourceLabelClass = 'mb-7px block text-13px font-500 text-[var(--color-text-2)]';

const sourceInputClass =
  'knowledge-source-input rounded-12px border-transparent bg-[var(--color-fill-1)] transition-[background-color,border-color,box-shadow] hover:bg-[var(--color-fill-2)] focus-within:shadow-[0_0_0_3px_rgba(var(--primary-6),0.1)]';

const sourceButtonClass =
  'knowledge-source-button rounded-10px border-transparent bg-[var(--color-fill-1)] text-[var(--color-text-2)] hover:bg-[var(--color-fill-2)] hover:text-[var(--color-text-1)]';

const sourceNoteClass =
  'knowledge-source-note flex gap-10px rounded-12px bg-[var(--color-fill-1)] px-12px py-10px text-12px leading-relaxed text-[var(--color-text-2)] shadow-[inset_0_0_0_1px_rgba(0,0,0,0.035)]';

const segmentGroupClass = 'inline-flex gap-4px rounded-11px bg-[var(--color-fill-1)] p-4px';

const segmentButtonBaseClass =
  'rounded-8px border-none px-13px py-7px text-12px font-inherit cursor-pointer transition-[background-color,color,box-shadow] focus-visible:outline-none focus-visible:shadow-[0_0_0_3px_rgba(var(--primary-6),0.12)]';

const segmentButtonActiveClass = 'bg-[var(--color-bg-2)] font-600 text-[rgb(var(--primary-6))] shadow-[0_2px_8px_rgba(var(--primary-6),0.12)]';

const segmentButtonIdleClass = 'bg-transparent text-[var(--color-text-2)] hover:bg-[var(--color-fill-2)] hover:text-[var(--color-text-1)]';

// ─── Component ──────────────────────────────────────────────────────────────

const SourceConfig: React.FC<SourceConfigProps> = ({ sourceType, value, onChange }) => {
  const { t } = useTranslation();
  const isDesktop = isDesktopShell();

  // ─── Shared updater ───────────────────────────────────────────────────────

  const update = useCallback(
    (patch: Partial<SourceConfigValue>) => {
      onChange({ ...value, ...patch });
    },
    [value, onChange],
  );

  // ─── Blank ────────────────────────────────────────────────────────────────

  if (sourceType === 'blank') {
    return (
      <div className={sourcePanelClass}>
        <div className={sourceTitleClass}>
          {t('knowledge.studio.srcTitleBlank', { defaultValue: '来源 · 空白知识库' })}
        </div>
        <div className={sourceNoteClass}>
          <Info theme='outline' size='14' className='mt-2px flex-none text-[var(--color-text-3)]' />
          <div>
            {t('knowledge.studio.blankNote', {
              defaultValue:
                '系统会自动创建一个由应用托管的 Markdown 目录。无需更多配置 —— 创建后把 .md 文件放进去，或让 AI 自动生成梗概与 README。',
            })}
          </div>
        </div>
      </div>
    );
  }

  // ─── Local ────────────────────────────────────────────────────────────────

  if (sourceType === 'local') {
    const handleBrowseFolder = async () => {
      if (!isDesktop) return;
      try {
        const files = await ipcBridge.dialog.showOpen.invoke({ properties: ['openDirectory'] });
        if (files?.[0]) {
          update({ rootPath: files[0] });
        }
      } catch (e) {
        Message.error(String(e));
      }
    };

    return (
      <div className={sourcePanelClass}>
        <div className={sourceTitleClass}>
          {t('knowledge.studio.srcTitleLocal', { defaultValue: '来源 · 本地文件夹' })}
        </div>
        <div>
          <label className={sourceLabelClass}>
            {t('knowledge.studio.localFolderPath', { defaultValue: '文件夹路径' })}
          </label>
          <div className='flex gap-9px'>
            <Input
              className={`${sourceInputClass} flex-1`}
              placeholder={t('knowledge.studio.localFolderPlaceholder', { defaultValue: '选择电脑上一个已有目录' })}
              value={value.rootPath ?? ''}
              onChange={(v) => update({ rootPath: v })}
              readOnly={isDesktop}
            />
            {isDesktop && (
              <Button className={sourceButtonClass} onClick={() => void handleBrowseFolder()}>
                <FolderOpen theme='outline' size='14' className='mr-4px' />
                {t('knowledge.studio.localBrowse', { defaultValue: '选择文件夹' })}
              </Button>
            )}
          </div>
        </div>
        <div className={sourceNoteClass}>
          <Info theme='outline' size='14' className='mt-2px flex-none text-[var(--color-text-3)]' />
          <div>
            {t('knowledge.studio.localReadonlyNote', {
              defaultValue:
                '应用以只读引用方式接入，绝不改动你的目录结构。目录里 .md 的增删会自动反映到库里。',
            })}
          </div>
        </div>
      </div>
    );
  }

  // ─── Web ──────────────────────────────────────────────────────────────────

  if (sourceType === 'web') {
    const urlMode = value.urlMode ?? 'snapshot';
    const entries = value.urlEntries ?? [{ url: '', title: '' }];

    const setEntries = (newEntries: UrlEntry[]) => {
      update({ urlEntries: newEntries });
    };

    const handleEntryChange = (idx: number, field: 'url' | 'title', val: string) => {
      const next = [...entries];
      next[idx] = { ...next[idx], [field]: val };
      setEntries(next);
    };

    const handleAddEntry = () => {
      if (entries.length >= MAX_URLS) return;
      setEntries([...entries, { url: '', title: '' }]);
    };

    const handleDeleteEntry = (idx: number) => {
      if (entries.length <= 1) return;
      setEntries(entries.filter((_, i) => i !== idx));
    };

    return (
      <div className={sourcePanelClass}>
        <div className={sourceTitleClass}>
          {t('knowledge.studio.srcTitleWeb', { defaultValue: '来源 · 网页 / URL' })}
        </div>

        {/* Crawl mode segment */}
        <div>
          <label className={sourceLabelClass}>
            {t('knowledge.studio.webCrawlMode', { defaultValue: '抓取模式' })}
          </label>
          <div className={segmentGroupClass}>
            <button
              type='button'
              className={`${segmentButtonBaseClass} ${urlMode === 'snapshot' ? segmentButtonActiveClass : segmentButtonIdleClass}`}
              onClick={() => update({ urlMode: 'snapshot' })}
            >
              {t('knowledge.studio.webSnapshot', { defaultValue: '快照（创建时抓取存档）' })}
            </button>
            <button
              type='button'
              className={`${segmentButtonBaseClass} ${urlMode === 'live' ? segmentButtonActiveClass : segmentButtonIdleClass}`}
              onClick={() => update({ urlMode: 'live' })}
            >
              {t('knowledge.studio.webRealtime', { defaultValue: '实时（运行时现查）' })}
            </button>
          </div>
          <div className='mt-6px text-11px text-[var(--color-text-3)]'>
            {t('knowledge.studio.webModeHint', {
              defaultValue:
                '快照：现在就抓取并存为本地文档，之后可随时刷新。实时：不抓取，会话运行时把这些网址作为实时来源查询。',
            })}
          </div>
        </div>

        {/* URL list */}
        <div>
          <label className={sourceLabelClass}>
            {t('knowledge.studio.webUrlList', { defaultValue: '网址列表' })}
            <span className='ml-6px font-400 text-[var(--color-text-3)]'>
              {t('knowledge.studio.webUrlMax', { defaultValue: '（最多 16 条）' })}
            </span>
          </label>
          {entries.map((entry, idx) => (
            <div key={idx} className='mb-8px flex items-center gap-8px'>
              <Input
                className={`${sourceInputClass} flex-1`}
                placeholder='https://example.com/docs'
                value={entry.url}
                onChange={(v) => handleEntryChange(idx, 'url', v)}
              />
              <Input
                className={`${sourceInputClass} w-128px flex-none`}
                placeholder={t('knowledge.studio.webTitleOptional', { defaultValue: '标题（可选）' })}
                value={entry.title}
                onChange={(v) => handleEntryChange(idx, 'title', v)}
              />
              <button
                type='button'
                className='flex size-34px flex-none cursor-pointer items-center justify-center rounded-10px border-none bg-[var(--color-fill-1)] text-[var(--color-text-3)] transition-colors hover:bg-[var(--color-danger-light-1)] hover:text-[rgb(var(--danger-6))] disabled:cursor-not-allowed disabled:opacity-45'
                onClick={() => handleDeleteEntry(idx)}
                disabled={entries.length <= 1}
              >
                <Close theme='outline' size='14' />
              </button>
            </div>
          ))}
          {entries.length < MAX_URLS && (
            <button
              type='button'
              className='w-full cursor-pointer rounded-12px border-none bg-[rgba(var(--primary-6),0.07)] p-10px text-12px font-500 text-[rgb(var(--primary-6))] transition-colors hover:bg-[rgba(var(--primary-6),0.12)] focus-visible:outline-none focus-visible:shadow-[0_0_0_3px_rgba(var(--primary-6),0.12)]'
              onClick={handleAddEntry}
            >
              ＋ {t('knowledge.studio.webAddUrl', { defaultValue: '添加网址' })}
            </button>
          )}
        </div>

        {/* Browser render switch */}
        <div className='flex items-center gap-10px'>
          <Switch
            size='small'
            checked={value.browserRender ?? false}
            onChange={(checked) => update({ browserRender: checked })}
          />
          <span className='text-12px text-[var(--color-text-2)]'>
            {t('knowledge.studio.webBrowserRenderLabel', {
              defaultValue: '用真实浏览器渲染后抓取',
            })}
          </span>
          <span className='text-11px text-[var(--color-text-3)]'>
            {t('knowledge.studio.webBrowserRenderNote', {
              defaultValue: '适合 JS 渲染的单页应用',
            })}
          </span>
        </div>
      </div>
    );
  }

  // ─── Feishu ───────────────────────────────────────────────────────────────

  if (sourceType === 'feishu') {
    return <FeishuConfig value={value} onChange={update} />;
  }

  // ─── Import ───────────────────────────────────────────────────────────────

  if (sourceType === 'import') {
    const handleBrowseZip = async () => {
      if (!isDesktop) return;
      try {
        const files = await ipcBridge.dialog.showOpen.invoke({
          properties: ['openFile'],
          filters: [{ name: 'Zip', extensions: ['zip'] }],
        });
        if (files?.[0]) {
          update({ importPath: files[0] });
        }
      } catch (e) {
        Message.error(String(e));
      }
    };

    return (
      <div className={sourcePanelClass}>
        <div className={sourceTitleClass}>
          {t('knowledge.studio.srcTitleImport', { defaultValue: '来源 · 导入 .zip 包' })}
        </div>
        <div>
          <label className={sourceLabelClass}>
            {t('knowledge.studio.importFile', { defaultValue: '知识库备份包' })}
          </label>
          <div className='flex gap-9px'>
            <Input
              className={`${sourceInputClass} flex-1`}
              placeholder={t('knowledge.studio.importPlaceholder', { defaultValue: '选择一个导出的 .zip 文件' })}
              value={value.importPath ?? ''}
              onChange={(v) => update({ importPath: v })}
              readOnly={isDesktop}
            />
            {isDesktop && (
              <Button className={sourceButtonClass} onClick={() => void handleBrowseZip()}>
                <FolderOpen theme='outline' size='14' className='mr-4px' />
                {t('knowledge.studio.importBrowse', { defaultValue: '选择文件' })}
              </Button>
            )}
          </div>
        </div>
        <div className={sourceNoteClass}>
          <Info theme='outline' size='14' className='mt-2px flex-none text-[var(--color-text-3)]' />
          <div>
            {t('knowledge.studio.importNote', {
              defaultValue:
                '从其它设备 / 库导出的 .zip 还原成一个新的托管库，导入后可继续编辑与挂载。',
            })}
          </div>
        </div>
      </div>
    );
  }

  return null;
};

// ─── Feishu Sub-component ───────────────────────────────────────────────────

interface FeishuConfigInternalProps {
  value: SourceConfigValue;
  onChange: (patch: Partial<SourceConfigValue>) => void;
}

const FeishuConfig: React.FC<FeishuConfigInternalProps> = ({ value, onChange }) => {
  const { t } = useTranslation();
  const [creds, setCreds] = useState<IConnectorCredentialSummary[]>([]);
  const [loading, setLoading] = useState(false);

  // Inline credential creation state
  const [showCreateForm, setShowCreateForm] = useState(false);
  const [credName, setCredName] = useState('');
  const [credAppId, setCredAppId] = useState('');
  const [credAppSecret, setCredAppSecret] = useState('');
  const [credCreating, setCredCreating] = useState(false);

  const syncInterval = value.syncInterval ?? 'manual';

  const refreshCreds = useCallback(async () => {
    setLoading(true);
    try {
      const all = await ipcBridge.knowledge.listCredentials.invoke();
      setCreds(all.filter((c) => c.kind === 'feishu'));
    } catch {
      // Silently fail — the select will be empty
    } finally {
      setLoading(false);
    }
  }, []);

  useEffect(() => {
    void refreshCreds();
  }, [refreshCreds]);

  const handleCreateCredential = async () => {
    const trimName = credName.trim();
    const trimAppId = credAppId.trim();
    const trimAppSecret = credAppSecret.trim();
    if (!trimName || !trimAppId || !trimAppSecret) {
      Message.warning(t('knowledge.studio.feishuCredFormRequired', { defaultValue: '请填写完整的凭证信息' }));
      return;
    }
    setCredCreating(true);
    try {
      const created = await ipcBridge.knowledge.createCredential.invoke({
        kind: 'feishu',
        name: trimName,
        payload: { app_id: trimAppId, app_secret: trimAppSecret },
      });
      // Refresh list and auto-select the new credential
      await refreshCreds();
      onChange({ credentialId: created.credentialId });
      // Reset form
      setShowCreateForm(false);
      setCredName('');
      setCredAppId('');
      setCredAppSecret('');
      Message.success(t('knowledge.studio.feishuCredCreated', { defaultValue: '凭证创建成功' }));
    } catch (e) {
      Message.error(String(e));
    } finally {
      setCredCreating(false);
    }
  };

  const handleCreateNew = () => {
    setShowCreateForm(true);
  };

  return (
    <div className={sourcePanelClass}>
      <div className={sourceTitleClass}>
        {t('knowledge.studio.srcTitleFeishu', { defaultValue: '来源 · 飞书知识空间' })}
      </div>

      {/* Credential select */}
      <div>
        <label className={sourceLabelClass}>
          {t('knowledge.studio.feishuCredential', { defaultValue: '连接凭证' })}
        </label>
        <div className='flex gap-9px'>
          <Select
            className={`${sourceInputClass} flex-1`}
            placeholder={t('knowledge.studio.feishuCredPlaceholder', { defaultValue: '选择一个已保存的飞书应用凭证…' })}
            value={value.credentialId}
            onChange={(v: ConnectorCredentialId | undefined) => onChange({ credentialId: v })}
            loading={loading}
            allowClear
            renderFormat={(_option, val) => {
              const cred = creds.find((c) => c.credentialId === val);
              return cred ? cred.name : '';
            }}
          >
            {creds.map((c) => (
              <Select.Option key={c.credentialId} value={c.credentialId}>
                <div className='flex flex-col py-2px'>
                  <span className='text-13px text-[var(--color-text-1)]'>{c.name}</span>
                  <span className='text-11px text-[var(--color-text-3)]'>
                    {t('knowledge.studio.feishuCredId', { defaultValue: 'ID' })}: {c.credentialId.slice(0, 8)}…
                  </span>
                </div>
              </Select.Option>
            ))}
          </Select>
          <Button className={sourceButtonClass} onClick={handleCreateNew}>
            ＋ {t('knowledge.studio.feishuNewCred', { defaultValue: '新增凭证' })}
          </Button>
        </div>
        <div className='mt-6px text-11px text-[var(--color-text-3)]'>
          {t('knowledge.studio.feishuCredHint', {
            defaultValue: '凭证 = 飞书自建应用的 App ID / Secret，提交时服务端会校验并 AES 加密存储。',
          })}
        </div>

        {/* Inline credential creation form */}
        {showCreateForm && (
          <div className='mt-10px space-y-8px rounded-14px bg-[var(--color-fill-1)] p-12px shadow-[inset_0_0_0_1px_rgba(0,0,0,0.035)]'>
            <div className='text-12px font-600 text-[var(--color-text-2)]'>
              {t('knowledge.studio.feishuNewCredTitle', { defaultValue: '新增飞书凭证' })}
            </div>
            <Input
              size='small'
              className={sourceInputClass}
              placeholder={t('knowledge.studio.feishuCredName', { defaultValue: '凭证名称（如：产品部飞书）' })}
              value={credName}
              onChange={(v) => setCredName(v)}
            />
            <Input
              size='small'
              className={sourceInputClass}
              placeholder='App ID'
              value={credAppId}
              onChange={(v) => setCredAppId(v)}
            />
            <Input.Password
              size='small'
              className={sourceInputClass}
              placeholder='App Secret'
              value={credAppSecret}
              onChange={(v) => setCredAppSecret(v)}
            />
            <div className='flex gap-8px'>
              <Button
                size='small'
                type='primary'
                loading={credCreating}
                onClick={() => void handleCreateCredential()}
              >
                {t('knowledge.studio.feishuCredSubmit', { defaultValue: '验证并保存' })}
              </Button>
              <Button size='small' onClick={() => setShowCreateForm(false)}>
                {t('knowledge.studio.feishuCredCancel', { defaultValue: '取消' })}
              </Button>
            </div>
          </div>
        )}
      </div>

      {/* Space ID */}
      <div>
        <label className={sourceLabelClass}>
          {t('knowledge.studio.feishuSpaceId', { defaultValue: '知识空间 ID' })}
        </label>
        <Input
          className={sourceInputClass}
          placeholder={t('knowledge.studio.feishuSpaceIdPlaceholder', { defaultValue: '飞书 Wiki 空间的 space_id' })}
          value={value.spaceId ?? ''}
          onChange={(v) => onChange({ spaceId: v })}
        />
      </div>

      {/* Sync interval segment */}
      <div>
        <label className={sourceLabelClass}>
          {t('knowledge.studio.feishuSyncFreq', { defaultValue: '同步频率' })}
        </label>
        <div className={segmentGroupClass}>
          {(['manual', 'hourly', 'daily'] as SyncInterval[]).map((interval) => (
            <button
              key={interval}
              type='button'
              className={`${segmentButtonBaseClass} ${syncInterval === interval ? segmentButtonActiveClass : segmentButtonIdleClass}`}
              onClick={() => onChange({ syncInterval: interval })}
            >
              {interval === 'manual' && t('knowledge.studio.feishuManual', { defaultValue: '仅手动' })}
              {interval === 'hourly' && t('knowledge.studio.feishuHourly', { defaultValue: '每小时' })}
              {interval === 'daily' && t('knowledge.studio.feishuDaily', { defaultValue: '每天' })}
            </button>
          ))}
        </div>
      </div>
    </div>
  );
};

export default SourceConfig;
