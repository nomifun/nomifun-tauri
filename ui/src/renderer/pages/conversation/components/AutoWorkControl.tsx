/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import React, { useEffect, useRef, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { useNavigate } from 'react-router-dom';
import { Button, Message, Popover, Select, Spin, Switch, Tooltip } from '@arco-design/web-react';
import type { SelectHandle } from '@arco-design/web-react/es/Select/interface';
import { ListAdd, Robot } from '@icon-park/react';
import classNames from 'classnames';
import { ipcBridge } from '@/common';
import type { AutoWorkRunState, AutoWorkTargetKind, IAutoWorkState } from '@/common/adapter/ipcBridge';
import { CAPABILITY_COLORS } from '@/renderer/components/capability/CapabilityIcon';
import { AUTOWORK_STATUS_COLOR } from '@/renderer/components/capability/capabilityStatusColors';
import { isLiveEventForTarget } from './liveEventMatch';
import { useRequirementTags } from '@renderer/pages/requirements/useRequirements';
import { capabilityHeaderButtonClass, capabilityHeaderButtonStyle } from './CapabilityHeaderButton';
import {
  getAutoWorkTagPickerMode,
  isAutoWorkTagPickerActionKey,
  isAutoWorkEnableBlocked,
  shouldFocusAutoWorkTagPickerAction,
} from './AutoWorkControl.model';

export interface AutoWorkTarget {
  kind: AutoWorkTargetKind;
  /** conversation/terminal session id — backend INTEGER (numeric-id spec §1). */
  id: number;
}

/** Draft (pre-creation) AutoWork config: held by the parent and applied once
 * the conversation exists (e.g. Guid page applies it right after create). */
export type AutoWorkDraftValue = { enabled: boolean; tag?: string };
export type AutoWorkDraft = {
  value: AutoWorkDraftValue;
  onChange: (next: AutoWorkDraftValue) => void;
};

interface AutoWorkControlProps {
  /** What this control drives: a chat conversation or a terminal session.
   * Omit when using `draft` mode. */
  target?: AutoWorkTarget;
  /** Controlled draft state — when set, the control never reads or writes the
   * backend AutoWork state; `target` is ignored. */
  draft?: AutoWorkDraft;
  /** When set, the control is disabled and this reason shows as a tooltip
   * (e.g. a terminal that is not an agent CLI). */
  disabledReason?: string;
  /** Optional safety note shown inside the popover (e.g. a non-full-auto terminal). */
  safetyHint?: string;
  /** When the config takes effect, shown at the panel bottom (draft mode). */
  applyNote?: string;
}

/**
 * Per-session AutoWork control. Rendered in a conversation header (kind
 * `conversation`) or a terminal session header (kind `terminal`). A compact
 * button opens a popover with the tag picker + toggle + tri-state status.
 * The Guid page renders the same control in `draft` mode to pre-configure a
 * conversation before it exists.
 */
const AutoWorkControl: React.FC<AutoWorkControlProps> = ({ target, draft, disabledReason, safetyHint, applyNote }) => {
  const { t } = useTranslation();
  const navigate = useNavigate();
  const { tags, loading: tagsLoading, error: tagsError, refresh: refreshTags } = useRequirementTags();
  const tagPickerMode = getAutoWorkTagPickerMode(tags.length, tagsLoading, tagsError);
  const tagOptions = tagPickerMode === 'ready'
    ? tags.map((tag) => ({ label: `${tag.tag} (${tag.done}/${tag.total})`, value: tag.tag }))
    : [];
  const tagPickerActionRef = useRef<HTMLButtonElement | null>(null);
  const tagSelectRef = useRef<SelectHandle>(null);
  const [state, setState] = useState<IAutoWorkState | null>(null);
  const [persistedTag, setPersistedTag] = useState<string | undefined>();
  const kind = target?.kind;
  const id = target?.id;
  const tag = draft ? draft.value.tag : persistedTag;
  const setTag = (next: string | undefined) => {
    if (draft) draft.onChange({ ...draft.value, tag: next });
    else setPersistedTag(next);
  };

  useEffect(() => {
    if (draft || !kind || !id) return;
    void ipcBridge.requirements.getAutoWork
      .invoke({ kind, target_id: id })
      .then((s) => {
        setState(s);
        setPersistedTag(s.tag);
      })
      .catch(() => {});
    // `draft` identity changes every render; only its presence matters here.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [kind, id, !!draft]);

  useEffect(() => {
    if (draft || !kind || !id) return;
    const unsub = ipcBridge.requirements.onAutoWork.on((s) => {
      if (isLiveEventForTarget(s.kind, s.target_id, kind, id)) {
        setState(s);
        setPersistedTag(s.tag);
      }
    });
    return () => unsub();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [kind, id, !!draft]);

  const enabled = draft ? draft.value.enabled : (state?.enabled ?? false);
  const running = state?.running ?? false;
  const runState: AutoWorkRunState = state?.run_state ?? 'off';
  // Draft mode shows "preset" wording with a neutral/primary dot — the live
  // run-state palette (idle orange / active green) is reserved for a running
  // session.
  const dotColor = draft ? (enabled ? CAPABILITY_COLORS.primary : CAPABILITY_COLORS.off) : AUTOWORK_STATUS_COLOR[runState];
  const statusText = draft
    ? enabled
      ? t('guid.advanced.draftOn')
      : t('guid.advanced.draftOff')
    : t(`requirements.autowork.state.${runState}`);

  const openNewRequirement = () => navigate('/requirements?new=1');

  const retryTags = () => {
    tagSelectRef.current?.focus();
    void refreshTags();
  };

  const setTagPickerActionRef = (node: unknown) => {
    tagPickerActionRef.current = node as HTMLButtonElement | null;
  };

  const handleTagPickerKeyDownCapture = (event: React.KeyboardEvent<HTMLDivElement>) => {
    if (
      !shouldFocusAutoWorkTagPickerAction(tagPickerMode, event.key, event.shiftKey) ||
      !tagPickerActionRef.current ||
      tagPickerActionRef.current.contains(event.target as Node)
    ) {
      return;
    }
    event.preventDefault();
    event.stopPropagation();
    tagPickerActionRef.current?.focus();
  };

  const handleTagPickerActionKeyDown = (event: React.KeyboardEvent<HTMLButtonElement>, action: () => void) => {
    if (!isAutoWorkTagPickerActionKey(event.key)) return;
    event.preventDefault();
    event.stopPropagation();
    action();
  };

  const tagPickerFeedback =
    tagPickerMode === 'loading' ? (
      <div
        className='flex items-center justify-center gap-8px px-16px py-18px text-12px text-t-tertiary'
        role='status'
        aria-live='polite'
        aria-atomic='true'
      >
        <Spin size={16} />
        <span>{t('requirements.autowork.loadingTags')}</span>
      </div>
    ) : tagPickerMode === 'error' ? (
      <div
        className='flex flex-col items-center gap-6px px-16px py-16px text-center'
        role='status'
        aria-live='polite'
        aria-atomic='true'
      >
        <span className='text-13px font-500 text-t-primary'>{t('requirements.autowork.loadErrorTitle')}</span>
        <span className='text-12px leading-16px text-t-tertiary'>
          {t('requirements.autowork.loadErrorDescription')}
        </span>
        <Button
          ref={setTagPickerActionRef}
          size='mini'
          type='text'
          onClick={retryTags}
          onKeyDown={(event) => handleTagPickerActionKeyDown(event, retryTags)}
        >
          {t('requirements.autowork.retry')}
        </Button>
      </div>
    ) : tagPickerMode === 'empty' ? (
      <div className='flex flex-col items-center gap-8px px-14px py-16px text-center'>
        <span
          className='grid h-34px w-34px place-items-center rounded-full bg-fill-2 text-primary-6'
          aria-hidden='true'
        >
          <ListAdd theme='outline' size='18' strokeWidth={3} />
        </span>
        <span className='text-13px font-500 text-t-primary'>{t('requirements.autowork.emptyTitle')}</span>
        <span className='text-12px leading-16px text-t-tertiary'>
          {t('requirements.autowork.emptyDescription')}
        </span>
        <Button
          ref={setTagPickerActionRef}
          size='mini'
          type='primary'
          shape='round'
          onClick={openNewRequirement}
          onKeyDown={(event) => handleTagPickerActionKeyDown(event, openNewRequirement)}
        >
          {t('requirements.autowork.emptyCta')}
        </Button>
      </div>
    ) : null;

  const toggle = async (next: boolean) => {
    if (next && !tag) {
      Message.warning(t('requirements.autowork.tagRequired'));
      return;
    }
    if (draft) {
      // Draft mode: just report upward — the parent applies after creation.
      draft.onChange({ enabled: next, tag });
      return;
    }
    if (!kind || !id) return;
    try {
      const s = await ipcBridge.requirements.setAutoWork.invoke({ kind, target_id: id, enabled: next, tag });
      setState(s);
      Message.success(next ? t('requirements.autowork.enabledOk') : t('requirements.autowork.disabledOk'));
    } catch (e) {
      Message.error(String(e));
    }
  };

  const panel = (
    <div className='flex flex-col gap-10px w-240px p-4px'>
      <div className='text-t-primary text-13px font-600'>{t('requirements.autowork.label')}</div>
      <div className='text-t-tertiary text-12px leading-16px'>{t('requirements.autowork.hint')}</div>
      {safetyHint ? <div className='text-12px leading-16px text-[rgb(var(--warning-6))]'>{safetyHint}</div> : null}
      <div className='flex flex-col gap-4px' onKeyDownCapture={handleTagPickerKeyDownCapture}>
        <span className='text-t-secondary text-12px'>{t('requirements.autowork.tagLabel')}</span>
        <Tooltip disabled={!enabled} content={t('requirements.autowork.disableToChangeTag')}>
          <Select
            ref={tagSelectRef}
            size='small'
            placeholder={t('requirements.autowork.selectTag')}
            value={tag}
            onChange={setTag}
            disabled={enabled}
            notFoundContent={tagPickerFeedback}
            options={tagOptions}
            allowClear
          />
        </Tooltip>
      </div>
      <div className='flex items-center justify-between'>
        <span className='inline-flex items-center gap-6px text-t-secondary text-12px'>
          <span className='inline-block w-6px h-6px rounded-full' style={{ backgroundColor: dotColor }} />
          {statusText}
        </span>
        <Switch
          checked={enabled}
          disabled={isAutoWorkEnableBlocked(enabled, tagPickerMode)}
          onChange={toggle}
        />
      </div>
      {running && state?.completed_count != null ? (
        <div className='text-t-tertiary text-11px'>
          {t('requirements.autowork.completedCount', { count: state.completed_count })}
        </div>
      ) : null}
      {applyNote ? <div className='text-t-quaternary text-11px leading-15px'>{applyNote}</div> : null}
    </div>
  );

  // Icon (tinted by run-state, matching the sidebar capability icon) + label
  // share one flex baseline. The status used to be a separate 6px dot beside a
  // primary-blue button; the icon itself now carries the status colour so the
  // header marker and the session-list icon are the same hue for every state.
  const button = (
    <Button
      size='mini'
      shape='round'
      type='secondary'
      disabled={!!disabledReason}
      className={capabilityHeaderButtonClass(enabled, 'shrink-0')}
      style={capabilityHeaderButtonStyle(dotColor)}
    >
      <span className='inline-flex items-center gap-6px leading-none'>
        <Robot
          theme='outline'
          size='14'
          fill={dotColor}
          className={classNames('block', runState === 'active' && 'autowork-spin')}
          style={{ lineHeight: 0 }}
        />
        <span className='text-12px'>{t('requirements.autowork.label')}</span>
      </span>
    </Button>
  );

  if (disabledReason) {
    // A disabled button does not fire pointer events, so wrap it for the tooltip.
    return (
      <Tooltip content={disabledReason}>
        <span className='inline-flex'>{button}</span>
      </Tooltip>
    );
  }

  return (
    <Popover trigger='click' position='br' content={panel}>
      {button}
    </Popover>
  );
};

export default AutoWorkControl;
