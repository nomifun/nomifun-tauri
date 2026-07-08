/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

/**
 * CanvasPage (`/workshop/:id`) — the Creative Workshop editor.
 *
 * Loads the canvas meta + (opaque) doc, hosts the thin top toolbar (back /
 * inline-editable title / autosave pill), and mounts the infinite-canvas editor
 * ({@link CanvasEditor}). The editor owns all interaction + persistence; this
 * shell relays the editor's autosave state to the pill and the title-rename
 * flow to the backend.
 */
import React, { useCallback, useEffect, useRef, useState } from 'react';
import { useNavigate, useParams } from 'react-router-dom';
import { useTranslation } from 'react-i18next';
import { Button, Result, Spin } from '@arco-design/web-react';
import { ArrowLeft, CheckOne, CloseOne, Loading, Platte } from '@icon-park/react';
import { useArcoMessage } from '@renderer/utils/ui/useArcoMessage';
import { getCanvas, patchCanvas } from './api';
import CanvasEditor from './canvas/CanvasEditor';
import type { SaveState } from './canvas/persistence';
import type { WorkshopCanvasDoc, WorkshopCanvasMeta } from './types';

// ─── Save-state pill ──────────────────────────────────────────────────────────

const SaveStatePill: React.FC<{ state: SaveState }> = ({ state }) => {
  const { t } = useTranslation();
  if (state === 'idle') return null;

  const config: Record<Exclude<SaveState, 'idle'>, { icon: React.ReactNode; label: string; className: string }> = {
    saving: {
      icon: <Loading theme='outline' size={13} className='animate-spin' />,
      label: t('workshop.editor.saving', { defaultValue: '保存中...' }),
      className: 'text-[var(--color-text-3)] border-[var(--color-border-2)] bg-[var(--color-fill-2)]',
    },
    saved: {
      icon: <CheckOne theme='outline' size={13} strokeWidth={3} />,
      label: t('workshop.editor.saved', { defaultValue: '已保存' }),
      className: 'text-[rgb(var(--success-6))] border-[rgba(var(--success-6),0.35)] bg-[rgba(var(--success-6),0.08)]',
    },
    error: {
      icon: <CloseOne theme='outline' size={13} strokeWidth={3} />,
      label: t('workshop.editor.saveFailed', { defaultValue: '保存失败' }),
      className: 'text-[rgb(var(--danger-6))] border-[rgba(var(--danger-6),0.35)] bg-[rgba(var(--danger-6),0.08)]',
    },
  };
  const { icon, label, className } = config[state];

  return (
    <span
      className={[
        'inline-flex items-center gap-5px rounded-full border border-solid px-9px py-3px',
        'text-11px font-600 leading-none whitespace-nowrap',
        className,
      ].join(' ')}
    >
      {icon}
      {label}
    </span>
  );
};

// ─── Editor shell ─────────────────────────────────────────────────────────────

