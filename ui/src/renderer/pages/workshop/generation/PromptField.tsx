/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

/**
 * Prompt textarea with `@`-mention support. Typing `@` opens a candidate
 * overlay drawing from two sources — canvas resource nodes (auto-numbered
 * 图1/视频2/文1 …) and library assets (searched live) — and selecting one
 * inserts an inline `@label` token into the prompt while recording the stable
 * ref in `data.mentions`.
 */

import React, { useEffect, useMemo, useRef, useState } from 'react';
import { useReactFlow } from '@xyflow/react';
import { useTranslation } from 'react-i18next';
import { AtSign, FileText, Pic, VideoTwo } from '@icon-park/react';
import { listAssets } from '../api';
import type { WorkshopFlowEdge, WorkshopFlowNode } from '../canvas/model';
import type { WorkshopAssetKind } from '../types';
import type { GenMode, MentionCandidate } from './genTypes';
import { collectNodeCandidates, mentionRefForAsset } from './pipeline';
import Floating from './Floating';

export interface PromptFieldProps {
  value: string;
  mode: GenMode;
  selfId: string;
  onChange: (text: string) => void;
  onAddMention: (ref: string) => void;
}

const KIND_ICON: Record<WorkshopAssetKind, React.ReactNode> = {
  image: <Pic theme='outline' size={13} strokeWidth={3} />,
  video: <VideoTwo theme='outline' size={13} strokeWidth={3} />,
  text: <FileText theme='outline' size={13} strokeWidth={3} />,
};

const MENTION_TOKEN = /(^|\s)@([^\s@]*)$/;

