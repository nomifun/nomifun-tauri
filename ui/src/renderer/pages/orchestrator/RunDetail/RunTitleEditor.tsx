/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import React, { useCallback, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Input } from '@arco-design/web-react';

/**
 * RunTitleEditor — the run goal rendered as an inline-editable title, modeled on
 * the conversation page's {@link ChatTitleEditor} (hover-revealed edit affordance,
 * click → in-place Arco Input, Enter commits / Escape cancels / blur commits).
 * Never a bare `<button>`: the resting state is a `role="button"` span. Commits
 * route through {@link ipcBridge.orchestrator.runs.rename} (PATCH `{ goal }`).
 */
export const RunTitleEditor: React.FC<{
  goal: string;
  onRename: (goal: string) => Promise<void>;
}> = ({ goal, onRename }) => {
  const { t } = useTranslation();
  const goalText = goal.trim() || t('orchestrator.run.untitledGoal');

  const [editing, setEditing] = useState(false);
  const [draft, setDraft] = useState(goal);
  const [saving, setSaving] = useState(false);

  const beginEdit = useCallback(() => {
    setDraft(goal);
    setEditing(true);
  }, [goal]);

  const commitEdit = useCallback(async () => {
    const next = draft.trim();
    if (!next || next === goal.trim()) {
      setEditing(false);
      return;
    }
    setSaving(true);
    try {
      await onRename(next);
      setEditing(false);
    } finally {
      setSaving(false);
    }
  }, [draft, goal, onRename]);

  if (editing) {
    return (
      <div
        className='flex min-w-0 max-w-full flex-1 items-center rounded-12px border border-solid bg-fill-2 shadow-[0_1px_2px_rgba(15,23,42,0.06)]'
        style={{ borderColor: 'var(--color-fill-3)' }}
      >
        <div className='min-w-0 flex-1 px-8px py-3px'>
          <Input
            autoFocus
            value={draft}
            disabled={saving}
            maxLength={200}
            size='default'
            placeholder={t('orchestrator.run.header.renamePlaceholder')}
            className='w-full min-w-0 max-w-full border-none bg-transparent shadow-none [&_.arco-input-inner-wrapper]:border-none [&_.arco-input-inner-wrapper]:bg-transparent [&_.arco-input-inner-wrapper]:shadow-none [&_.arco-input]:bg-transparent [&_.arco-input]:px-0 [&_.arco-input]:text-15px [&_.arco-input]:font-600 [&_.arco-input]:leading-22px [&_.arco-input]:text-[var(--color-text-1)]'
            onChange={setDraft}
            onFocus={(event) => event.target.select()}
            onPressEnter={() => void commitEdit()}
            onBlur={() => void commitEdit()}
            onKeyDown={(event) => {
              if (event.key === 'Escape') {
                event.preventDefault();
                setDraft(goal);
                setEditing(false);
              }
            }}
          />
        </div>
      </div>
    );
  }

  return (
    <div className='group flex min-w-0 max-w-full flex-1 items-center rounded-12px border border-solid border-transparent transition-all duration-180 hover:bg-fill-2 hover:border-[var(--color-fill-3)] hover:shadow-[0_1px_2px_rgba(15,23,42,0.06)] focus-within:bg-fill-2 focus-within:border-[var(--color-fill-3)]'>
      <div className='min-w-0 flex-1 px-8px py-3px'>
        <span
          role='button'
          tabIndex={0}
          title={t('orchestrator.run.header.rename')}
          className='block min-w-0 cursor-text overflow-hidden text-ellipsis whitespace-nowrap text-15px font-600 leading-22px text-t-primary transition-colors duration-150 outline-none group-hover:text-[rgb(var(--primary-6))] group-focus-within:text-[rgb(var(--primary-6))]'
          onClick={beginEdit}
          onKeyDown={(event) => {
            if (event.key === 'Enter' || event.key === ' ') {
              event.preventDefault();
              beginEdit();
            }
          }}
        >
          {goalText}
        </span>
      </div>
    </div>
  );
};