const CanvasPage: React.FC = () => {
  const { t } = useTranslation();
  const navigate = useNavigate();
  const { id } = useParams<{ id: string }>();
  const [message, messageHolder] = useArcoMessage();

  const [meta, setMeta] = useState<WorkshopCanvasMeta | null>(null);
  const [doc, setDoc] = useState<WorkshopCanvasDoc | null>(null);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [saveState, setSaveState] = useState<SaveState>('idle');

  const load = useCallback(async () => {
    if (!id) return;
    setLoading(true);
    try {
      const detail = await getCanvas(id);
      setMeta(detail.meta);
      setDoc(detail.doc);
      setError(null);
    } catch (e) {
      console.error('[workshop] failed to load canvas', e);
      setError(e instanceof Error ? e.message : String(e));
      setMeta(null);
      setDoc(null);
    } finally {
      setLoading(false);
    }
  }, [id]);

  useEffect(() => {
    void load();
  }, [load]);

  // ─── Inline title rename ────────────────────────────────────────────────────

  const [editingTitle, setEditingTitle] = useState(false);
  const [titleDraft, setTitleDraft] = useState('');
  const titleInputRef = useRef<HTMLInputElement | null>(null);

  const beginEditTitle = useCallback(() => {
    if (!meta) return;
    setTitleDraft(meta.title);
    setEditingTitle(true);
  }, [meta]);

  useEffect(() => {
    if (editingTitle) titleInputRef.current?.focus();
  }, [editingTitle]);

  const commitTitle = useCallback(async () => {
    if (!meta || !id) {
      setEditingTitle(false);
      return;
    }
    const next = titleDraft.trim();
    setEditingTitle(false);
    if (!next || next === meta.title) return;
    setSaveState('saving');
    try {
      const updated = await patchCanvas(id, { title: next });
      setMeta(updated);
      setSaveState('saved');
    } catch (e) {
      setSaveState('error');
      message.error(
        `${t('workshop.actions.renameFailed', { defaultValue: '重命名失败' })}: ${e instanceof Error ? e.message : String(e)}`
      );
    }
  }, [meta, id, titleDraft, message, t]);

  const goBack = useCallback(() => navigate('/workshop'), [navigate]);

  // ─── Render ─────────────────────────────────────────────────────────────────

  if (loading) {
    return (
      <div className='size-full flex items-center justify-center'>
        <Spin />
      </div>
    );
  }

  if (error || !meta || !doc || !id) {
    return (
      <div className='size-full flex items-center justify-center px-16px'>
        {messageHolder}
        <Result
          status='error'
          title={t('workshop.editor.loadError', { defaultValue: '画布加载失败' })}
          subTitle={error ?? undefined}
          extra={
            <div className='flex items-center justify-center gap-10px'>
              <Button onClick={goBack}>{t('workshop.editor.backToList', { defaultValue: '返回画布列表' })}</Button>
              <Button type='primary' onClick={() => void load()}>
                {t('workshop.editor.retry', { defaultValue: '重试' })}
              </Button>
            </div>
          }
        />
      </div>
    );
  }

  return (
    <div className='size-full flex flex-col overflow-hidden bg-[var(--color-bg-1)]'>
      {messageHolder}

      {/* Top toolbar */}
      <div className='shrink-0 flex items-center gap-12px px-16px h-52px border-b border-solid border-[var(--color-border-2)] border-l-0 border-r-0 border-t-0 bg-[var(--color-bg-2)] z-10'>
        <div
          role='button'
          tabIndex={0}
          title={t('workshop.editor.back', { defaultValue: '返回' })}
          onClick={goBack}
          onKeyDown={(e) => {
            if (e.key === 'Enter' || e.key === ' ') {
              e.preventDefault();
              goBack();
            }
          }}
          className={[
            'grid h-32px w-32px place-items-center rounded-8px shrink-0 cursor-pointer',
            'text-[var(--color-text-2)] hover:bg-[var(--color-fill-2)] hover:text-[var(--color-text-1)]',
            'transition-colors',
          ].join(' ')}
        >
          <ArrowLeft theme='outline' size={18} strokeWidth={3} />
        </div>

        <span
          className='flex items-center justify-center w-28px h-28px rd-8px shrink-0 text-[rgb(var(--primary-6))]'
          style={{ background: 'rgba(var(--primary-6),0.12)' }}
        >
          <Platte theme='outline' size={16} fill='currentColor' className='block' style={{ lineHeight: 0 }} />
        </span>

        {/* Inline-editable title */}
        {editingTitle ? (
          <input
            ref={titleInputRef}
            value={titleDraft}
            maxLength={80}
            onChange={(e) => setTitleDraft(e.target.value)}
            onBlur={() => void commitTitle()}
            onKeyDown={(e) => {
              if (e.key === 'Enter') {
                e.preventDefault();
                void commitTitle();
              } else if (e.key === 'Escape') {
                e.preventDefault();
                setEditingTitle(false);
              }
            }}
            className={[
              'min-w-0 max-w-360px flex-none text-15px font-700 text-[var(--color-text-1)]',
              'bg-transparent border-none outline-none',
              'border-b border-solid !border-b-[rgb(var(--primary-6))] px-1px py-2px',
            ].join(' ')}
          />
        ) : (
          <div
            role='button'
            tabIndex={0}
            title={t('workshop.editor.renameHint', { defaultValue: '点击重命名' })}
            onClick={beginEditTitle}
            onKeyDown={(e) => {
              if (e.key === 'Enter' || e.key === ' ') {
                e.preventDefault();
                beginEditTitle();
              }
            }}
            className={[
              'min-w-0 max-w-360px truncate cursor-text rounded-6px px-6px py-2px',
              'text-15px font-700 text-[var(--color-text-1)]',
              'hover:bg-[var(--color-fill-2)] transition-colors',
            ].join(' ')}
          >
            {meta.title}
          </div>
        )}

        <SaveStatePill state={saveState} />
      </div>

      {/* Infinite canvas */}
      <div className='relative flex-1 min-h-0 overflow-hidden'>
        <CanvasEditor key={id} canvasId={id} initialDoc={doc} onSaveStateChange={setSaveState} />
      </div>
    </div>
  );
};

export default CanvasPage;
