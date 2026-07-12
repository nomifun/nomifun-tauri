/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import classNames from 'classnames';
import React, { useMemo, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Button, Dropdown, Menu, Modal } from '@arco-design/web-react';
import { DeleteOne, More, PauseOne, PlayOne } from '@icon-park/react';

export type ScheduledTaskMenuAction = 'pause' | 'resume' | 'remove';

export function getScheduledTaskMenuActions(
  enabled: boolean,
  isManualOnly: boolean
): ScheduledTaskMenuAction[] {
  return isManualOnly ? ['remove'] : [enabled ? 'pause' : 'resume', 'remove'];
}

interface ScheduledTaskActionsProps {
  enabled: boolean;
  isManualOnly: boolean;
  onToggle: () => Promise<void>;
  onRemove: () => Promise<void>;
}

const ScheduledTaskActions: React.FC<ScheduledTaskActionsProps> = ({
  enabled,
  isManualOnly,
  onToggle,
  onRemove,
}) => {
  const { t } = useTranslation();
  const [menuVisible, setMenuVisible] = useState(false);
  const actions = useMemo(
    () => getScheduledTaskMenuActions(enabled, isManualOnly),
    [enabled, isManualOnly]
  );

  const handleMenuItem = (key: string) => {
    setMenuVisible(false);
    if (key === 'remove') {
      Modal.confirm({
        title: t('cron.confirmDeleteWithConversations'),
        okText: t('common.remove'),
        cancelText: t('common.cancel'),
        okButtonProps: { status: 'danger' },
        onOk: onRemove,
      });
      return;
    }
    void onToggle();
  };

  return (
    <div
      className='hidden shrink-0 md:block md:[grid-column:5] md:[grid-row:1] md:justify-self-center'
      onClick={(event) => event.stopPropagation()}
    >
      <Dropdown
        trigger='click'
        position='br'
        popupVisible={menuVisible}
        onVisibleChange={setMenuVisible}
        getPopupContainer={() => document.body}
        unmountOnExit={false}
        droplist={
          <Menu onClickMenuItem={handleMenuItem}>
            {actions.map((action) => {
              const isRemove = action === 'remove';
              const label = isRemove
                ? t('common.remove')
                : t(action === 'pause' ? 'cron.actions.pause' : 'cron.actions.resume');
              const icon = isRemove ? (
                <DeleteOne theme='outline' size='14' />
              ) : action === 'pause' ? (
                <PauseOne theme='outline' size='14' />
              ) : (
                <PlayOne theme='outline' size='14' />
              );

              return (
                <Menu.Item key={action}>
                  <div
                    className={classNames(
                      'flex items-center gap-8px',
                      isRemove && 'text-[rgb(var(--danger-6))]'
                    )}
                  >
                    {icon}
                    <span>{label}</span>
                  </div>
                </Menu.Item>
              );
            })}
          </Menu>
        }
      >
        <Button
          type='text'
          size='mini'
          aria-label={t('common.more')}
          className={classNames(
            '!h-24px !w-24px !min-w-24px !rounded-6px !p-0 !text-t-secondary',
            'pointer-events-none opacity-0 transition-opacity hover:!text-t-primary',
            'group-hover:pointer-events-auto group-hover:opacity-100',
            'focus-visible:pointer-events-auto focus-visible:opacity-100',
            menuVisible && '!pointer-events-auto !opacity-100'
          )}
          icon={<More theme='outline' size='14' fill='currentColor' className='block leading-none' />}
          onClick={() => {
            setMenuVisible(true);
          }}
        />
      </Dropdown>
    </div>
  );
};

export default ScheduledTaskActions;
