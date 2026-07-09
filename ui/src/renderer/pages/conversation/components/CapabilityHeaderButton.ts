/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import type { CSSProperties } from 'react';
import classNames from 'classnames';

export const capabilityHeaderButtonClass = (active: boolean, extra?: string) =>
  classNames('capability-header-btn', active ? 'capability-header-btn--active' : 'capability-header-btn--inactive', extra);

export const capabilityHeaderButtonStyle = (accent: string): CSSProperties =>
  ({
    '--capability-accent': accent,
  }) as CSSProperties;
