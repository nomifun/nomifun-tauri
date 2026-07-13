/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

/**
 * Result presentation for a succeeded card:
 *  - image → the primary result inline (extra images fan out as canvas nodes);
 *    a "continue editing" box spawns a downstream card seeded from this result
 *  - video → an inline player + continue-editing box
 *  - text → the generated text (scrollable) + a "turn into text node" action
 */

import React, { useEffect, useMemo, useRef, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { FileText, Return, TransferData } from '@icon-park/react';
import { useWorkshopMedia } from '../canvas/media';
import type { WorkshopGeneratorBatch } from '../types';
import type { GenMode } from './genTypes';
import { loadWorkshopText } from './pipeline';

export interface ResultViewProps {
  mode: GenMode;
  resultAssetIds: string[];
  batch?: WorkshopGeneratorBatch;
  onContinueEdit?: (instruction: string) => void;
  onToTextNode: (content: string) => void;
}

const Spinner: React.FC = () => (
  <span className='h-16px w-16px animate-spin rounded-full border-2 border-solid border-[var(--color-fill-3)] border-t-[rgb(var(--primary-6))]' />
);

const ContinueBox: React.FC<{ onSubmit: (v: string) => void }> = ({ onSubmit }) => {
  const { t } = useTranslation();
  const [draft, setDraft] = useState('');
  const submit = (): void => {
    const v = draft.trim();
    if (!v) return;
    onSubmit(v);
    setDraft('');
  };
  return (
    <div className='flex items-center gap-6px rounded-9px border border-solid border-[var(--color-border-2)] bg-[var(--color-fill-1)] px-8px py-5px focus-within:border-[rgb(var(--primary-6))]'>
      <input
        value={draft}
        onChange={(e) => setDraft(e.target.value)}
        onKeyDown={(e) => {
          e.stopPropagation();
          if (e.key === 'Enter') {
            e.preventDefault();
            submit();
          }
        }}
        placeholder={t('workshopGeneration.result.continuePlaceholder', { defaultValue: '继续编辑：输入指令回车…' })}
        className='nodrag min-w-0 flex-1 border-none bg-transparent text-12px text-[var(--color-text-1)] outline-none placeholder:text-[var(--color-text-3)]'
      />
      <span
        role='button'
        tabIndex={0}
        title={t('workshopGeneration.result.continue', { defaultValue: '继续编辑' })}
        onClick={submit}
        onKeyDown={(e) => (e.key === 'Enter' || e.key === ' ') && submit()}
        className={[
          'nodrag grid h-22px w-22px shrink-0 place-items-center rounded-6px cursor-pointer transition-colors',
          draft.trim()
            ? 'bg-[rgb(var(--primary-6))] text-white hover:opacity-90'
            : 'bg-[var(--color-fill-3)] text-[var(--color-text-3)]',
        ].join(' ')}
      >
        <Return theme='outline' size={13} strokeWidth={3} />
      </span>
    </div>
  );
};

const ResultView: React.FC<ResultViewProps> = ({ mode, resultAssetIds, batch, onContinueEdit, onToTextNode }) => {
  const { t } = useTranslation();

  const primaryId = useMemo(() => {
    if (batch?.primary && resultAssetIds.includes(batch.primary)) return batch.primary;
    return resultAssetIds[0] ?? null;
  }, [batch, resultAssetIds]);

  const media = useWorkshopMedia(mode === 'text' ? null : primaryId);

  const [text, setText] = useState<string | null>(null);
  useEffect(() => {
    if (mode !== 'text' || !primaryId) {
      setText(null);
      return;
    }
    let cancelled = false;
    setText(null);
    void loadWorkshopText(primaryId).then((v) => {
      if (!cancelled) setText(v);
    });
    return () => {
      cancelled = true;
    };
  }, [mode, primaryId]);

  if (resultAssetIds.length === 0) return null;

  if (mode === 'text') {
    return (
      <div className='flex flex-col gap-8px'>
        <div className='max-h-160px overflow-y-auto whitespace-pre-wrap break-words rounded-9px border border-solid border-[var(--color-border-2)] bg-[var(--color-fill-1)] px-10px py-8px text-12px leading-[1.6] text-[var(--color-text-1)] nowheel'>
          {text ?? <span className='text-[var(--color-text-3)]'>{t('workshopGeneration.result.loading', { defaultValue: '加载中…' })}</span>}
        </div>
        {text != null && (
          <div
            role='button'
            tabIndex={0}
            onClick={() => onToTextNode(text)}
            onKeyDown={(e) => (e.key === 'Enter' || e.key === ' ') && onToTextNode(text)}
            className='nodrag inline-flex w-fit items-center gap-5px rounded-7px border border-solid border-[var(--color-border-2)] px-9px py-5px text-11px font-500 text-[var(--color-text-2)] cursor-pointer hover:border-[rgb(var(--primary-6))] hover:text-[rgb(var(--primary-6))] transition-colors'
          >
            <TransferData theme='outline' size={12} strokeWidth={3} />
            {t('workshopGeneration.result.toTextNode', { defaultValue: '转为文本节点' })}
          </div>
        )}
      </div>
    );
  }

  return (
    <div className='flex flex-col gap-8px'>
      <div className='relative overflow-hidden rounded-10px border border-solid border-[var(--color-border-2)] bg-[var(--color-fill-1)]'>
        {mode === 'video' ? (
          media.status === 'ready' ? (
            <video src={media.url} controls playsInline className='nodrag block max-h-200px w-full bg-black object-contain' />
          ) : (
            <div className='flex h-120px items-center justify-center'>
              {media.status === 'error' ? (
                <span className='text-11px text-[rgb(var(--danger-6))]'>
                  {t('workshopGeneration.result.loadFailed', { defaultValue: '加载失败' })}
                </span>
              ) : (
                <Spinner />
              )}
            </div>
          )
        ) : media.status === 'ready' ? (
          <img src={media.url} alt='' draggable={false} className='block max-h-200px w-full select-none object-contain' />
        ) : (
          <div className='flex h-120px items-center justify-center'>
            {media.status === 'error' ? (
              <span className='text-11px text-[rgb(var(--danger-6))]'>
                {t('workshopGeneration.result.loadFailed', { defaultValue: '加载失败' })}
              </span>
            ) : (
              <Spinner />
            )}
          </div>
        )}
        {resultAssetIds.length > 1 && (
          <span className='absolute right-6px top-6px inline-flex items-center gap-3px rounded-full bg-black/55 px-7px py-2px text-10px font-600 text-white backdrop-blur-sm'>
            <FileText theme='outline' size={10} strokeWidth={3} />
            {t('workshopGeneration.result.batch', { count: resultAssetIds.length, defaultValue: '{{count}} 张' })}
          </span>
        )}
      </div>
      {onContinueEdit ? (
        <ContinueBox onSubmit={onContinueEdit} />
      ) : (
        <div className='rounded-8px bg-[var(--color-fill-1)] px-9px py-6px text-11px leading-17px text-[var(--color-text-2)]'>
          {t('workshopGeneration.result.localContinueUnavailable', {
            defaultValue: '本地 Z-Image 当前仅支持文生图，暂不支持继续编辑图片',
          })}
        </div>
      )}
    </div>
  );
};

export default ResultView;
