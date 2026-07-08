/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

/**
 * ShortcutsHelp — a dismissible overlay listing every canvas keyboard / mouse
 * shortcut. Opened from the toolbar "?" button (or the `?` / `Shift+/` key).
 */

import React, { useEffect } from 'react';
import { CloseSmall, Keyboard } from '@icon-park/react';
import { useTranslation } from 'react-i18next';

export interface ShortcutsHelpProps {
  onClose: () => void;
}

interface Shortcut {
  keysKey: string;
  keysFallback: string;
  descKey: string;
  descFallback: string;
}

const SHORTCUTS: Shortcut[] = [
  { keysKey: 'k.pan', keysFallback: '左键 / 中键拖拽', descKey: 'd.pan', descFallback: '平移画布' },
  { keysKey: 'k.zoom', keysFallback: '滚轮', descKey: 'd.zoom', descFallback: '以指针为锚缩放' },
  { keysKey: 'k.boxSelect', keysFallback: 'Ctrl/⌘ + 拖拽', descKey: 'd.boxSelect', descFallback: '框选' },
  { keysKey: 'k.addSelect', keysFallback: 'Shift / Ctrl + 点击', descKey: 'd.addSelect', descFallback: '追加 / 取消选择' },
  { keysKey: 'k.selectAll', keysFallback: 'Ctrl/⌘ + A', descKey: 'd.selectAll', descFallback: '全选' },
  { keysKey: 'k.copy', keysFallback: 'Ctrl/⌘ + C', descKey: 'd.copy', descFallback: '复制节点' },
  { keysKey: 'k.paste', keysFallback: 'Ctrl/⌘ + V', descKey: 'd.paste', descFallback: '粘贴（含系统剪贴板图片/文本）' },
  { keysKey: 'k.duplicate', keysFallback: 'Ctrl/⌘ + D', descKey: 'd.duplicate', descFallback: '复制副本' },
  { keysKey: 'k.group', keysFallback: 'Ctrl/⌘ + G', descKey: 'd.group', descFallback: '打组' },
  { keysKey: 'k.ungroup', keysFallback: 'Ctrl/⌘ + Shift + G', descKey: 'd.ungroup', descFallback: '解组' },
  { keysKey: 'k.delete', keysFallback: 'Delete / Backspace', descKey: 'd.delete', descFallback: '删除选中节点或连线' },
  { keysKey: 'k.undo', keysFallback: 'Ctrl/⌘ + Z', descKey: 'd.undo', descFallback: '撤销' },
  { keysKey: 'k.redo', keysFallback: 'Ctrl/⌘ + Shift + Z / Ctrl + Y', descKey: 'd.redo', descFallback: '重做' },
  { keysKey: 'k.assets', keysFallback: 'A', descKey: 'd.assets', descFallback: '打开 / 关闭资产库' },
  { keysKey: 'k.escape', keysFallback: 'Esc', descKey: 'd.escape', descFallback: '取消选择 / 关闭浮层' },
  { keysKey: 'k.connect', keysFallback: '拖拽右侧锚点', descKey: 'd.connect', descFallback: '连线；拖到空白处快捷建节点' },
];

const ShortcutsHelp: React.FC<ShortcutsHelpProps> = ({ onClose }) => {
  const { t } = useTranslation();

  useEffect(() => {
    const onKey = (e: KeyboardEvent): void => {
      if (e.key === 'Escape') onClose();
    };
    window.addEventListener('keydown', onKey);
    return () => window.removeEventListener('keydown', onKey);
  }, [onClose]);

  return (
    <div
      className='absolute inset-0 z-40 flex items-center justify-center bg-[rgba(0,0,0,0.42)] px-16px'
      onClick={onClose}
    >
      <div
        className='flex max-h-[80%] w-full max-w-460px flex-col overflow-hidden rounded-16px border border-solid border-[var(--color-border-2)] bg-[var(--color-bg-2)] shadow-[0_24px_64px_rgba(0,0,0,0.32)]'
        onClick={(e) => e.stopPropagation()}
      >
        <div className='flex items-center gap-9px border-b border-solid border-[var(--color-border-2)] border-l-0 border-r-0 border-t-0 px-18px py-14px'>
          <span className='flex h-26px w-26px items-center justify-center rounded-8px text-[rgb(var(--primary-6))]' style={{ background: 'rgba(var(--primary-6),0.12)' }}>
            <Keyboard theme='outline' size={16} strokeWidth={3} />
          </span>
          <span className='text-15px font-700 text-[var(--color-text-1)]'>
            {t('workshopCanvas.shortcuts.title', { defaultValue: '快捷键' })}
          </span>
          <div
            role='button'
            tabIndex={0}
            title={t('workshopCanvas.shortcuts.close', { defaultValue: '关闭' })}
            onClick={onClose}
            onKeyDown={(e) => {
              if (e.key === 'Enter' || e.key === ' ') {
                e.preventDefault();
                onClose();
              }
            }}
            className='ml-auto grid h-28px w-28px place-items-center rounded-7px cursor-pointer text-[var(--color-text-3)] hover:bg-[var(--color-fill-2)] hover:text-[var(--color-text-1)]'
          >
            <CloseSmall theme='outline' size={20} strokeWidth={3} />
          </div>
        </div>

        <div className='min-h-0 flex-1 overflow-y-auto px-18px py-10px'>
          {SHORTCUTS.map((s) => (
            <div key={s.keysKey} className='flex items-center justify-between gap-16px py-7px'>
              <span className='text-13px text-[var(--color-text-2)]'>
                {t(`workshopCanvas.shortcuts.${s.descKey}`, { defaultValue: s.descFallback })}
              </span>
              <kbd className='rounded-6px border border-solid border-[var(--color-border-2)] bg-[var(--color-fill-1)] px-8px py-3px text-11px font-600 text-[var(--color-text-1)] whitespace-nowrap'>
                {t(`workshopCanvas.shortcuts.${s.keysKey}`, { defaultValue: s.keysFallback })}
              </kbd>
            </div>
          ))}
        </div>
      </div>
    </div>
  );
};

export default ShortcutsHelp;
