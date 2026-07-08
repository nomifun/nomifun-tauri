/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

/**
 * WorkshopListPage (`/workshop`) — the Creative Workshop canvas gallery.
 *
 * Grid of canvas cards (thumbnail / title / node-count / updated-time) with
 * create-and-jump, inline-modal rename, confirm-delete, search filter, and an
 * elegant empty state. Mirrors the knowledge / publicCompanion visual language
 * (rounded surfaces, theme variables, hover-revealed actions). M1 replaces the
 * editor at `/workshop/:id`; this page stays the domain's home.
 */
import React, { useCallback, useEffect, useMemo, useState } from 'react';
import { useNavigate } from 'react-router-dom';
import { useTranslation } from 'react-i18next';
import type { TFunction } from 'i18next';
import { Button, Form, Input, Modal, Result, Spin } from '@arco-design/web-react';
import { Delete, EditTwo, LinkOne, Platte, Plus, Search } from '@icon-park/react';
import { useLayoutContext } from '@renderer/hooks/context/LayoutContext';
import { useArcoMessage } from '@renderer/utils/ui/useArcoMessage';
import { createCanvas, deleteCanvas, listCanvases, patchCanvas, resolveWorkshopUrl } from './api';
import type { WorkshopCanvasMeta } from './types';

// ─── Relative-time formatter (i18n-backed) ───────────────────────────────────

function formatRelativeTime(epochMs: number, t: TFunction): string {
  const diff = Date.now() - epochMs;
  const minutes = Math.floor(diff / 60000);
  const hours = Math.floor(minutes / 60);
  const days = Math.floor(hours / 24);
  if (minutes < 1) return t('workshop.time.justNow', { defaultValue: '刚刚' });
  if (minutes < 60) return t('workshop.time.minutesAgo', { count: minutes, defaultValue: '{{count}} 分钟前' });
  if (hours < 24) return t('workshop.time.hoursAgo', { count: hours, defaultValue: '{{count}} 小时前' });
  if (days === 1) return t('workshop.time.yesterday', { defaultValue: '昨天' });
  if (days < 7) return t('workshop.time.daysAgo', { count: days, defaultValue: '{{count}} 天前' });
  return t('workshop.time.weeksAgo', { defaultValue: '上周' });
}

// ─── Canvas card ──────────────────────────────────────────────────────────────

interface CanvasCardProps {
  canvas: WorkshopCanvasMeta;
  onOpen: (c: WorkshopCanvasMeta) => void;
  onRename: (c: WorkshopCanvasMeta) => void;
  onDelete: (c: WorkshopCanvasMeta) => void;
}

