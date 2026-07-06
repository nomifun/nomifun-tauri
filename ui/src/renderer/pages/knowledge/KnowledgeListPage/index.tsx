/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

/**
 * KnowledgeListPage — Card grid with two-dimension filters (kind + tag).
 *
 * Consumes KnowledgeCard (B2), KnowledgeTagFilterBar (B3), useKnowledgeBases,
 * useKnowledgeTags (B1). Uses CreateStudio (Phase C) for the create flow.
 * The old Form-based create Modal has been removed; only the edit path (openEdit)
 * retains a simple modal.
 */
import React, { useMemo, useRef, useState } from 'react';
import { useNavigate } from 'react-router-dom';
import { useTranslation } from 'react-i18next';
import {
  Button,
  Checkbox,
  Form,
  Input,
  Message,
  Modal,
  Result,
  Typography,
} from '@arco-design/web-react';
import { Search } from '@icon-park/react';
import { useLayoutContext } from '@renderer/hooks/context/LayoutContext';
import { isDesktopShell } from '@renderer/utils/platform';
import { ipcBridge } from '@/common';
import type { KnowledgeKindShortcut } from '../KnowledgeEmptyState';
import type { IKnowledgeBase, IKnowledgeTag } from '@/common/adapter/ipcBridge';
import {
  knowledgeErrorText,
  useKnowledgeBases,
} from '../useKnowledge';
import { useKnowledgeTags } from '../useKnowledgeTags';
import KnowledgeEmptyState from '../KnowledgeEmptyState';
import KnowledgeCard from '../KnowledgeCard';
import KnowledgeTagFilterBar, { type KnowledgeKind, type KnowledgeSort } from '../KnowledgeTagFilterBar';
import KnowledgeTagManagementModal from '../KnowledgeTagManagementModal';
import CreateStudio from '../CreateStudio';

// ─── Filter pure function ────────────────────────────────────────────────────

/**
 * Pure filter: kind dimension (exact match), tag dimension (OR / union within
 * selected tags), search dimension (name or description substring, case-insensitive).
 * Dimensions are AND-ed together.
 */
export function filterBases(
  bases: IKnowledgeBase[],
  kind: KnowledgeKind | 'all',
  tagKeys: string[],
  q: string
): IKnowledgeBase[] {
  const lq = q.toLowerCase().trim();
  return bases.filter(
    (b) =>
      (kind === 'all' || b.kind === kind) &&
      (tagKeys.length === 0 || tagKeys.some((k) => b.tags.includes(k))) &&
      (!lq || b.name.toLowerCase().includes(lq) || (b.description ?? '').toLowerCase().includes(lq))
  );
}

// ─── Sort comparator ─────────────────────────────────────────────────────────

function sortBases(bases: IKnowledgeBase[], sort: KnowledgeSort): IKnowledgeBase[] {
  const arr = [...bases];
  switch (sort) {
    case 'updated':
      return arr.sort((a, b) => b.updated_at - a.updated_at);
    case 'created':
      return arr.sort((a, b) => b.created_at - a.created_at);
    case 'name':
      return arr.sort((a, b) => a.name.localeCompare(b.name));
    case 'size':
      return arr.sort((a, b) => b.total_size - a.total_size);
  }
}

/** Self-managing checkbox for the imperative delete Modal.confirm. The modal
 * content never re-renders from page state, so this holds its own `checked`
 * state (so it toggles visually) and reports every change via `onChange`. */
const PurgeFilesCheckbox: React.FC<{ label: string; onChange: (v: boolean) => void }> = ({ label, onChange }) => {
  const [checked, setChecked] = useState(false);
  return (
    <Checkbox
      checked={checked}
      onChange={(v) => {
        setChecked(v);
        onChange(v);
      }}
    >
      {label}
    </Checkbox>
  );
};

// ─── Main Component ──────────────────────────────────────────────────────────