const PromptField: React.FC<PromptFieldProps> = ({ value, mode, selfId, onChange, onAddMention }) => {
  const { t } = useTranslation();
  const rf = useReactFlow<WorkshopFlowNode, WorkshopFlowEdge>();
  const taRef = useRef<HTMLTextAreaElement | null>(null);

  const [open, setOpen] = useState(false);
  const [query, setQuery] = useState('');
  const [tokenStart, setTokenStart] = useState(0);
  const [assetCands, setAssetCands] = useState<MentionCandidate[]>([]);
  const [active, setActive] = useState(0);
  const [rect, setRect] = useState<DOMRect | null>(null);

  // Node candidates are cheap; recompute when the overlay opens.
  const nodeCands = useMemo<MentionCandidate[]>(
    () => (open ? collectNodeCandidates(rf.getNodes(), selfId) : []),
    [open, rf, selfId]
  );

  const candidates = useMemo<MentionCandidate[]>(() => {
    const q = query.trim().toLowerCase();
    const nodes = q ? nodeCands.filter((c) => c.label.toLowerCase().includes(q)) : nodeCands;
    return [...nodes, ...assetCands];
  }, [nodeCands, assetCands, query]);

  // Fetch library-asset candidates (debounced) whenever the query changes.
  useEffect(() => {
    if (!open) return;
    let cancelled = false;
    const handle = window.setTimeout(() => {
      void listAssets({ in_library: true, q: query.trim() || undefined, page_size: 12 })
        .then((res) => {
          if (cancelled) return;
          setAssetCands(
            res.items.map((a) => ({
              ref: mentionRefForAsset(a.kind, a.id),
              label: a.title,
              kind: a.kind,
              source: 'asset' as const,
            }))
          );
        })
        .catch(() => {
          if (!cancelled) setAssetCands([]);
        });
    }, 200);
    return () => {
      cancelled = true;
      window.clearTimeout(handle);
    };
  }, [open, query]);

  useEffect(() => setActive(0), [candidates.length]);

  const syncOverlay = (text: string, caret: number): void => {
    const upto = text.slice(0, caret);
    const m = MENTION_TOKEN.exec(upto);
    if (m) {
      setQuery(m[2]);
      setTokenStart(caret - m[2].length - 1);
      setRect(taRef.current?.getBoundingClientRect() ?? null);
      setOpen(true);
    } else {
      setOpen(false);
    }
  };

  const handleChange = (e: React.ChangeEvent<HTMLTextAreaElement>): void => {
    const text = e.target.value;
    onChange(text);
    syncOverlay(text, e.target.selectionStart ?? text.length);
  };

  const insert = (cand: MentionCandidate): void => {
    const ta = taRef.current;
    const caret = ta?.selectionStart ?? value.length;
    const before = value.slice(0, tokenStart);
    const after = value.slice(caret);
    const token = `@${cand.label} `;
    const next = `${before}${token}${after}`;
    onChange(next);
    onAddMention(cand.ref);
    setOpen(false);
    // Restore caret just past the inserted token.
    requestAnimationFrame(() => {
      const pos = before.length + token.length;
      if (ta) {
        ta.focus();
        ta.setSelectionRange(pos, pos);
      }
    });
  };

  const onKeyDown = (e: React.KeyboardEvent<HTMLTextAreaElement>): void => {
    e.stopPropagation(); // keep canvas shortcuts (Delete/Backspace/…) from firing
    if (!open || candidates.length === 0) {
      if (e.key === 'Escape') setOpen(false);
      return;
    }
    if (e.key === 'ArrowDown') {
      e.preventDefault();
      setActive((i) => (i + 1) % candidates.length);
    } else if (e.key === 'ArrowUp') {
      e.preventDefault();
      setActive((i) => (i - 1 + candidates.length) % candidates.length);
    } else if (e.key === 'Enter') {
      e.preventDefault();
      insert(candidates[active] ?? candidates[0]);
    } else if (e.key === 'Escape') {
      e.preventDefault();
      setOpen(false);
    }
  };

  return (
    <>
      <textarea
        ref={taRef}
        value={value}
        onChange={handleChange}
        onKeyDown={onKeyDown}
        onKeyUp={(e) => syncOverlay(e.currentTarget.value, e.currentTarget.selectionStart ?? 0)}
        onBlur={() => window.setTimeout(() => setOpen(false), 120)}
        placeholder={t('workshopGeneration.prompt.placeholder', { defaultValue: '描述你想生成的内容，输入 @ 引用素材…' })}
        className={[
          'nodrag nowheel w-full box-border resize-none rounded-9px border border-solid px-10px py-8px',
          'border-[var(--color-border-2)] bg-[var(--color-fill-1)] text-12px leading-[1.55] text-[var(--color-text-1)]',
          'outline-none transition-colors placeholder:text-[var(--color-text-3)] focus:border-[rgb(var(--primary-6))]',
        ].join(' ')}
        rows={3}
      />

      <Floating anchorRect={rect} open={open && candidates.length > 0} onClose={() => setOpen(false)} maxHeight={240}>
        <div className='flex items-center gap-6px border-b border-solid border-[var(--color-border-2)] border-l-0 border-r-0 border-t-0 px-10px py-6px'>
          <AtSign theme='outline' size={12} strokeWidth={3} className='text-[rgb(var(--primary-6))]' />
          <span className='text-10px font-600 text-[var(--color-text-3)]'>
            {t('workshopGeneration.mention.title', { defaultValue: '引用素材' })}
          </span>
        </div>
        <div className='min-h-0 flex-1 overflow-y-auto py-4px'>
          {candidates.map((c, i) => (
            <div
              key={c.ref}
              role='button'
              tabIndex={0}
              onMouseEnter={() => setActive(i)}
              onMouseDown={(e) => {
                e.preventDefault(); // keep textarea focus so caret restore works
                insert(c);
              }}
              className={[
                'mx-4px flex items-center gap-8px rounded-7px px-8px py-6px cursor-pointer transition-colors',
                i === active ? 'bg-[rgba(var(--primary-6),0.12)]' : 'hover:bg-[var(--color-fill-2)]',
              ].join(' ')}
            >
              <span className='flex h-18px w-18px shrink-0 items-center justify-center rounded-5px bg-[var(--color-fill-2)] text-[var(--color-text-2)]'>
                {KIND_ICON[c.kind]}
              </span>
              <span className='truncate text-12px text-[var(--color-text-1)]'>{c.label}</span>
              <span className='ml-auto shrink-0 text-9px uppercase tracking-wide text-[var(--color-text-3)]'>
                {c.source === 'node'
                  ? t('workshopGeneration.mention.onCanvas', { defaultValue: '画布' })
                  : t('workshopGeneration.mention.inLibrary', { defaultValue: '资产库' })}
              </span>
            </div>
          ))}
        </div>
      </Floating>
    </>
  );
};

export default PromptField;
