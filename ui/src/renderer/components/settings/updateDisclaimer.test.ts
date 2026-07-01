/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import { readFileSync } from 'node:fs';
import { describe, expect, test } from 'bun:test';

const readSource = (url: URL) => readFileSync(url, 'utf8');

describe('update disclaimer', () => {
  test('keeps the requested nonprofit data-loss disclaimer fixed in Chinese', () => {
    const zhUpdate = JSON.parse(readSource(new URL('../../services/i18n/locales/zh-CN/update.json', import.meta.url)));

    expect(zhUpdate.disclaimer).toBe(
      '免责声明：这是一个公益免费开源项目，故项目作者不承担任何版本迭代导致用户数据丢失、损坏的后果，请谨慎进行升级。'
    );
  });

  test('renders the disclaimer once in the update modal bottom chrome', () => {
    const updateModalSource = readSource(new URL('./UpdateModal.tsx', import.meta.url));
    const renderDisclaimerCalls = updateModalSource.match(/renderDisclaimer\(/g) ?? [];

    expect(renderDisclaimerCalls).toHaveLength(1);
    expect(/renderDisclaimer\(\s*'shrink-0 border-t/.test(updateModalSource)).toBe(true);
  });
});