const CanvasCard: React.FC<CanvasCardProps> = ({ canvas, onOpen, onRename, onDelete }) => {
  const { t } = useTranslation();
  const thumb = resolveWorkshopUrl(canvas.thumbnail_url);
  const meta = [
    t('workshop.list.card.nodeCount', { count: canvas.node_count, defaultValue: '{{count}} 个节点' }),
    t('workshop.list.card.updatedAt', {
      time: formatRelativeTime(canvas.updated_at, t),
      defaultValue: '{{time}} 更新',
    }),
  ];

  return (
    <div
      className={[
        'group relative flex flex-col overflow-hidden rounded-16px border border-solid',
        'border-[var(--color-border-2)] bg-[var(--color-bg-2)] box-border cursor-pointer',
        'transition-all duration-160',
        'hover:border-[var(--color-border-3)] hover:shadow-[0_14px_38px_rgba(0,0,0,0.15)] hover:-translate-y-2px',
      ].join(' ')}
      onClick={() => onOpen(canvas)}
    >
      {/* Thumbnail / gradient placeholder */}
      <div
        className='relative w-full overflow-hidden'
        style={{
          aspectRatio: '16 / 10',
          background:
            'linear-gradient(135deg, rgba(var(--primary-5),0.14) 0%, rgba(var(--primary-6),0.28) 100%)',
        }}
      >
        {thumb ? (
          <img
            src={thumb}
            alt={t('workshop.list.card.thumbAlt', { defaultValue: '画布缩略图' })}
            className='absolute inset-0 h-full w-full object-cover'
            loading='lazy'
          />
        ) : (
          <div className='absolute inset-0 grid place-items-center text-[rgba(var(--primary-6),0.55)]'>
            <Platte theme='outline' size={40} fill='currentColor' className='block' style={{ lineHeight: 0 }} />
          </div>
        )}

        {/* Hover actions (top-right) */}
        <div
          className={[
            'absolute top-10px right-10px flex gap-6px',
            'pointer-events-none opacity-0 transition-opacity duration-150',
            'group-hover:pointer-events-auto group-hover:opacity-100',
          ].join(' ')}
          onClick={(e) => e.stopPropagation()}
        >
          {(
            [
              { key: 'open', icon: <LinkOne theme='outline' size={14} strokeWidth={3} />, label: t('workshop.list.card.open', { defaultValue: '打开' }), run: () => onOpen(canvas) },
              { key: 'rename', icon: <EditTwo theme='outline' size={14} strokeWidth={3} />, label: t('workshop.list.card.rename', { defaultValue: '重命名' }), run: () => onRename(canvas) },
              { key: 'delete', icon: <Delete theme='outline' size={14} strokeWidth={3} />, label: t('workshop.list.card.delete', { defaultValue: '删除' }), run: () => onDelete(canvas), danger: true },
            ] satisfies { key: string; icon: React.ReactNode; label: string; run: () => void; danger?: boolean }[]
          ).map((action) => (
            <div
              key={action.key}
              role='button'
              tabIndex={0}
              title={action.label}
              onClick={action.run}
              onKeyDown={(e) => {
                if (e.key === 'Enter' || e.key === ' ') {
                  e.preventDefault();
                  action.run();
                }
              }}
              className={[
                'grid h-28px w-28px place-items-center rounded-8px cursor-pointer',
                'border border-solid border-[var(--color-border-2)]',
                'bg-[var(--color-bg-2)] backdrop-blur-sm',
                action.danger
                  ? 'text-[var(--color-text-3)] hover:!border-[rgba(var(--danger-6),0.4)] hover:!text-[rgb(var(--danger-6))] hover:!bg-[rgba(var(--danger-6),0.08)]'
                  : 'text-[var(--color-text-3)] hover:border-[var(--color-border-3)] hover:text-[var(--color-text-1)] hover:bg-[var(--color-fill-2)]',
                'transition-colors',
              ].join(' ')}
            >
              {action.icon}
            </div>
          ))}
        </div>
      </div>

      {/* Body */}
      <div className='flex flex-col gap-6px p-14px'>
        <div className='truncate text-15px font-700 leading-[1.3] text-[var(--color-text-1)]'>
          {canvas.title}
        </div>
        <div className='flex flex-wrap items-center gap-7px text-12px leading-16px text-[var(--color-text-3)]'>
          {meta.map((item, index) => (
            <React.Fragment key={item}>
              {index > 0 && <i className='h-3px w-3px rounded-full bg-[var(--color-fill-4)]' aria-hidden='true' />}
              <span className='whitespace-nowrap'>{item}</span>
            </React.Fragment>
          ))}
        </div>
      </div>
    </div>
  );
};

// ─── Main page ──────────────────────────────────────────────────────────────

