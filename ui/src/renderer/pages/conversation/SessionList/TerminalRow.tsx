/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import React, { useCallback, useEffect, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { useNavigate } from 'react-router-dom';
import { DeleteOne, EditOne, More, PlayOne, Power, Pushpin, Refresh, Terminal } from '@icon-park/react';
import { Checkbox, Dropdown, Input, Menu, Modal, Popover } from '@arco-design/web-react';
import classNames from 'classnames';
import { ipcBridge } from '@/common';
import type { AutoWorkRunState, IdmmRunState, ITerminalSession } from '@/common/adapter/ipcBridge';
import { CapabilityIconCluster } from '@/renderer/components/capability/CapabilityIcon';
import FlexFullContainer from '@/renderer/components/layout/FlexFullContainer';
import TerminalHoverCard from '@/renderer/pages/conversation/components/TerminalHoverCard';

import { buildSessionCapabilityItems, CAPABILITY_ICON_SIZE } from './utils/sessionCapabilityItems';
import { formatSessionAgeLabel } from './utils/sessionAge';

interface TerminalRowProps {
  session: ITerminalSession;
  active: boolean;
  onClick: () => void;
  selectionMode?: boolean;
  selected?: boolean;
  onToggleSelect?: () => void;
  /** Indent row content (pl-34px) while keeping the bg full-width — mirrors ConversationRow's dimIcon, for rows nested inside a workpath drawer. */
  indent?: boolean;
  /** Aggregated status of cron jobs targeting this terminal session ('none' = no jobs). */
  cronStatus?: 'none' | 'active' | 'paused' | 'error';
  /** AutoWork run state when enabled for this terminal (undefined = not enabled / unknown). */
  autoworkState?: AutoWorkRunState;
  /** IDMM run state when enabled for this terminal (undefined = not enabled / unknown). */
  idmmState?: IdmmRunState;
  /** Sidebar display preference: show/hide the compact age marker on the right. */
  showSessionAge?: boolean;
}

const statusDotClass = (status: ITerminalSession['last_status']) =>
  status === 'running' ? 'bg-green-500' : status === 'error' ? 'bg-red-500' : 'bg-t-tertiary';

/**
 * Single terminal-session row, extracted from TerminalSiderSection for reuse in
 * the unified workpath SessionList tree. Visuals are aligned with
 * ConversationRow (same row height / hover background / name typography);
 * behaviors (status dot, pin, rename, lifecycle, delete, "..." menu) are
 * carried over from the original sidebar row unchanged. The right-click
 * context menu opens the same dropdown as the "..." button.
 */
const TerminalRow: React.FC<TerminalRowProps> = ({
  session,
  active,
  onClick,
  selectionMode,
  selected,
  onToggleSelect,
  indent,
  cronStatus = 'none',
  autoworkState,
  idmmState,
  showSessionAge = true,
}) => {
  const { t } = useTranslation();
  const navigate = useNavigate();
  const [menuVisible, setMenuVisible] = useState(false);
  const [renameVisible, setRenameVisible] = useState(false);
  const [renameName, setRenameName] = useState('');

  // Session-level capability markers (trailing cluster), shared builder with
  // ConversationRow: 定时任务 → 自动工作 → 智能决策.
  const capabilityItems = buildSessionCapabilityItems(t, { cronStatus, autoworkState, idmmState });
  const ageLabel = formatSessionAgeLabel(t, session.created_at);

  // 进入批量选择模式时菜单容器被卸载，但 menuVisible 残留会在退出选择模式时把菜单弹回来
  useEffect(() => {
    if (selectionMode) {
      setMenuVisible(false);
    }
  }, [selectionMode]);

  // Pin/unpin: PATCH the top-level `pinned` field, same call as the original row.
  const handlePin = useCallback(async () => {
    await ipcBridge.terminal.update.invoke({ id: session.id, pinned: !session.pinned });
  }, [session.id, session.pinned]);

  const handleRenameOpen = useCallback(() => {
    setRenameName(session.name);
    setRenameVisible(true);
  }, [session.name]);

  const handleRenameConfirm = useCallback(async () => {
    const trimmed = renameName.trim();
    if (trimmed && trimmed !== session.name) {
      await ipcBridge.terminal.update.invoke({ id: session.id, name: trimmed });
    }
    setRenameVisible(false);
  }, [renameName, session.id, session.name]);

  const handleDelete = useCallback(() => {
    Modal.confirm({
      title: t('terminal.deleteConfirmTitle'),
      content: t('terminal.deleteConfirmContent'),
      okButtonProps: { status: 'danger' },
      onOk: async () => {
        await ipcBridge.terminal.remove.invoke({ id: session.id });
        // Navigate away if the deleted session is currently open
        if (active) {
          navigate('/guid');
        }
      },
    });
  }, [session.id, active, navigate, t]);

  // 关闭 (close): kill the running process; the session row stays (status → exited).
  const handleClose = useCallback(async () => {
    await ipcBridge.terminal.kill.invoke({ id: session.id });
  }, [session.id]);

  // 唤醒 / 重启 (wake / restart): respawn the PTY in place for the same session id.
  const handleRelaunch = useCallback(async () => {
    await ipcBridge.terminal.relaunch.invoke({ id: session.id });
  }, [session.id]);

  const handleRowClick = () => {
    if (selectionMode) {
      onToggleSelect?.();
      return;
    }
    onClick();
  };

  // Right-click opens the same menu as the "..." button (ConversationRow paradigm).
  const handleRowContextMenu = (event: React.MouseEvent<HTMLDivElement>) => {
    event.preventDefault();
    event.stopPropagation();
    if (selectionMode) {
      return;
    }
    setMenuVisible(true);
  };

  return (
    <>
      {/* Hover popover mirrors ConversationRow: surfaces the terminal's config
          (id/cwd/command/backend/status) so both session kinds align. */}
      <Popover
        trigger='hover'
        position='right'
        content={<TerminalHoverCard session={session} />}
        triggerProps={{ mouseEnterDelay: 400 }}
      >
        <div
          id={`terminal-${session.id}`}
        className={classNames(
          'chat-history__item h-34px rd-8px flex items-center group cursor-pointer relative overflow-hidden shrink-0 min-w-0 transition-colors justify-start gap-8px pr-16px',
          indent ? 'pl-34px' : 'pl-10px',
          {
            'hover:bg-fill-3': !selectionMode && !active,
            '!bg-primary-1 !text-primary-6': active,
            'bg-[rgba(var(--primary-6),0.08)]': selectionMode && selected,
          }
        )}
        onClick={handleRowClick}
        onContextMenu={handleRowContextMenu}
      >
        {selectionMode && (
          <span
            className='mr-8px flex-center'
            onClick={(event) => {
              event.stopPropagation();
              onToggleSelect?.();
            }}
          >
            <Checkbox checked={!!selected} />
          </span>
        )}
        {/* Leading icon with pin overlay for pinned sessions */}
        <span className='size-22px flex items-center justify-center shrink-0 relative'>
          <Terminal
            theme='outline'
            size='16'
            className={classNames('line-height-0 flex-shrink-0 text-t-secondary', {
              'group-hover:opacity-0 transition-opacity': !!session.pinned,
            })}
          />
          {!selectionMode && session.pinned && (
            <span
              className='absolute inset-0 flex-center text-t-secondary pointer-events-none opacity-0 group-hover:opacity-100 transition-opacity'
              style={{ lineHeight: 0 }}
            >
              <Pushpin theme='outline' size='14' />
            </span>
          )}
        </span>
        {/* Capability markers are identity/status signals, so keep them before the
            session name instead of handing their slot to hover actions. */}
        {!selectionMode && capabilityItems.length > 0 && (
          <CapabilityIconCluster items={capabilityItems} size={CAPABILITY_ICON_SIZE} className='shrink-0' />
        )}
        {/* Name owns the flexible middle; age is a fixed right-aligned marker so
            rows scan cleanly without metadata hugging the title. */}
        <FlexFullContainer className='h-24px min-w-0 flex-1' containerClassName='flex items-center'>
          <span className='chat-history__item-name min-w-0 text-14px font-[500] lh-24px text-t-primary'>{session.name}</span>
        </FlexFullContainer>
        {/* Pin dot indicator for pinned sessions (visible at rest, hidden on hover when menu shows) */}
        {!selectionMode && session.pinned && (
          <span className={classNames('size-6px rd-full shrink-0 bg-aou-1', { 'group-hover:hidden': !menuVisible })} />
        )}
        {/* Status dot (hidden on hover when actions are visible) */}
        <span
          className={classNames('size-6px rd-full shrink-0', statusDotClass(session.last_status), {
            'group-hover:hidden': !selectionMode,
          })}
        />
        {showSessionAge && ageLabel && (
          <span
            className={classNames('shrink-0 w-40px text-right text-11px text-t-tertiary', {
              'group-hover:hidden': !menuVisible,
              hidden: menuVisible,
            })}
          >
            {ageLabel}
          </span>
        )}
        {/* Per-row action dropdown (visible on hover, hidden in selection mode) */}
        {!selectionMode && (
          <div
            className={classNames('absolute right-8px top-1/2 -translate-y-1/2 items-center justify-end', {
              flex: menuVisible,
              'hidden group-hover:flex': !menuVisible,
            })}
            onClick={(e) => e.stopPropagation()}
          >
            <Dropdown
              droplist={
                <Menu
                  onClickMenuItem={(key) => {
                    setMenuVisible(false);
                    if (key === 'pin') {
                      void handlePin();
                    } else if (key === 'rename') {
                      handleRenameOpen();
                    } else if (key === 'delete') {
                      handleDelete();
                    } else if (key === 'close') {
                      void handleClose();
                    } else if (key === 'wake' || key === 'restart') {
                      void handleRelaunch();
                    }
                  }}
                >
                  <Menu.Item key='pin'>
                    <div className='flex items-center gap-8px'>
                      <Pushpin theme='outline' size='14' />
                      <span>{session.pinned ? t('terminal.action.unpin') : t('terminal.action.pin')}</span>
                    </div>
                  </Menu.Item>
                  {/* Lifecycle: running → restart + close; exited → wake. */}
                  {session.last_status === 'running' ? (
                    <Menu.Item key='restart'>
                      <div className='flex items-center gap-8px'>
                        <Refresh theme='outline' size='14' />
                        <span>{t('terminal.action.restart')}</span>
                      </div>
                    </Menu.Item>
                  ) : (
                    <Menu.Item key='wake'>
                      <div className='flex items-center gap-8px'>
                        <PlayOne theme='outline' size='14' />
                        <span>{t('terminal.action.wake')}</span>
                      </div>
                    </Menu.Item>
                  )}
                  {session.last_status === 'running' && (
                    <Menu.Item key='close'>
                      <div className='flex items-center gap-8px'>
                        <Power theme='outline' size='14' />
                        <span>{t('terminal.action.close')}</span>
                      </div>
                    </Menu.Item>
                  )}
                  <Menu.Item key='rename'>
                    <div className='flex items-center gap-8px'>
                      <EditOne theme='outline' size='14' />
                      <span>{t('terminal.action.rename')}</span>
                    </div>
                  </Menu.Item>
                  <Menu.Item key='delete'>
                    <div className='flex items-center gap-8px text-[rgb(var(--warning-6))]'>
                      <DeleteOne theme='outline' size='14' />
                      <span>{t('terminal.action.delete')}</span>
                    </div>
                  </Menu.Item>
                </Menu>
              }
              trigger='click'
              position='br'
              popupVisible={menuVisible}
              onVisibleChange={(visible) => setMenuVisible(visible)}
              getPopupContainer={() => document.body}
              unmountOnExit={false}
            >
              <span
                title={t('common.more')}
                className={classNames(
                  'flex-center cursor-pointer transition-colors text-t-secondary hover:text-t-primary size-20px rd-4px sider-action-btn',
                  {
                    flex: menuVisible,
                    'hidden group-hover:flex': !menuVisible,
                  }
                )}
                onClick={(e) => {
                  e.stopPropagation();
                  setMenuVisible((v) => !v);
                }}
              >
                <More theme='outline' size='14' fill='currentColor' className='block leading-none' />
              </span>
            </Dropdown>
          </div>
        )}
      </div>
      </Popover>

      {/* Rename modal */}
      <Modal
        title={t('terminal.renameTitle')}
        visible={renameVisible}
        onOk={handleRenameConfirm}
        onCancel={() => setRenameVisible(false)}
        okText={t('common.confirm')}
        cancelText={t('common.cancel')}
        autoFocus
        unmountOnExit
      >
        <Input
          value={renameName}
          onChange={setRenameName}
          placeholder={t('terminal.renamePlaceholder')}
          onPressEnter={handleRenameConfirm}
        />
      </Modal>
    </>
  );
};

export default TerminalRow;
