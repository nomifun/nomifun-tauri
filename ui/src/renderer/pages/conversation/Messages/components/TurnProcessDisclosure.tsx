/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import type { TurnDisclosureProcessState } from '../turnDisclosureModel';
import { Down } from '@icon-park/react';
import classNames from 'classnames';
import React, { useEffect, useMemo, useRef, useState } from 'react';
import { useTranslation } from 'react-i18next';

export interface TurnProcessDisclosureView<T> {
  id: string;
  processItems: T[];
  startAt: number;
  endAt: number;
  state: TurnDisclosureProcessState;
  running: boolean;
  defaultCollapsed: boolean;
}

interface TurnProcessDisclosureProps<T> {
  item: TurnProcessDisclosureView<T>;
  highlighted?: boolean;
  renderProcessItem: (item: T) => React.ReactNode;
  getProcessItemKey: (item: T) => string;
  getProcessItemState: (item: T) => TurnDisclosureProcessState;
  getProcessItemLayoutKind?: (item: T) => string;
}

export interface TurnProcessDisclosureExpansionSnapshot {
  itemId: string;
  hasProcessItems: boolean;
}

const labelKeyByState: Record<TurnDisclosureProcessState, string> = {
  completed: 'messages.turnProcessed',
  running: 'messages.turnProcessing',
  waiting: 'messages.turnWaiting',
  failed: 'messages.turnFailed',
  canceled: 'messages.turnCanceled',
};

const defaultLabelByState: Record<TurnDisclosureProcessState, string> = {
  completed: 'Processed {{duration}}',
  running: 'Processing {{duration}}',
  waiting: 'Waiting for confirmation {{duration}}',
  failed: 'Failed {{duration}}',
  canceled: 'Canceled {{duration}}',
};

const sanitizeDomId = (value: string): string => value.replace(/[^A-Za-z0-9_-]/g, '_');

const getDefaultExpanded = (hasProcessItems: boolean, defaultCollapsed: boolean): boolean =>
  hasProcessItems && !defaultCollapsed;

export function shouldResetTurnProcessDisclosureExpansion(
  previous: TurnProcessDisclosureExpansionSnapshot,
  next: TurnProcessDisclosureExpansionSnapshot
): boolean {
  if (previous.itemId !== next.itemId) return true;
  if (previous.hasProcessItems !== next.hasProcessItems) return true;
  return false;
}

const formatTurnDuration = (ms: number, t: ReturnType<typeof useTranslation>['t']): string => {
  const totalSeconds = Math.max(0, Math.round(ms / 1000));
  const sUnit = t('common.unit.second_short', { defaultValue: 's' });
  const mUnit = t('common.unit.minute_short', { defaultValue: 'm' });
  const hUnit = t('common.unit.hour_short', { defaultValue: 'h' });

  if (totalSeconds < 60) return `${totalSeconds}${sUnit}`;
  const minutes = Math.floor(totalSeconds / 60);
  const seconds = totalSeconds % 60;
  if (minutes < 60) return `${minutes}${mUnit} ${seconds}${sUnit}`;
  const hours = Math.floor(minutes / 60);
  const remainingMinutes = minutes % 60;
  return `${hours}${hUnit} ${remainingMinutes}${mUnit}`;
};

function TurnProcessDisclosure<T>({
  item,
  highlighted = false,
  renderProcessItem,
  getProcessItemKey,
  getProcessItemState,
  getProcessItemLayoutKind,
}: TurnProcessDisclosureProps<T>) {
  const { t } = useTranslation();
  const hasProcessItems = item.processItems.length > 0;
  const [expanded, setExpanded] = useState(() => getDefaultExpanded(hasProcessItems, item.defaultCollapsed));
  const [now, setNow] = useState(() => Date.now());
  const expansionSnapshotRef = useRef<TurnProcessDisclosureExpansionSnapshot>({
    itemId: item.id,
    hasProcessItems,
  });

  useEffect(() => {
    const nextSnapshot: TurnProcessDisclosureExpansionSnapshot = { itemId: item.id, hasProcessItems };
    const shouldReset = shouldResetTurnProcessDisclosureExpansion(expansionSnapshotRef.current, nextSnapshot);
    expansionSnapshotRef.current = nextSnapshot;

    if (shouldReset) {
      setExpanded(getDefaultExpanded(hasProcessItems, item.defaultCollapsed));
    }
  }, [hasProcessItems, item.defaultCollapsed, item.id]);

  useEffect(() => {
    if (highlighted && hasProcessItems) setExpanded(true);
  }, [hasProcessItems, highlighted]);

  useEffect(() => {
    if (!item.running) return;
    setNow(Date.now());
    const timer = window.setInterval(() => {
      setNow(Date.now());
    }, 1000);
    return () => window.clearInterval(timer);
  }, [item.running]);

  const currentItemKey = useMemo(() => {
    const activeItem = item.processItems.findLast((processItem) => {
      const state = getProcessItemState(processItem);
      return state === 'running' || state === 'waiting';
    });
    const failedItem =
      activeItem ??
      item.processItems.findLast((processItem) => {
        const state = getProcessItemState(processItem);
        return state === 'failed' || state === 'canceled';
      });
    const latestItem = failedItem ?? item.processItems.at(-1);
    return latestItem ? getProcessItemKey(latestItem) : undefined;
  }, [getProcessItemKey, getProcessItemState, item.processItems]);

  const durationEndAt = item.running ? now : item.endAt;
  const duration = formatTurnDuration(durationEndAt - item.startAt, t);
  const label = t(labelKeyByState[item.state], {
    duration,
    defaultValue: defaultLabelByState[item.state],
  });
  const bodyId = `turn-process-disclosure-body-${sanitizeDomId(item.id)}`;
  const disclosureExpanded = hasProcessItems && expanded;

  return (
    <div className={classNames('turn-process-disclosure', `turn-process-disclosure--${item.state}`)}>
      <button
        type='button'
        className={classNames(
          'turn-process-disclosure__header',
          !hasProcessItems && 'turn-process-disclosure__header--static'
        )}
        onClick={() => {
          if (hasProcessItems) setExpanded((value) => !value);
        }}
        aria-expanded={hasProcessItems ? disclosureExpanded : undefined}
        aria-controls={hasProcessItems ? bodyId : undefined}
      >
        <span className='turn-process-disclosure__label'>{label}</span>
        {hasProcessItems && (
          <Down
            theme='outline'
            size='14'
            fill='currentColor'
            className={classNames(
              'turn-process-disclosure__arrow',
              disclosureExpanded && 'turn-process-disclosure__arrow--open'
            )}
          />
        )}
      </button>
      {disclosureExpanded && (
        <div id={bodyId} className='turn-process-disclosure__body'>
          {item.processItems.map((processItem) => {
            const itemKey = getProcessItemKey(processItem);
            const state = getProcessItemState(processItem);
            const layoutKind = getProcessItemLayoutKind?.(processItem) ?? 'other';
            return (
              <div
                key={itemKey}
                className={classNames(
                  'turn-process-disclosure__item',
                  `turn-process-disclosure__item--${layoutKind}`,
                  `turn-process-disclosure__item--${state}`,
                  itemKey === currentItemKey && 'turn-process-disclosure__item--current'
                )}
              >
                {renderProcessItem(processItem)}
              </div>
            );
          })}
        </div>
      )}
    </div>
  );
}

export default TurnProcessDisclosure;
