/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import { describe, expect, test } from 'bun:test';
import { dialog } from './ipcBridge';
import * as bridge from '../../platform/bridge';

const realWindow = (globalThis as { window?: Window }).window;

function installWebRuntime() {
  (globalThis as { window?: unknown }).window = {};
}

function restoreWindow() {
  if (realWindow === undefined) {
    delete (globalThis as { window?: Window }).window;
  } else {
    (globalThis as { window?: Window }).window = realWindow;
  }
}

describe('dialog.showOpen WebUI fallback', () => {
  test('routes directory selection through the bridge in non-Tauri WebUI', async () => {
    installWebRuntime();

    let outboundName = '';
    let outboundProperties: string[] | undefined;

    const dispose = bridge.on('subscribe-show-open', (data: { id: string; data: { properties: string[] } }) => {
      outboundName = 'subscribe-show-open';
      outboundProperties = data.data.properties;
      bridge.emit(`subscribe.callback-show-open${data.id}`, ['/srv/projects/demo']);
    });

    try {
      const selection = dialog.showOpen.invoke({ properties: ['openDirectory', 'createDirectory'] });

      expect(outboundName).toBe('subscribe-show-open');
      expect(outboundProperties).toEqual(['openDirectory', 'createDirectory']);
      expect(await selection).toEqual(['/srv/projects/demo']);
    } finally {
      dispose();
      restoreWindow();
    }
  });
});
