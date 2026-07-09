/**
 * AssistantTagPicker — A chip-toggle group for one tag dimension in the edit
 * drawer. Mirrors the filter-bar chip language (idle fill-2 / active primary
 * triad) and supports inline-create: an "+ Add" affordance reveals an input
 * that calls onCreateTag, then auto-selects the new key.
 *
 * Theme variables only; `<div onClick>` for clickables (no <button>).
 */
import type {
  AssistantTag,
  AssistantTagDimension,
  CreateAssistantTagRequest,
} from '@/common/types/agent/assistantTypes';
import { Input } from '@arco-design/web-react';
import { Close, Plus } from '@icon-park/react';
import React, { forwardRef, useCallback, useEffect, useImperativeHandle, useRef, useState } from 'react';
import { useTranslation } from 'react-i18next';

type AssistantTagPickerProps = {
  dimension: AssistantTagDimension;
  label: string;
  tags: AssistantTag[];
  value: string[];
  onChange: (v: string[]) => void;
  onCreateTag: (req: CreateAssistantTagRequest) => Promise<AssistantTag>;
  localeKey: string;
  readOnly: boolean;
  commitOnBlur?: boolean;
};

export type AssistantTagPickerHandle = {
  flushPendingTag: () => Promise<string[]>;
  resetPendingTag: () => void;
};

const AssistantTagPicker = forwardRef<AssistantTagPickerHandle, AssistantTagPickerProps>(function AssistantTagPicker(
  { dimension, label, tags, value, onChange, onCreateTag, localeKey, readOnly, commitOnBlur = false },
  ref
) {
  const { t } = useTranslation();
  const [adding, setAdding] = useState(false);
  const [draft, setDraft] = useState('');
  const [creating, setCreating] = useState(false);
  const latestValueRef = useRef(value);
  const draftRef = useRef('');
  const creatingPromiseRef = useRef<Promise<string[]> | null>(null);

  useEffect(() => {
    latestValueRef.current = value;
  }, [value]);

  const resetPendingTag = useCallback(() => {
    draftRef.current = '';
    setAdding(false);
    setDraft('');
  }, []);

  const setDraftValue = useCallback((next: string) => {
    draftRef.current = next;
    setDraft(next);
  }, []);

  const toggle = (key: string) => {
    if (readOnly) return;
    const next = value.includes(key) ? value.filter((k) => k !== key) : [...value, key];
    latestValueRef.current = next;
    onChange(next);
  };

  const submitNew = useCallback(async (): Promise<string[]> => {
    if (creatingPromiseRef.current) return creatingPromiseRef.current;

    const newLabel = draftRef.current.trim();
    if (!newLabel) {
      resetPendingTag();
      return latestValueRef.current;
    }

    const promise = (async () => {
      setCreating(true);
      try {
        const created = await onCreateTag({ dimension, label: newLabel });
        const current = latestValueRef.current;
        const next = current.includes(created.key) ? current : [...current, created.key];
        latestValueRef.current = next;
        onChange(next);
        resetPendingTag();
        return next;
      } finally {
        setCreating(false);
        creatingPromiseRef.current = null;
      }
    })();
    creatingPromiseRef.current = promise;
    return promise;
  }, [dimension, onChange, onCreateTag, resetPendingTag]);

  useImperativeHandle(
    ref,
    () => ({
      flushPendingTag: submitNew,
      resetPendingTag,
    }),
    [resetPendingTag, submitNew]
  );

  const handleAddBlur = (event: React.FocusEvent<HTMLDivElement>) => {
    if (!commitOnBlur) return;
    const nextFocus = event.relatedTarget;
    if (nextFocus && event.currentTarget.contains(nextFocus as Node)) return;
    void submitNew().catch((error) => {
      console.error('Failed to create tag from picker:', error);
    });
  };

  return (
    <div className='flex flex-col gap-8px'>
      <span className='text-12px font-medium text-[var(--color-text-3)]'>{label}</span>
      <div className='flex flex-wrap items-center gap-8px'>
        {tags.map((tag) => {
          const active = value.includes(tag.key);
          const tagLabel = tag.label_i18n?.[localeKey] || tag.label;
          return (
            <div
              key={tag.key}
              role='button'
              tabIndex={readOnly ? -1 : 0}
              aria-pressed={active}
              onClick={() => toggle(tag.key)}
              onKeyDown={(e) => {
                if (!readOnly && (e.key === 'Enter' || e.key === ' ')) {
                  e.preventDefault();
                  toggle(tag.key);
                }
              }}
              className={[
                'inline-flex items-center select-none rounded-[16px] px-12px py-3px text-13px leading-20px',
                'border border-solid transition-all duration-150 whitespace-nowrap',
                readOnly ? 'cursor-default' : 'cursor-pointer',
                active
                  ? 'bg-[var(--color-primary-light-1)] text-[rgb(var(--primary-6))] border-[var(--color-primary-light-3)] font-medium'
                  : 'bg-[var(--color-fill-2)] text-[var(--color-text-2)] border-[var(--color-border-2)] ' +
                    (readOnly ? '' : 'hover:bg-[var(--color-fill-3)] hover:text-[var(--color-text-1)]'),
              ].join(' ')}
            >
              {tagLabel}
            </div>
          );
        })}

        {!readOnly &&
          (adding ? (
            <div className='inline-flex items-center gap-6px' onBlur={handleAddBlur}>
              <Input
                size='small'
                autoFocus
                value={draft}
                onChange={setDraftValue}
                onPressEnter={() => {
                  void submitNew().catch((error) => {
                    console.error('Failed to create tag from picker:', error);
                  });
                }}
                disabled={creating}
                placeholder={t('settings.assistantTagAddPlaceholder', { defaultValue: 'New tag…' })}
                className='!w-128px !rounded-[16px]'
              />
              <div
                role='button'
                tabIndex={0}
                onMouseDown={(e) => e.preventDefault()}
                onClick={() => {
                  resetPendingTag();
                }}
                onKeyDown={(e) => {
                  if (e.key === 'Enter') {
                    resetPendingTag();
                  }
                }}
                className='flex items-center justify-center w-20px h-20px rounded-full cursor-pointer text-[var(--color-text-3)] hover:bg-[var(--color-fill-2)] transition-colors'
              >
                <Close theme='outline' size={13} strokeWidth={3} />
              </div>
            </div>
          ) : (
            <div
              role='button'
              tabIndex={0}
              data-testid={`tag-picker-add-${dimension}`}
              onClick={() => setAdding(true)}
              onKeyDown={(e) => {
                if (e.key === 'Enter' || e.key === ' ') {
                  e.preventDefault();
                  setAdding(true);
                }
              }}
              className={[
                'inline-flex items-center gap-4px select-none rounded-[16px] px-11px py-3px text-13px leading-20px cursor-pointer',
                'border border-dashed border-[var(--color-border-3)] text-[var(--color-text-3)] transition-all duration-150',
                'hover:text-[rgb(var(--primary-6))] hover:border-[var(--color-primary-light-3)] hover:bg-[var(--color-primary-light-1)]',
              ].join(' ')}
            >
              <Plus theme='outline' size={13} strokeWidth={3} />
              {t('common.add', { defaultValue: 'Add' })}
            </div>
          ))}

        {tags.length === 0 && readOnly && (
          <span className='text-12px text-[var(--color-text-3)]'>
            {t('settings.assistantTagNone', { defaultValue: 'None' })}
          </span>
        )}
      </div>
    </div>
  );
});

AssistantTagPicker.displayName = 'AssistantTagPicker';

export default AssistantTagPicker;
