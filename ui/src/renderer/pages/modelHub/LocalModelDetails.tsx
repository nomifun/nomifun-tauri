/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import React, { useState } from 'react';
import { Down } from '@icon-park/react';
import classNames from 'classnames';
import { useTranslation } from 'react-i18next';

export interface LocalModelDetailsProps {
  forcedOpen?: boolean;
  className?: string;
  children: React.ReactNode;
}

const LocalModelDetails: React.FC<LocalModelDetailsProps> = ({ forcedOpen = false, className, children }) => {
  const { t } = useTranslation();
  const [manualOpen, setManualOpen] = useState(false);
  const open = manualOpen || forcedOpen;

  return (
    <div className={classNames('mt-9px', className)}>
      <button
        type='button'
        aria-expanded={open}
        aria-disabled={forcedOpen}
        onClick={() => !forcedOpen && setManualOpen((current) => !current)}
        className={classNames(
          'h-28px border-none rd-7px px-8px inline-flex items-center gap-5px text-11px font-500 transition-colors',
          open ? 'bg-[var(--color-fill-2)] text-t-primary' : 'bg-transparent text-t-secondary',
          forcedOpen ? 'cursor-default' : 'cursor-pointer hover:bg-[var(--color-fill-2)] hover:text-t-primary'
        )}
      >
        <Down
          theme='outline'
          size='12'
          className={classNames('transition-transform duration-180', open ? 'rotate-180' : '-rotate-90')}
        />
        <span>
          {t(
            open
              ? 'settings.modelHub.local.capabilityCenter.collapseDetails'
              : 'settings.modelHub.local.capabilityCenter.details'
          )}
        </span>
      </button>
      {open && <div className='mt-8px px-2px'>{children}</div>}
    </div>
  );
};

export default LocalModelDetails;
