/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import type { TurnDisclosureProcessState } from '../turnDisclosureModel';
import { Spin } from '@arco-design/web-react';
import { Attention, Brain, CheckOne, Edit, FolderOpen, Right, Terminal } from '@icon-park/react';
import classNames from 'classnames';
import React, { useEffect, useRef, useState } from 'react';

export type TurnProcessReceiptIcon = 'tool' | 'file' | 'edit' | 'thinking' | 'permission' | 'status';

export interface TurnProcessReceiptView<T> {
  id: string;
  item: T;
  label: string;
  state: TurnDisclosureProcessState;
  icon: TurnProcessReceiptIcon;
  defaultExpanded: boolean;
  hasDetail?: boolean;
}

interface TurnProcessReceiptProps<T> {
  receipt: TurnProcessReceiptView<T>;
  highlighted?: boolean;
  renderProcessItem: (item: T) => React.ReactNode;
}

export interface TurnProcessReceiptExpansionSnapshot {
  receiptId: string;
  canExpand: boolean;
}

const sanitizeDomId = (value: string): string => value.replace(/[^A-Za-z0-9_-]/g, '_');

const getDefaultExpanded = (defaultExpanded: boolean, canExpand: boolean): boolean => defaultExpanded && canExpand;

const receiptIconMarkerByIcon: Record<TurnProcessReceiptIcon, string> = {
  tool: 'terminal',
  file: 'file',
  edit: 'edit',
  thinking: 'thinking',
  permission: 'permission',
  status: 'status',
};

export function shouldResetTurnProcessReceiptExpansion(
  previous: TurnProcessReceiptExpansionSnapshot,
  next: TurnProcessReceiptExpansionSnapshot
): boolean {
  if (previous.receiptId !== next.receiptId) return true;
  if (previous.canExpand !== next.canExpand) return true;
  return false;
}

const ReceiptIcon: React.FC<{
  icon: TurnProcessReceiptIcon;
  state: TurnDisclosureProcessState;
}> = ({ icon, state }) => {
  if (state === 'running') return <Spin size={12} />;
  if (state === 'failed' || state === 'canceled') return <Attention theme='outline' size='15' fill='currentColor' />;
  if (icon === 'file') return <FolderOpen theme='outline' size='15' fill='currentColor' />;
  if (icon === 'edit') return <Edit theme='outline' size='15' fill='currentColor' />;
  if (icon === 'thinking') return <Brain theme='outline' size='15' fill='currentColor' />;
  if (icon === 'permission') return <Attention theme='outline' size='15' fill='currentColor' />;
  if (icon === 'status') return <CheckOne theme='outline' size='15' fill='currentColor' />;
  return <Terminal theme='outline' size='15' fill='currentColor' />;
};

function TurnProcessReceipt<T>({ receipt, highlighted = false, renderProcessItem }: TurnProcessReceiptProps<T>) {
  const canExpand = receipt.hasDetail === true;
  const [expanded, setExpanded] = useState(() => getDefaultExpanded(receipt.defaultExpanded, canExpand));
  const expansionSnapshotRef = useRef<TurnProcessReceiptExpansionSnapshot>({
    receiptId: receipt.id,
    canExpand,
  });

  useEffect(() => {
    const nextSnapshot: TurnProcessReceiptExpansionSnapshot = { receiptId: receipt.id, canExpand };
    const shouldReset = shouldResetTurnProcessReceiptExpansion(expansionSnapshotRef.current, nextSnapshot);
    expansionSnapshotRef.current = nextSnapshot;

    if (shouldReset) {
      setExpanded(getDefaultExpanded(receipt.defaultExpanded, canExpand));
    }
  }, [canExpand, receipt.defaultExpanded, receipt.id]);

  useEffect(() => {
    if (highlighted && canExpand) setExpanded(true);
  }, [canExpand, highlighted]);

  const bodyId = `turn-process-receipt-body-${sanitizeDomId(receipt.id)}`;
  const receiptIconMarker =
    receipt.state === 'running'
      ? 'loading'
      : receipt.state === 'failed' || receipt.state === 'canceled'
        ? 'attention'
        : receiptIconMarkerByIcon[receipt.icon];
  const headerContent = (
    <>
      <span className='turn-process-receipt__icon' aria-hidden='true' data-receipt-icon={receiptIconMarker}>
        <ReceiptIcon icon={receipt.icon} state={receipt.state} />
      </span>
      <span className='turn-process-receipt__label'>{receipt.label}</span>
      {canExpand && (
        <Right
          theme='outline'
          size='13'
          className={classNames('turn-process-receipt__arrow', expanded && 'turn-process-receipt__arrow--open')}
        />
      )}
    </>
  );

  return (
    <div className={classNames('turn-process-receipt', `turn-process-receipt--${receipt.state}`)}>
      {canExpand ? (
        <button
          type='button'
          className='turn-process-receipt__header'
          onClick={() => setExpanded((value) => !value)}
          aria-expanded={expanded}
          aria-controls={bodyId}
        >
          {headerContent}
        </button>
      ) : (
        <div className='turn-process-receipt__header turn-process-receipt__header--static'>{headerContent}</div>
      )}
      {canExpand && expanded && (
        <div id={bodyId} className='turn-process-receipt__body'>
          {renderProcessItem(receipt.item)}
        </div>
      )}
    </div>
  );
}

export default TurnProcessReceipt;
