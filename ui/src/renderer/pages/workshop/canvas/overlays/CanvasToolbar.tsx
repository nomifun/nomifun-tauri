/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

/**
 * CanvasToolbar — the top-right floating glass control cluster: undo / redo,
 * background style cycle, asset-library toggle, add-node, and the shortcuts
 * help. Rendered inside a react-flow `<Panel>` so it floats over the graph.
 */

import React from 'react';
import { GridNine, Help, ImageFiles, ListMiddle, Plus, Redo, Square, Undo } from '@icon-park/react';
import { useTranslation } from 'react-i18next';
import type { WorkshopCanvasBackground } from '../../types';

const CHROME_BTN =
  'grid h-30px w-30px place-items-center rounded-8px cursor-pointer transition-colors text-[var(--color-text-2)] hover:bg-[var(--color-fill-2)] hover:text-[var(--color-text-1)]';
const CHROME_BTN_DISABLED = 'grid h-30px w-30px place-items-center rounded-8px text-[var(--color-text-3)] opacity-45 cursor-not-allowed';
const CHROME_BTN_ACTIVE =
  'grid h-30px w-30px place-items-center rounded-8px cursor-pointer transition-colors text-[rgb(var(--primary-6))]';

export interface CanvasToolbarProps {
  canUndo: boolean;
  canRedo: boolean;
  onUndo: () => void;
  onRedo: () => void;
  background: WorkshopCanvasBackground;
  onCycleBackground: () => void;
  assetsOpen: boolean;
  onToggleAssets: () => void;
  onAddNode: () => void;
  onOpenHelp: () => void;
}

const ChromeButton: React.FC<{
  label: string;
  onClick?: () => void;
  disabled?: boolean;
  active?: boolean;
  children: React.ReactNode;
}> = ({ label, onClick, disabled, active, children }) => (
  <div
    role='button'
    tabIndex={disabled ? -1 : 0}
    title={label}
    aria-label={label}
    aria-disabled={disabled}
    onClick={() => {
      if (!disabled) onClick?.();
    }}
    onKeyDown={(e) => {
      if (disabled) return;
      if (e.key === 'Enter' || e.key === ' ') {
        e.preventDefault();
        onClick?.();
      }
    }}
    className={disabled ? CHROME_BTN_DISABLED : active ? CHROME_BTN_ACTIVE : CHROME_BTN}
  >
    {children}
  </div>
);

const BG_ICON: Record<WorkshopCanvasBackground, React.ReactNode> = {
  dots: <GridNine theme='outline' size={17} strokeWidth={3} />,
  lines: <ListMiddle theme='outline' size={17} strokeWidth={3} />,
  blank: <Square theme='outline' size={17} strokeWidth={3} />,
};

const CanvasToolbar: React.FC<CanvasToolbarProps> = ({
  canUndo,
  canRedo,
  onUndo,
  onRedo,
  background,
  onCycleBackground,
  assetsOpen,
  onToggleAssets,
  onAddNode,
  onOpenHelp,
}) => {
  const { t } = useTranslation();
  const bgLabel = t(`workshopCanvas.background.${background}`, { defaultValue: background });

  return (
    <div className='flex items-center gap-1px rounded-11px border border-solid border-[var(--color-border-2)] bg-[var(--color-bg-2)] px-4px py-4px shadow-[0_8px_28px_rgba(0,0,0,0.16)] backdrop-blur-md'>
      <ChromeButton label={t('workshopCanvas.toolbar.undo', { defaultValue: '撤销 (Ctrl+Z)' })} onClick={onUndo} disabled={!canUndo}>
        <Undo theme='outline' size={17} strokeWidth={3} />
      </ChromeButton>
      <ChromeButton label={t('workshopCanvas.toolbar.redo', { defaultValue: '重做 (Ctrl+Shift+Z)' })} onClick={onRedo} disabled={!canRedo}>
        <Redo theme='outline' size={17} strokeWidth={3} />
      </ChromeButton>

      <span className='mx-3px h-18px w-1px bg-[var(--color-border-2)]' />

      <ChromeButton
        label={t('workshopCanvas.toolbar.background', { style: bgLabel, defaultValue: '背景：{{style}}' })}
        onClick={onCycleBackground}
      >
        {BG_ICON[background]}
      </ChromeButton>
      <ChromeButton label={t('workshopCanvas.toolbar.addNode', { defaultValue: '新建节点' })} onClick={onAddNode}>
        <Plus theme='outline' size={18} strokeWidth={3} />
      </ChromeButton>
      <ChromeButton label={t('workshopCanvas.toolbar.assets', { defaultValue: '资产库 (A)' })} onClick={onToggleAssets} active={assetsOpen}>
        <ImageFiles theme='outline' size={17} strokeWidth={3} />
      </ChromeButton>

      <span className='mx-3px h-18px w-1px bg-[var(--color-border-2)]' />

      <ChromeButton label={t('workshopCanvas.toolbar.help', { defaultValue: '快捷键帮助 (?)' })} onClick={onOpenHelp}>
        <Help theme='outline' size={17} strokeWidth={3} />
      </ChromeButton>
    </div>
  );
};

export default CanvasToolbar;
