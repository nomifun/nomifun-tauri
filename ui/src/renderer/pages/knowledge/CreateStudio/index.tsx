/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

/**
 * CreateStudio — Wide dialog for creating a new knowledge base.
 *
 * Layout: left TypeRail (kind selector) + right panel (SourceConfig + BasicInfo).
 * Footer with low-barrier hint + cancel / submit.
 *
 * C3 wires: basic info (name/desc/AI row/tags), submission per sourceType,
 * feishu inline credential creation in SourceConfig.
 */
import React, { useCallback, useRef, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Button, Message, Modal, Popconfirm, Tooltip } from '@arco-design/web-react';
import { Close } from '@icon-park/react';
import { useLayoutContext } from '@renderer/hooks/context/LayoutContext';
import { ipcBridge } from '@/common';
import type { IKnowledgeBase } from '@/common/adapter/ipcBridge';
import { isAutogenNoProviderError, knowledgeErrorText, notifySourceFetchResult } from '../useKnowledge';
import { useKnowledgeTags } from '../useKnowledgeTags';
import KnowledgeModelSelector, { useKnowledgeAutogenModel } from '../KnowledgeModelSelector';
import type { KnowledgeKind } from '../KnowledgeTagFilterBar';
import SourceConfig from './SourceConfig';
import type { SourceConfigValue, SyncInterval } from './SourceConfig';
import TeachingCard from './TeachingCard';
import TypeRail from './TypeRail';
import TagPicker from './TagPicker';
import {
  canSubmitStudioSourceConfig,
  canSubmitStudioSourceType,
  normalizeStudioInitialKind,
  type StudioSourceType,
} from './sourceTypes';

// ─── Props ───────────────────────────────────────────────────────────────────

export interface CreateStudioProps {
  visible: boolean;
  /** Pre-select a kind when opening (e.g. from empty-state shortcut). */
  initialKind?: KnowledgeKind;
  onClose: () => void;
  /** Called after successful creation with the new base object. */
  onCreated: (base: IKnowledgeBase) => void;
}

// ─── Sync interval → minutes mapping ────────────────────────────────────────

function syncIntervalToMinutes(interval: SyncInterval | undefined): number | undefined {
  switch (interval) {
    case 'hourly': return 60;
    case 'daily': return 1440;
    default: return undefined;
  }
}

const studioFieldClass =
  'knowledge-studio-field rounded-14px bg-[var(--color-bg-2)] p-12px shadow-[inset_0_0_0_1px_rgba(0,0,0,0.035)]';

const studioInputClass =
  'knowledge-studio-input w-full rounded-12px border border-transparent bg-[var(--color-fill-1)] px-13px py-11px text-13px text-[var(--color-text-1)] outline-none font-[inherit] transition-[background-color,border-color,box-shadow,color] placeholder:text-[var(--color-text-4)] hover:bg-[var(--color-fill-2)] focus:border-[rgba(var(--primary-6),0.36)] focus:bg-[var(--color-bg-2)] focus-visible:shadow-[0_0_0_3px_rgba(var(--primary-6),0.12)]';

const studioActionClass =
  'knowledge-studio-ai-action inline-flex items-center gap-4px border-0 bg-transparent p-0 text-12px font-500 leading-20px text-[var(--color-text-2)] appearance-none transition-colors hover:text-[rgb(var(--primary-6))] focus-visible:outline-none focus-visible:text-[rgb(var(--primary-6))]';

// ─── Component ───────────────────────────────────────────────────────────────