const KnowledgeListPage: React.FC = () => {
  const { t } = useTranslation();
  const navigate = useNavigate();
  const layout = useLayoutContext();
  const isMobile = layout?.isMobile ?? false;

  // Data
  const { bases, loading, error, refresh } = useKnowledgeBases();
  const { tags, createTag, updateTag, deleteTag } = useKnowledgeTags();
  const [tagModalVisible, setTagModalVisible] = useState(false);

  // Filter state
  const [kindFilter, setKindFilter] = useState<KnowledgeKind | null>(null);
  const [tagFilter, setTagFilter] = useState<string[]>([]);
  const [searchQuery, setSearchQuery] = useState('');
  const [sort, setSort] = useState<KnowledgeSort>('updated');

  // Compute counts from the full (unfiltered) set
  const kindCounts = useMemo(() => {
    const counts: Record<string, number> = {};
    for (const b of bases) {
      counts[b.kind] = (counts[b.kind] ?? 0) + 1;
    }
    return counts;
  }, [bases]);

  const tagCounts = useMemo(() => {
    const counts: Record<string, number> = {};
    for (const b of bases) {
      for (const tk of b.tags) {
        counts[tk] = (counts[tk] ?? 0) + 1;
      }
    }
    return counts;
  }, [bases]);

  // Tag map for KnowledgeCard
  const tagMap = useMemo(() => {
    const m: Record<string, IKnowledgeTag> = {};
    for (const tag of tags) m[tag.key] = tag;
    return m;
  }, [tags]);

  // Filtered + sorted result
  const displayBases = useMemo(
    () => sortBases(filterBases(bases, kindFilter ?? 'all', tagFilter, searchQuery), sort),
    [bases, kindFilter, tagFilter, searchQuery, sort]
  );

  // ─── CreateStudio state ─────────────────────────────────────────────────────

  const [studioVisible, setStudioVisible] = useState(false);
  const [studioInitialKind, setStudioInitialKind] = useState<KnowledgeKind | undefined>(undefined);

  const openStudio = (initialKind?: KnowledgeKindShortcut) => {
    setStudioInitialKind(initialKind as KnowledgeKind | undefined);
    setStudioVisible(true);
  };

  const handleStudioCreated = (base: unknown) => {
    setStudioVisible(false);
    void refresh();
    // Navigate to the new base detail
    if (base && typeof base === 'object' && 'id' in base) {
      navigate(`/knowledge/${(base as IKnowledgeBase).id}`);
    }
  };

  // ─── Edit Modal (lightweight — only for renaming/describing existing bases) ─

  const [form] = Form.useForm<{ name: string; description?: string }>();
  const [editing, setEditing] = useState<IKnowledgeBase | null>(null);
  const [editModalVisible, setEditModalVisible] = useState(false);
  const [saving, setSaving] = useState(false);

  const openEdit = (base: IKnowledgeBase) => {
    setEditing(base);
    form.resetFields();
    form.setFieldsValue({ name: base.name, description: base.description });
    setEditModalVisible(true);
  };

  const closeEditModal = () => {
    setEditModalVisible(false);
    setEditing(null);
  };

  const handleEditSubmit = async () => {
    try {
      const values = await form.validate();
      if (!editing) return;
      setSaving(true);
      await ipcBridge.knowledge.updateBase.invoke({
        id: editing.id,
        name: values.name,
        description: values.description ?? '',
      });
      Message.success(t('knowledge.actions.saveOk'));
      closeEditModal();
      void refresh();
    } catch (e) {
      if (e instanceof Error || typeof e === 'string') Message.error(knowledgeErrorText(e));
    } finally {
      setSaving(false);
    }
  };

  // ─── Delete ─────────────────────────────────────────────────────────────────

  // The delete confirm uses the imperative Modal.confirm, whose `content` is
  // rendered ONCE and never re-renders from page state — a page-level
  // `useState` checkbox could neither toggle visually nor be read by the
  // already-captured `onOk` closure. A ref carries the choice instead, and the
  // checkbox below manages its own checked state.
  const purgeRef = useRef(false);

  const handleDelete = async (base: IKnowledgeBase) => {
    try {
      await ipcBridge.knowledge.deleteBase.invoke({ id: base.id, purge: base.managed && purgeRef.current });
      Message.success(t('knowledge.actions.deleteOk'));
      purgeRef.current = false;
      void refresh();
    } catch (e) {
      Message.error(String(e));
    }
  };

  const handleCardDelete = (base: IKnowledgeBase, _e: React.MouseEvent) => {
    purgeRef.current = false;
    Modal.confirm({
      title: t('knowledge.actions.deleteConfirm', { defaultValue: '确认删除？' }),
      content: base.managed ? (
        <PurgeFilesCheckbox
          label={t('knowledge.actions.deleteWithFiles', { defaultValue: '同时删除文件' })}
          onChange={(v) => {
            purgeRef.current = v;
          }}
        />
      ) : undefined,
      onOk: () => handleDelete(base),
      onCancel: () => {
        purgeRef.current = false;
      },
    });
  };

  // ─── Import (direct for empty state) ──────────────────────────────────────

  const handleImport = async () => {
    try {
      if (isDesktopShell()) {
        const files = await ipcBridge.dialog.showOpen.invoke({
          properties: ['openFile'],
          filters: [{ name: 'Knowledge Base Archive', extensions: ['zip'] }],
        });
        if (!files?.[0]) return;
        await ipcBridge.knowledge.importBase.invoke({ src_path: files[0] });
      } else {
        Message.info(t('knowledge.empty.importDesktopOnly', { defaultValue: '导入功能仅桌面端可用' }));
        return;
      }
      Message.success(t('knowledge.empty.importOk', { defaultValue: '导入成功' }));
      void refresh();
    } catch (e) {
      Message.error(knowledgeErrorText(e));
    }
  };

  // ─── Tag management modal ─────────────────────────────────────────────────

  const handleManageTags = () => {
    setTagModalVisible(true);
  };

  // ─── Render ─────────────────────────────────────────────────────────────────

  return (
    <div
      className={[
        'size-full box-border overflow-y-auto',
        isMobile ? 'px-16px py-14px' : 'px-12px py-24px md:px-40px md:py-32px',
      ].join(' ')}
    >
      <div className='mx-auto flex w-full max-w-1180px box-border flex-col gap-16px'>
        {/* Header */}
        <div className='flex w-full flex-wrap items-start justify-between gap-x-20px gap-y-10px'>
          <div>
            <h1 className='m-0 text-26px font-bold text-[var(--color-text-1)] tracking-tight'>
              {t('knowledge.title', { defaultValue: '知识库' })}
            </h1>
            <Typography.Paragraph className='!m-0 !mt-6px text-[var(--color-text-3)] text-13px max-w-560px'>
              {t('knowledge.subtitle', { defaultValue: '集中管理你的专属领域知识。任意会话、终端、数字伙伴都能挂载它作为模型的扩展知识来源。' })}
            </Typography.Paragraph>
          </div>
          <div className='flex items-center gap-10px'>
            {/* Search */}
            <div className='flex items-center gap-8px bg-[var(--color-fill-2)] border border-solid border-[var(--color-border-3)] rounded-10px px-12px py-8px w-220px'>
              <Search theme='outline' size={14} className='text-[var(--color-text-3)] flex-none' />
              <input
                className='border-none bg-transparent outline-none text-[var(--color-text-1)] text-13px w-full font-[inherit] placeholder:text-[var(--color-text-3)]'
                placeholder={t('knowledge.searchPlaceholder', { defaultValue: '搜索知识库...' })}
                value={searchQuery}
                onChange={(e) => setSearchQuery(e.target.value)}
              />
            </div>
            {/* Create button */}
            <div
              role='button'
              tabIndex={0}
              onClick={() => openStudio()}
              onKeyDown={(e) => {
                if (e.key === 'Enter' || e.key === ' ') {
                  e.preventDefault();
                  openStudio();
                }
              }}
              className={[
                'inline-flex items-center gap-7px cursor-pointer select-none',
                'rounded-full px-18px py-9px text-14px font-700',
                'border border-solid border-transparent',
                'bg-[rgba(var(--primary-6),0.12)] text-[var(--color-text-1)]',
                'shadow-[0_6px_18px_rgba(var(--primary-6),0.14)]',
                'hover:bg-[rgba(var(--primary-6),0.18)]',
                'focus-visible:border-[rgb(var(--primary-6))] focus-visible:outline-none',
                'transition-all',
              ].join(' ')}
            >
              <span className='text-16px leading-none text-[rgb(var(--primary-6))]'>＋</span>
              {t('knowledge.newBase', { defaultValue: '新建知识库' })}
            </div>
          </div>
        </div>

        {/* Error state */}
        {error ? (
          <Result
            status='error'
            title={t('knowledge.loadError', { defaultValue: '加载失败' })}
            subTitle={error}
            extra={<Button onClick={() => void refresh()}>{t('knowledge.retry', { defaultValue: '重试' })}</Button>}
          />
        ) : bases.length === 0 && !loading ? (
          <KnowledgeEmptyState onCreate={openStudio} onImport={() => void handleImport()} />
        ) : (
          <>
            {/* Two-dimension filter bar */}
            <KnowledgeTagFilterBar
              kindFilter={kindFilter}
              tagFilter={tagFilter}
              onKindChange={setKindFilter}
              onTagChange={setTagFilter}
              kindCounts={kindCounts}
              tagCounts={tagCounts}
              tags={tags}
              onManageTags={handleManageTags}
              sort={sort}
              onSortChange={setSort}
            />

            {/* Card grid */}
            <div className='grid gap-16px' style={{ gridTemplateColumns: 'repeat(auto-fill, minmax(330px, 1fr))' }}>
              {displayBases.map((base) => (
                <KnowledgeCard
                  key={base.id}
                  base={base}
                  tagMap={tagMap}
                  onOpen={(b) => navigate(`/knowledge/${b.id}`)}
                  onEdit={(b) => openEdit(b)}
                  onDelete={handleCardDelete}
                />
              ))}

              {/* Add-new dashed card (always last) */}
              <div
                role='button'
                tabIndex={0}
                onClick={() => openStudio()}
                onKeyDown={(e) => {
                  if (e.key === 'Enter' || e.key === ' ') {
                    e.preventDefault();
                    openStudio();
                  }
                }}
                className={[
                  'flex flex-col items-center justify-center gap-8px cursor-pointer select-none',
                  'min-h-188px rounded-16px',
                  'border border-dashed border-[var(--color-border-3)] bg-transparent',
                  'text-[var(--color-text-3)]',
                  'hover:border-[var(--color-primary-light-3)] hover:text-[rgb(var(--primary-6))] hover:bg-[var(--color-primary-light-1)]',
                  'transition-all duration-150',
                ].join(' ')}
              >
                <div className='w-38px h-38px rounded-full border border-solid border-current grid place-items-center text-20px leading-none'>
                  ＋
                </div>
                <span className='text-13px'>{t('knowledge.newBase', { defaultValue: '新建知识库' })}</span>
              </div>
            </div>

            {/* Empty filter result */}
            {displayBases.length === 0 && bases.length > 0 && (
              <div className='flex flex-col items-center gap-8px py-40px text-[var(--color-text-3)] text-13px'>
                {t('knowledge.filterEmpty', { defaultValue: '没有匹配的知识库' })}
              </div>
            )}
          </>
        )}
      </div>

      {/* ─── CreateStudio (replaces old create Modal) ────────────────────────── */}
      <CreateStudio
        visible={studioVisible}
        initialKind={studioInitialKind}
        onClose={() => setStudioVisible(false)}
        onCreated={handleStudioCreated}
      />

      {/* ─── Edit Modal (lightweight, for existing bases only) ────────────────── */}
      <Modal
        title={t('knowledge.editBase')}
        visible={editModalVisible}
        confirmLoading={saving}
        onOk={() => void handleEditSubmit()}
        onCancel={closeEditModal}
        autoFocus={false}
      >
        <Form form={form} layout='vertical'>
          <Form.Item
            label={t('knowledge.form.name')}
            field='name'
            rules={[{ required: true, message: t('knowledge.form.nameRequired') }]}
          >
            <Input placeholder={t('knowledge.form.namePlaceholder')} maxLength={64} />
          </Form.Item>
          <Form.Item label={t('knowledge.form.description')} field='description'>
            <Input.TextArea
              placeholder={t('knowledge.form.descriptionPlaceholder')}
              autoSize={{ minRows: 2, maxRows: 4 }}
              maxLength={500}
            />
          </Form.Item>
        </Form>
      </Modal>

      {/* ─── Tag Management Modal ─────────────────────────────────────────── */}
      <KnowledgeTagManagementModal
        visible={tagModalVisible}
        onClose={() => setTagModalVisible(false)}
        tags={tags}
        createTag={createTag}
        updateTag={updateTag}
        deleteTag={deleteTag}
      />
    </div>
  );
};

export default KnowledgeListPage;