const WorkshopListPage: React.FC = () => {
  const { t } = useTranslation();
  const navigate = useNavigate();
  const layout = useLayoutContext();
  const isMobile = layout?.isMobile ?? false;
  const [message, messageHolder] = useArcoMessage();

  const [canvases, setCanvases] = useState<WorkshopCanvasMeta[]>([]);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [creating, setCreating] = useState(false);
  const [searchQuery, setSearchQuery] = useState('');

  const refresh = useCallback(async () => {
    setLoading(true);
    try {
      setCanvases(await listCanvases());
      setError(null);
    } catch (e) {
      console.error('[workshop] failed to load canvases', e);
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setLoading(false);
    }
  }, []);

  useEffect(() => {
    void refresh();
  }, [refresh]);

  const displayed = useMemo(() => {
    const q = searchQuery.trim().toLowerCase();
    if (!q) return canvases;
    return canvases.filter((c) => c.title.toLowerCase().includes(q));
  }, [canvases, searchQuery]);

  // ─── Create + jump ─────────────────────────────────────────────────────────

  const handleCreate = useCallback(async () => {
    if (creating) return;
    setCreating(true);
    try {
      const created = await createCanvas();
      navigate(`/workshop/${created.id}`);
    } catch (e) {
      message.error(
        `${t('workshop.actions.createFailed', { defaultValue: '创建失败' })}: ${e instanceof Error ? e.message : String(e)}`
      );
    } finally {
      setCreating(false);
    }
  }, [creating, navigate, message, t]);

  // ─── Rename modal ─────────────────────────────────────────────────────────

  const [form] = Form.useForm<{ title: string }>();
  const [renaming, setRenaming] = useState<WorkshopCanvasMeta | null>(null);
  const [savingRename, setSavingRename] = useState(false);

  const openRename = useCallback(
    (c: WorkshopCanvasMeta) => {
      setRenaming(c);
      form.resetFields();
      form.setFieldsValue({ title: c.title });
    },
    [form]
  );

  const submitRename = useCallback(async () => {
    if (!renaming) return;
    try {
      const values = await form.validate();
      setSavingRename(true);
      await patchCanvas(renaming.id, { title: values.title.trim() });
      message.success(t('workshop.actions.renameOk', { defaultValue: '已重命名' }));
      setRenaming(null);
      void refresh();
    } catch (e) {
      // Form validation rejects with a field-error map (no `message`); ignore those.
      if (e instanceof Error) {
        message.error(
          `${t('workshop.actions.renameFailed', { defaultValue: '重命名失败' })}: ${e.message}`
        );
      }
    } finally {
      setSavingRename(false);
    }
  }, [renaming, form, message, t, refresh]);

  // ─── Delete ─────────────────────────────────────────────────────────────────

  const handleDelete = useCallback(
    (c: WorkshopCanvasMeta) => {
      Modal.confirm({
        title: t('workshop.actions.deleteConfirmTitle', { defaultValue: '删除画布' }),
        content: t('workshop.actions.deleteConfirmContent', {
          title: c.title,
          defaultValue: '确定删除「{{title}}」吗？此操作不可撤销。',
        }),
        okButtonProps: { status: 'danger' },
        onOk: async () => {
          try {
            await deleteCanvas(c.id);
            message.success(t('workshop.actions.deleteOk', { defaultValue: '画布已删除' }));
            void refresh();
          } catch (e) {
            message.error(
              `${t('workshop.actions.deleteFailed', { defaultValue: '删除失败' })}: ${e instanceof Error ? e.message : String(e)}`
            );
          }
        },
      });
    },
    [message, t, refresh]
  );

  const openCanvas = useCallback((c: WorkshopCanvasMeta) => navigate(`/workshop/${c.id}`), [navigate]);

  // ─── Render ─────────────────────────────────────────────────────────────────

  return (
    <div
      className={[
        'size-full box-border overflow-y-auto',
        isMobile ? 'px-16px py-14px' : 'px-12px py-24px md:px-40px md:py-32px',
      ].join(' ')}
    >
      {messageHolder}
      <div className='mx-auto flex w-full max-w-1180px box-border flex-col gap-16px'>
        {/* Header */}
        <div className='flex w-full flex-wrap items-start justify-between gap-x-20px gap-y-12px'>
          <div className='flex items-start gap-12px min-w-0'>
            <span
              className='flex items-center justify-center w-40px h-40px rd-11px shrink-0 text-[rgb(var(--primary-6))]'
              style={{
                background: 'linear-gradient(150deg, rgba(var(--primary-5),0.16) 0%, rgba(var(--primary-6),0.26) 100%)',
                border: '1px solid rgba(var(--primary-6),0.22)',
              }}
            >
              <Platte theme='outline' size='22' fill='currentColor' className='block' style={{ lineHeight: 0 }} />
            </span>
            <div className='min-w-0'>
              <h1 className='m-0 mb-3px text-22px font-bold text-[var(--color-text-1)] tracking-tight'>
                {t('workshop.title', { defaultValue: '创意工坊' })}
              </h1>
              <p className='m-0 text-13px text-[var(--color-text-3)] leading-19px max-w-560px'>
                {t('workshop.subtitle', {
                  defaultValue: '在一张无限画布上，用节点与连线组织素材与 AI 生成，图片、视频、文本自由混排创作。',
                })}
              </p>
            </div>
          </div>

          {!error && (canvases.length > 0 || loading) && (
            <div className='flex items-center gap-10px'>
              <div className='flex items-center gap-8px bg-[var(--color-fill-2)] border border-solid border-[var(--color-border-3)] rounded-10px px-12px py-8px w-200px'>
                <Search theme='outline' size={14} className='text-[var(--color-text-3)] flex-none' />
                <input
                  className='border-none bg-transparent outline-none text-[var(--color-text-1)] text-13px w-full font-[inherit] placeholder:text-[var(--color-text-3)]'
                  placeholder={t('workshop.list.searchPlaceholder', { defaultValue: '搜索画布...' })}
                  value={searchQuery}
                  onChange={(e) => setSearchQuery(e.target.value)}
                />
              </div>
              <Button type='primary' loading={creating} className='shrink-0' onClick={() => void handleCreate()}>
                <span className='inline-flex items-center gap-6px'>
                  <Plus theme='outline' size='15' fill='currentColor' className='block' style={{ lineHeight: 0 }} />
                  {t('workshop.list.newCanvas', { defaultValue: '新建画布' })}
                </span>
              </Button>
            </div>
          )}
        </div>

        {/* Body states */}
        {error ? (
          <Result
            status='error'
            title={t('workshop.list.loadError', { defaultValue: '加载失败' })}
            subTitle={error}
            extra={
              <Button onClick={() => void refresh()}>{t('workshop.list.retry', { defaultValue: '重试' })}</Button>
            }
          />
        ) : loading ? (
          <div className='flex justify-center py-56px'>
            <Spin />
          </div>
        ) : canvases.length === 0 ? (
          <div className='flex flex-col items-center gap-14px rd-16px border border-dashed border-[var(--color-border-2)] bg-fill-1 px-20px py-52px text-center'>
            <span
              className='flex items-center justify-center w-56px h-56px rd-16px text-[rgb(var(--primary-6))]'
              style={{
                background: 'linear-gradient(150deg, rgba(var(--primary-5),0.16) 0%, rgba(var(--primary-6),0.28) 100%)',
                border: '1px solid rgba(var(--primary-6),0.22)',
              }}
            >
              <Platte theme='outline' size='28' fill='currentColor' className='block' style={{ lineHeight: 0 }} />
            </span>
            <div className='flex flex-col gap-4px'>
              <span className='text-15px font-600 text-[var(--color-text-1)]'>
                {t('workshop.list.empty.title', { defaultValue: '还没有画布' })}
              </span>
              <span className='text-13px text-[var(--color-text-3)] max-w-[440px]'>
                {t('workshop.list.empty.desc', { defaultValue: '创建一张无限画布，开始你的 AI 视觉创作。' })}
              </span>
            </div>
            <Button type='primary' loading={creating} onClick={() => void handleCreate()}>
              <span className='inline-flex items-center gap-6px'>
                <Plus theme='outline' size='15' fill='currentColor' className='block' style={{ lineHeight: 0 }} />
                {t('workshop.list.createFirst', { defaultValue: '新建第一张画布' })}
              </span>
            </Button>
          </div>
        ) : (
          <>
            <div
              className='grid gap-16px'
              style={{ gridTemplateColumns: 'repeat(auto-fill, minmax(min(300px, 100%), 1fr))' }}
            >
              {displayed.map((canvas) => (
                <CanvasCard
                  key={canvas.id}
                  canvas={canvas}
                  onOpen={openCanvas}
                  onRename={openRename}
                  onDelete={handleDelete}
                />
              ))}

              {/* Add-new dashed card (only when not filtering) */}
              {searchQuery.trim() === '' && (
                <div
                  role='button'
                  tabIndex={0}
                  onClick={() => void handleCreate()}
                  onKeyDown={(e) => {
                    if (e.key === 'Enter' || e.key === ' ') {
                      e.preventDefault();
                      void handleCreate();
                    }
                  }}
                  className={[
                    'flex flex-col items-center justify-center gap-8px cursor-pointer select-none',
                    'rounded-16px border border-dashed border-[var(--color-border-3)] bg-transparent',
                    'text-[var(--color-text-3)]',
                    'hover:border-[var(--color-primary-light-3)] hover:text-[rgb(var(--primary-6))] hover:bg-[var(--color-primary-light-1)]',
                    'transition-all duration-150',
                  ].join(' ')}
                  style={{ aspectRatio: '16 / 12.5' }}
                >
                  <div className='w-38px h-38px rounded-full border border-solid border-current grid place-items-center'>
                    <Plus theme='outline' size='20' fill='currentColor' className='block' style={{ lineHeight: 0 }} />
                  </div>
                  <span className='text-13px'>{t('workshop.list.newCanvas', { defaultValue: '新建画布' })}</span>
                </div>
              )}
            </div>

            {/* Empty filter result */}
            {displayed.length === 0 && (
              <div className='flex flex-col items-center gap-8px py-40px text-[var(--color-text-3)] text-13px'>
                {t('workshop.list.filterEmpty', { defaultValue: '没有匹配的画布' })}
              </div>
            )}
          </>
        )}
      </div>

      {/* Rename modal */}
      <Modal
        title={t('workshop.rename.title', { defaultValue: '重命名画布' })}
        visible={renaming !== null}
        confirmLoading={savingRename}
        onOk={() => void submitRename()}
        onCancel={() => setRenaming(null)}
        autoFocus={false}
        unmountOnExit
      >
        <Form form={form} layout='vertical'>
          <Form.Item
            label={t('workshop.rename.label', { defaultValue: '画布名称' })}
            field='title'
            rules={[{ required: true, message: t('workshop.rename.required', { defaultValue: '请输入画布名称' }) }]}
          >
            <Input
              placeholder={t('workshop.rename.placeholder', { defaultValue: '输入画布名称' })}
              maxLength={80}
              showWordLimit
              onPressEnter={() => void submitRename()}
            />
          </Form.Item>
        </Form>
      </Modal>
    </div>
  );
};

export default WorkshopListPage;
