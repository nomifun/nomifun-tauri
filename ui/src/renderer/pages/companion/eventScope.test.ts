/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import { describe, expect, it } from 'vitest';
import { isForCompanion } from './eventScope';
import { parseCompanionId } from '@/common/types/ids';

const C1 = parseCompanionId('019b0000-0000-7000-8000-000000000001');
const C2 = parseCompanionId('019b0000-0000-7000-8000-000000000002');

describe('isForCompanion', () => {
  it('命中本伙伴 id', () => expect(isForCompanion({ companion_id: C1 }, C1)).toBe(true));
  it('拒绝其他伙伴 id', () => expect(isForCompanion({ companion_id: C2 }, C1)).toBe(false));
  it('缺省(undefined/null)放行——兼容旧后端/全局事件', () => {
    expect(isForCompanion({}, C1)).toBe(true);
    expect(isForCompanion({ companion_id: null }, C1)).toBe(true);
  });
  it('重复拒绝其他 canonical 目标', () => expect(isForCompanion({ companion_id: C2 }, C1)).toBe(false));
});
