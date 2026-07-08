/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

/**
 * Model selector for the generation card. The trigger shows the current
 * provider + model; the dropdown lists mode-appropriate models grouped by
 * provider with a search box, and an empty state that links to the Model Hub
 * when nothing matches.
 */

import React, { useMemo, useRef, useState } from 'react';
import { useNavigate } from 'react-router-dom';
import { useTranslation } from 'react-i18next';
import { Down, MagicWand, Search } from '@icon-park/react';
import type { GenMode, ModelOption } from './genTypes';
import { useGeneratorModels } from './useGeneratorModels';
import Floating from './Floating';

export interface ModelPickerProps {
  mode: GenMode;
  providerId?: string;
  model?: string;
  onChange: (opt: ModelOption) => void;
}

const ModelPicker: React.FC<ModelPickerProps> = ({ mode, providerId, model, onChange }) => {
  const { t } = useTranslation();
  const navigate = useNavigate();
  const { groups, flat, hasProviders } = useGeneratorModels(mode);

  const [open, setOpen] = useState(false);
  const [query, setQuery] = useState('');
  const [rect, setRect] = useState<DOMRect | null>(null);
  const triggerRef = useRef<HTMLDivElement | null>(null);

  const selected = useMemo(
    () => flat.find((m) => m.providerId === providerId && m.model === model) ?? null,
    [flat, providerId, model]
  );

  const filteredGroups = useMemo(() => {
    const q = query.trim().toLowerCase();
    if (!q) return groups;
    return groups
      .map((g) => ({
        ...g,
        models: g.models.filter((m) => m.model.toLowerCase().includes(q) || g.providerName.toLowerCase().includes(q)),
      }))
      .filter((g) => g.models.length > 0);
  }, [groups, query]);

  const openMenu = (): void => {
    const r = triggerRef.current?.getBoundingClientRect() ?? null;
    setRect(r);
    setQuery('');
    setOpen(true);
  };

  const pick = (opt: ModelOption): void => {
    onChange(opt);
    setOpen(false);
  };

  const goToModelHub = (): void => {
    setOpen(false);
    navigate(mode === 'text' ? '/models?section=models' : '/models?section=creation');
  };

  return (
    <>
      <div
        ref={triggerRef}
        role='button'
        tabIndex={0}
        onClick={(e) => {
          e.stopPropagation();
          openMenu();
        }}
        onKeyDown={(e) => {
          if (e.key === 'Enter' || e.key === ' ') {
            e.preventDefault();
            openMenu();
          }
        }}
        className={[
          'nodrag flex w-full box-border items-center gap-8px rounded-9px border border-solid px-10px py-7px cursor-pointer',
          'transition-colors select-none',
          open
            ? 'border-[rgb(var(--primary-6))] bg-[rgba(var(--primary-6),0.06)]'
            : 'border-[var(--color-border-2)] bg-[var(--color-fill-1)] hover:border-[var(--color-border-3)]',
        ].join(' ')}
      >
        <span
          className='flex h-20px w-20px shrink-0 items-center justify-center rounded-6px text-[rgb(var(--primary-6))]'
          style={{ background: 'rgba(var(--primary-6),0.12)' }}
        >
          <MagicWand theme='outline' size={12} strokeWidth={3} />
        </span>
        <span className='flex min-w-0 flex-1 flex-col leading-tight'>
          {selected ? (
            <>
              <span className='truncate text-12px font-600 text-[var(--color-text-1)]'>{selected.model}</span>
              <span className='truncate text-10px text-[var(--color-text-3)]'>{selected.providerName}</span>
            </>
          ) : (
            <span className='truncate text-12px text-[var(--color-text-3)]'>
              {t('workshopGeneration.model.placeholder', { defaultValue: '选择模型' })}
            </span>
          )}
        </span>
        <Down theme='outline' size={13} strokeWidth={3} className='shrink-0 text-[var(--color-text-3)]' />
      </div>

      <Floating anchorRect={rect} open={open} onClose={() => setOpen(false)} maxHeight={340}>
        {flat.length === 0 ? (
          <div className='flex flex-col items-center gap-8px px-16px py-22px text-center'>
            <MagicWand theme='outline' size={26} className='text-[var(--color-text-3)]' />
            <span className='text-12px text-[var(--color-text-2)]'>
              {hasProviders
                ? t('workshopGeneration.model.emptyNoMatch', { defaultValue: '没有匹配的模型' })
                : t('workshopGeneration.model.emptyNoProviders', { defaultValue: '尚未配置模型平台' })}
            </span>
            <div
              role='button'
              tabIndex={0}
              onClick={goToModelHub}
              onKeyDown={(e) => (e.key === 'Enter' || e.key === ' ') && goToModelHub()}
              className='rounded-7px bg-[rgb(var(--primary-6))] px-12px py-6px text-11px font-600 text-white cursor-pointer hover:opacity-90'
            >
              {t('workshopGeneration.model.goToHub', { defaultValue: '前往模型中心' })}
            </div>
          </div>
        ) : (
          <>
            <div className='flex items-center gap-6px border-b border-solid border-[var(--color-border-2)] border-l-0 border-r-0 border-t-0 px-10px py-7px'>
              <Search theme='outline' size={13} strokeWidth={3} className='shrink-0 text-[var(--color-text-3)]' />
              <input
                autoFocus
                value={query}
                onChange={(e) => setQuery(e.target.value)}
                placeholder={t('workshopGeneration.model.search', { defaultValue: '搜索模型…' })}
                className='min-w-0 flex-1 border-none bg-transparent text-12px text-[var(--color-text-1)] outline-none placeholder:text-[var(--color-text-3)]'
              />
            </div>
            <div className='min-h-0 flex-1 overflow-y-auto py-4px'>
              {filteredGroups.length === 0 ? (
                <div className='px-14px py-16px text-center text-12px text-[var(--color-text-3)]'>
                  {t('workshopGeneration.model.emptyNoMatch', { defaultValue: '没有匹配的模型' })}
                </div>
              ) : (
                filteredGroups.map((g) => (
                  <div key={g.providerId} className='mb-2px'>
                    <div className='px-12px pb-3px pt-6px text-10px font-600 uppercase tracking-wide text-[var(--color-text-3)]'>
                      {g.providerName}
                    </div>
                    {g.models.map((m) => {
                      const active = selected?.providerId === m.providerId && selected?.model === m.model;
                      return (
                        <div
                          key={`${m.providerId}:${m.model}`}
                          role='button'
                          tabIndex={0}
                          onClick={() => pick(m)}
                          onKeyDown={(e) => (e.key === 'Enter' || e.key === ' ') && pick(m)}
                          className={[
                            'mx-4px flex items-center gap-8px rounded-7px px-8px py-6px cursor-pointer transition-colors',
                            active
                              ? 'bg-[rgba(var(--primary-6),0.12)] text-[rgb(var(--primary-6))]'
                              : 'text-[var(--color-text-1)] hover:bg-[var(--color-fill-2)]',
                          ].join(' ')}
                        >
                          <span className='truncate text-12px font-500'>{m.model}</span>
                          {active && (
                            <span className='ml-auto h-6px w-6px shrink-0 rounded-full bg-[rgb(var(--primary-6))]' />
                          )}
                        </div>
                      );
                    })}
                  </div>
                ))
              )}
            </div>
          </>
        )}
      </Floating>
    </>
  );
};

export default ModelPicker;