const CreateStudio: React.FC<CreateStudioProps> = ({
  visible,
  initialKind,
  onClose,
  onCreated,
}) => {
  const { t } = useTranslation();
  const layoutCtx = useLayoutContext();
  const isMobile = layoutCtx?.isMobile ?? false;

  // ─── State ──────────────────────────────────────────────────────────────────

  const [sourceType, setSourceType] = useState<StudioSourceType>(() => normalizeStudioInitialKind(initialKind));
  const [sourceConfigValue, setSourceConfigValue] = useState<SourceConfigValue>({});

  // Basic info
  const [name, setName] = useState('');
  const [description, setDescription] = useState('');
  const [selectedTags, setSelectedTags] = useState<string[]>([]);

  // AI description
  const { choice: modelChoice, setChoice: setModelChoice } = useKnowledgeAutogenModel();
  const [generateLoading, setGenerateLoading] = useState(false);
  const [polishLoading, setPolishLoading] = useState(false);
  const aiSessionRef = useRef(0);

  // Submission
  const [submitting, setSubmitting] = useState(false);

  // Tags
  const { tags, createTag } = useKnowledgeTags();

  // ─── Reset state when dialog opens ────────────────────────────────────────

  React.useEffect(() => {
    if (visible) {
      setSourceType(normalizeStudioInitialKind(initialKind));
      setSourceConfigValue({});
      setName('');
      setDescription('');
      setSelectedTags([]);
      aiSessionRef.current += 1;
    }
  }, [visible, initialKind]);

  const handleSourceChange = useCallback((val: SourceConfigValue) => {
    setSourceConfigValue(val);
  }, []);

  // ─── AI Description Helpers (migrated from old modal) ─────────────────────

  const aiErrorText = (e: unknown) =>
    isAutogenNoProviderError(e) ? t('knowledge.actions.autogenNoProvider') : knowledgeErrorText(e);

  const runDescriptionAi = async <T extends { description: string }>(
    busy: boolean,
    setBusy: (v: boolean) => void,
    invoke: () => Promise<T>,
  ): Promise<T | undefined> => {
    if (busy) return undefined;
    setBusy(true);
    const session = aiSessionRef.current;
    try {
      const res = await invoke();
      if (session !== aiSessionRef.current) return undefined;
      setDescription(res.description);
      return res;
    } catch (e) {
      Message.error(aiErrorText(e));
      return undefined;
    } finally {
      setBusy(false);
    }
  };

  /** AI generate description from local dir root_path. */
  const handleGenerateFromDir = async () => {
    const rootPath = sourceConfigValue.rootPath?.trim();
    if (!rootPath) return;
    await runDescriptionAi(generateLoading, setGenerateLoading, () =>
      ipcBridge.knowledge.generateDescription.invoke({
        name: name.trim() || undefined,
        root_path: rootPath,
        ...(modelChoice ?? {}),
      }),
    );
  };

  /** AI polish current description draft. */
  const handlePolish = async () => {
    if (!description.trim()) return;
    await runDescriptionAi(polishLoading, setPolishLoading, () =>
      ipcBridge.knowledge.polishDescription.invoke({
        name: name.trim() || undefined,
        draft: description.trim(),
        ...(modelChoice ?? {}),
      }),
    );
  };

  const handleInsertTemplate = () => {
    setDescription(t('knowledge.form.descriptionTemplate'));
  };

  // Determine if AI generate is available (needs rootPath for local type)
  const canGenerate = sourceType === 'local' && Boolean(sourceConfigValue.rootPath?.trim());
  const insertTemplateLabel = t('knowledge.studio.insertTemplate', { defaultValue: '＋ 插入模板' });

  // ─── Submit Logic ─────────────────────────────────────────────────────────

  const handleSubmit = async () => {
    const sourceValidation = canSubmitStudioSourceConfig(sourceType, sourceConfigValue);
    if (!sourceValidation.ok) {
      const defaultValue =
        sourceValidation.messageKey === 'knowledge.studio.localFolderRequired'
          ? '请先选择本地文件夹'
          : '飞书知识空间创建暂不可用';
      Message.warning(t(sourceValidation.messageKey, { defaultValue }));
      if (!canSubmitStudioSourceType(sourceType)) {
        setSourceType('blank');
        setSourceConfigValue({});
      }
      return;
    }

    const trimmedName = name.trim();
    if (!trimmedName) {
      Message.warning(t('knowledge.studio.nameRequired', { defaultValue: '请填写知识库名称' }));
      return;
    }
    setSubmitting(true);
    try {
      const desc = description.trim() || undefined;
      const tagKeys = selectedTags.length > 0 ? selectedTags : undefined;

      if (sourceType === 'import') {
        // Import path: use importBase, then updateBase for tags
        const importPath = sourceConfigValue.importPath?.trim();
        if (!importPath) {
          Message.warning(t('knowledge.studio.importRequired', { defaultValue: '请选择导入文件' }));
          setSubmitting(false);
          return;
        }
        const created = await ipcBridge.knowledge.importBase.invoke({ src_path: importPath });
        // Apply name/desc/tags via updateBase since importBase doesn't accept them.
        // If updateBase fails the base already exists — navigate anyway so the user
        // can fix metadata from the detail page.
        try {
          await ipcBridge.knowledge.updateBase.invoke({
            id: created.id,
            name: trimmedName,
            description: desc,
            tags: tagKeys,
          });
        } catch (updateErr) {
          Message.warning(
            t('knowledge.studio.importMetaPartialFail', {
              defaultValue: '库已导入，但名称/描述保存失败，请在详情页补充',
            }),
          );
        }
        Message.success(t('knowledge.studio.createOk', { defaultValue: '知识库创建成功' }));
        onCreated(created);
        return;
      }

      // Build source for non-blank/non-local/non-import
      let source: {
        kind: string;
        mode: 'live' | 'snapshot';
        entries?: { url: string; title?: string; rendered?: boolean }[];
        credential_ref?: string;
        scope?: Record<string, unknown>;
        sync?: { interval_minutes?: number };
      } | undefined;

      if (sourceType === 'web') {
        const urlMode = sourceConfigValue.urlMode ?? 'snapshot';
        const entries = (sourceConfigValue.urlEntries ?? [])
          .map((e) => ({
            url: e.url.trim(),
            title: e.title?.trim() || undefined,
            rendered: sourceConfigValue.browserRender || undefined,
          }))
          .filter((e) => e.url.length > 0);
        if (entries.length === 0) {
          Message.warning(t('knowledge.studio.webUrlRequired', { defaultValue: '请至少填写一个网址' }));
          setSubmitting(false);
          return;
        }
        // Validate each URL is a well-formed http(s) address before submitting:
        // snapshot mode fails on first fetch for a malformed URL, and live mode
        // would otherwise silently store a dead source.
        const invalidEntry = entries.find((e) => {
          try {
            const u = new URL(e.url);
            return u.protocol !== 'http:' && u.protocol !== 'https:';
          } catch {
            return true;
          }
        });
        if (invalidEntry) {
          Message.warning(
            t('knowledge.studio.webUrlInvalid', {
              defaultValue: '网址格式不正确,需以 http:// 或 https:// 开头:{{url}}',
              url: invalidEntry.url,
            })
          );
          setSubmitting(false);
          return;
        }
        source = { kind: 'url', mode: urlMode, entries };
      } else if (sourceType === 'feishu') {
        if (!sourceConfigValue.credentialId) {
          Message.warning(t('knowledge.studio.feishuCredRequired', { defaultValue: '请选择或创建飞书凭证' }));
          setSubmitting(false);
          return;
        }
        const intervalMinutes = syncIntervalToMinutes(sourceConfigValue.syncInterval);
        source = {
          kind: 'feishu',
          mode: 'snapshot',
          credential_ref: sourceConfigValue.credentialId,
          scope: sourceConfigValue.spaceId ? { space_id: sourceConfigValue.spaceId } : undefined,
          sync: intervalMinutes ? { interval_minutes: intervalMinutes } : undefined,
        };
      }

      const rootPath = sourceType === 'local' ? sourceConfigValue.rootPath?.trim() || undefined : undefined;

      const created = await ipcBridge.knowledge.createBase.invoke({
        name: trimmedName,
        description: desc,
        root_path: rootPath,
        source,
        tags: tagKeys,
      });

      if (created.source_fetch) {
        notifySourceFetchResult(t, created.source_fetch);
      }

      Message.success(t('knowledge.studio.createOk', { defaultValue: '知识库创建成功' }));
      onCreated(created);
    } catch (e) {
      if (e instanceof Error || typeof e === 'string') Message.error(knowledgeErrorText(e));
      else Message.error(String(e));
    } finally {
      setSubmitting(false);
    }
  };

  // ─── Modal width / fullscreen ─────────────────────────────────────────────

  const modalStyle: React.CSSProperties = isMobile
    ? { width: '100vw', maxWidth: '100vw', top: 0, padding: 0, borderRadius: 0 }
    : { width: 1000, maxWidth: '92vw', borderRadius: 16 };

  const modalClassName = isMobile ? 'create-studio-modal--mobile' : '';

  const studioViewportHeight = isMobile
    ? 'calc(100dvh - 48px)'
    : 'min(760px, calc(100dvh - 80px))';

  // ─── Render ───────────────────────────────────────────────────────────────

  return (
    <Modal
      visible={visible}
      onCancel={onClose}
      footer={null}
      title={null}
      closable={false}
      autoFocus={false}
      mountOnEnter
      unmountOnExit
      style={modalStyle}
      className={modalClassName}
      maskClosable
    >
      <div
        className='flex min-h-0 flex-col overflow-hidden'
        style={{ height: studioViewportHeight, maxHeight: studioViewportHeight }}
      >
        {/* ─── Header ──────────────────────────────────────────────────────── */}
        <div className='flex shrink-0 items-start justify-between gap-16px border-b border-b-[var(--color-border)] px-24px pb-16px pt-20px'>
          <div>
            <h2 className='m-0 text-19px font-700 text-[var(--color-text-1)]'>
              {t('knowledge.studio.title', { defaultValue: '新建知识库' })}
            </h2>
            <p className='m-0 mt-4px text-13px text-[var(--color-text-3)]'>
              {t('knowledge.studio.subtitle', { defaultValue: '选择左侧的类型，右侧只显示该类型需要的配置 · 仅「名称」必填' })}
            </p>
          </div>
          <div
            onClick={onClose}
            className='flex size-30px flex-none cursor-pointer items-center justify-center rounded-8px border border-[var(--color-border)] bg-[var(--color-fill-1)] text-[var(--color-text-3)] hover:bg-[var(--color-fill-2)] hover:text-[var(--color-text-1)]'
          >
            <span className='leading-none'><Close theme="outline" size="14" /></span>
          </div>
        </div>

        {/* ─── Body: Rail + Config ─────────────────────────────────────────── */}
        <div
          className={`flex min-h-0 flex-1 ${isMobile ? 'flex-col' : ''}`}
          style={!isMobile ? {
            display: 'grid',
            gridTemplateColumns: '236px minmax(0, 1fr)',
            gridTemplateRows: 'minmax(0, 1fr)',
          } : undefined}
        >
          {/* Left rail */}
          <TypeRail value={sourceType} onChange={setSourceType} />

          {/* Right config area */}
          <div className='knowledge-studio-config-panel min-h-0 flex-1 overflow-y-auto bg-[var(--color-fill-1)] p-22px'>
            {/* ─── Basic Info Section ──────────────────────────────────────── */}
            <div className='knowledge-studio-basic-card mb-14px rounded-16px bg-[var(--color-bg-2)] p-16px shadow-[0_10px_30px_rgba(15,23,42,0.04)]'>
              <div className='mb-14px flex items-start justify-between gap-12px'>
                <div>
                  <div className='text-13px font-700 text-[var(--color-text-1)]'>
                    {t('knowledge.studio.basicInfoTitle', { defaultValue: '基本信息' })}
                  </div>
                  <div className='mt-3px text-12px leading-relaxed text-[var(--color-text-3)]'>
                    {t('knowledge.studio.basicInfoHelp', { defaultValue: '名称用于识别，描述决定模型何时查阅此库。' })}
                  </div>
                </div>
                <span className='shrink-0 rounded-8px bg-[rgba(var(--primary-6),0.08)] px-8px py-4px text-11px font-600 text-[rgb(var(--primary-6))]'>
                  {t('knowledge.studio.requiredBadge', { defaultValue: '名称必填' })}
                </span>
              </div>

              {/* Name (required) */}
              <div className={studioFieldClass}>
                <label className='mb-7px block text-13px font-500 text-[var(--color-text-2)]'>
                  <span className='text-[rgb(var(--warning-6))]'>*</span>{' '}
                  {t('knowledge.studio.nameLabel', { defaultValue: '名称' })}
                </label>
                <input
                  className={studioInputClass}
                  placeholder={t('knowledge.studio.namePlaceholder', { defaultValue: '例如：团队规范、产品 FAQ、领域术语' })}
                  value={name}
                  onChange={(e) => setName(e.target.value)}
                  maxLength={64}
                />
              </div>

              {/* Description */}
              <div className={`${studioFieldClass} mt-10px`}>
                <label className='mb-7px block text-13px font-500 text-[var(--color-text-2)]'>
                  {t('knowledge.studio.descLabel', { defaultValue: '描述' })}
                  <span className='ml-6px font-400 text-[var(--color-text-3)] text-11px'>
                    {t('knowledge.studio.descHint', { defaultValue: '会注入会话提示词，帮 AI 判断何时查阅此库' })}
                  </span>
                </label>
                <textarea
                  className={`${studioInputClass} min-h-82px resize-y`}
                  placeholder={t('knowledge.studio.descPlaceholder', { defaultValue: '这个知识库收录什么、什么场景下该查阅它' })}
                  value={description}
                  onChange={(e) => setDescription(e.target.value)}
                  maxLength={500}
                  rows={2}
                />

                {/* AI action row */}
                <div className='knowledge-studio-ai-actions mt-9px flex flex-wrap items-center gap-8px'>
                  {/* AI Generate (only for local with rootPath) */}
                  <Tooltip disabled={canGenerate} content={t('knowledge.studio.aiGenerateNeedPath', { defaultValue: '需要先选择本地目录' })}>
                    <button
                      type='button'
                      disabled={!canGenerate}
                      className={[
                        studioActionClass,
                        canGenerate
                          ? 'cursor-pointer'
                          : 'cursor-not-allowed opacity-55',
                      ].join(' ')}
                      onClick={canGenerate ? () => void handleGenerateFromDir() : undefined}
                    >
                      {generateLoading ? '...' : t('knowledge.studio.aiGenerate', { defaultValue: 'AI 生成' })}
                    </button>
                  </Tooltip>

                  {/* Insert template */}
                  {description.trim() ? (
                    <Popconfirm
                      title={t('knowledge.form.templateOverwriteConfirm', { defaultValue: '将覆盖当前描述，确认？' })}
                      onOk={handleInsertTemplate}
                    >
                      <button type='button' className={`${studioActionClass} cursor-pointer`}>
                        {insertTemplateLabel}
                      </button>
                    </Popconfirm>
                  ) : (
                    <button
                      type='button'
                      className={`${studioActionClass} cursor-pointer`}
                      onClick={handleInsertTemplate}
                    >
                      {insertTemplateLabel}
                    </button>
                  )}

                  {/* Polish */}
                  {description.trim() && (
                    <button
                      type='button'
                      className={`${studioActionClass} cursor-pointer`}
                      onClick={() => void handlePolish()}
                    >
                      {polishLoading ? '...' : t('knowledge.studio.polishAi', { defaultValue: '美化' })}
                    </button>
                  )}

                  {/* Model selector (pushed right) */}
                  <span className='ml-auto'>
                    <KnowledgeModelSelector choice={modelChoice} onChange={(c) => void setModelChoice(c)} size='mini' />
                  </span>
                </div>
              </div>

              {/* Tags */}
              <div className={`${studioFieldClass} mt-10px`}>
                <label className='mb-7px block text-13px font-500 text-[var(--color-text-2)]'>
                  {t('knowledge.studio.tagLabel', { defaultValue: '标签' })}
                  <span className='ml-6px font-400 text-[var(--color-text-3)] text-11px'>
                    {t('knowledge.studio.tagHint', { defaultValue: '可选，方便分类筛选' })}
                  </span>
                </label>
                <TagPicker
                  value={selectedTags}
                  onChange={setSelectedTags}
                  tags={tags}
                  createTag={(label) => createTag(label)}
                />
              </div>
            </div>

            {/* ─── Source Config (per type) ─────────────────────────────────── */}
            <SourceConfig sourceType={sourceType} value={sourceConfigValue} onChange={handleSourceChange} />
            <TeachingCard sourceType={sourceType} />
          </div>
        </div>

        {/* ─── Footer ──────────────────────────────────────────────────────── */}
        <div className='flex shrink-0 items-center justify-between gap-12px border-t border-t-[var(--color-border)] bg-[var(--color-bg-1)] px-24px py-14px'>
          {/* Left hint */}
          <div className='flex items-center gap-7px text-12px text-[var(--color-text-3)]'>
            <span className='rounded-6px bg-[var(--color-success-light-1)] px-7px py-2px text-10px font-600 text-[rgb(var(--success-6))]'>
              {t('knowledge.studio.lowBarrier', { defaultValue: '低门槛' })}
            </span>
            <span>{t('knowledge.studio.footerHint', { defaultValue: '只有「名称」必填，来源等都能创建后再调整' })}</span>
          </div>

          {/* Actions */}
          <div className='flex gap-10px'>
            <Button
              size='default'
              className='knowledge-studio-footer-action !rounded-10px !border-transparent !bg-[var(--color-fill-1)] !px-16px !text-[var(--color-text-2)] hover:!bg-[var(--color-fill-2)] hover:!text-[var(--color-text-1)]'
              onClick={onClose}
            >
              {t('knowledge.studio.cancel', { defaultValue: '取消' })}
            </Button>
            <Button
              type='primary'
              size='default'
              className='knowledge-studio-footer-action !rounded-10px !border-transparent !px-18px !shadow-[0_8px_20px_rgba(var(--primary-6),0.18)] hover:!shadow-[0_10px_24px_rgba(var(--primary-6),0.22)]'
              loading={submitting}
              onClick={() => void handleSubmit()}
            >
              {t('knowledge.studio.submit', { defaultValue: '创建知识库' })}
            </Button>
          </div>
        </div>
      </div>
    </Modal>
  );
};

export default CreateStudio;
