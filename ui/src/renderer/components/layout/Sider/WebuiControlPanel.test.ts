/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import { readFileSync } from 'node:fs';
import { describe, expect, test } from 'bun:test';

const readSource = (url: URL) => readFileSync(url, 'utf8');

describe('WebuiControlPanel QR login URL selection', () => {
  test('builds QR URLs from all access URLs instead of only status.networkUrl', () => {
    const source = readSource(new URL('./WebuiControlPanel.tsx', import.meta.url));

    expect(source.includes('getWebuiQrBaseUrls(status, accessUrls, port)')).toBe(true);
    expect(source.includes('status.allowRemote && status.networkUrl')).toBe(false);
  });

  test('makes the QR base URL selector a visible address decision', () => {
    const source = readSource(new URL('./WebuiControlPanel.tsx', import.meta.url));

    expect(source.includes('settings.webui.qrAddressPickerTitle')).toBe(true);
    expect(source.includes('settings.webui.qrAddressPickerDesc')).toBe(true);
    expect(source.includes('qr-address-picker')).toBe(true);
    expect(source.includes('border-[rgba(var(--primary-6),0.30)]')).toBe(true);
  });
});

describe('Open Capabilities WebUI entry', () => {
  test('moves the full WebUI control out of the crowded footer into the Open Capabilities page', () => {
    const footerSource = readSource(new URL('./SiderFooter.tsx', import.meta.url));
    const pageSource = readSource(new URL('../../../pages/openCapabilities/index.tsx', import.meta.url));
    const webuiTabStart = pageSource.indexOf("<Tabs.TabPane key='webui'");
    const mcpTabStart = pageSource.indexOf("<Tabs.TabPane key='mcp'");
    const webuiTabSource = pageSource.slice(webuiTabStart, mcpTabStart);

    expect(footerSource.includes('SiderWebuiControl')).toBe(false);
    expect(webuiTabStart).toBeGreaterThan(-1);
    expect(mcpTabStart).toBeGreaterThan(webuiTabStart);
    expect(webuiTabSource.includes("<WebuiControlPanel mode='page' />")).toBe(true);
    expect(webuiTabSource.includes('RegisterKnowledgeButton')).toBe(false);
    expect(webuiTabSource.includes('projectRegisterTitle')).toBe(false);
  });

  test('splits WebUI and MCP into subtabs instead of coupling both surfaces on one page', () => {
    const pageSource = readSource(new URL('../../../pages/openCapabilities/index.tsx', import.meta.url));

    expect(pageSource.includes("Tabs.TabPane key='webui'")).toBe(true);
    expect(pageSource.includes("Tabs.TabPane key='mcp'")).toBe(true);
    expect(pageSource.includes('activeOpenCapabilityTab')).toBe(true);
  });

  test('lets users choose NomiFun Remote MCP capability domains on this page', () => {
    const pageSource = readSource(new URL('../../../pages/openCapabilities/index.tsx', import.meta.url));

    expect(pageSource.includes('MCP_DOMAIN_OPTIONS')).toBe(true);
    expect(pageSource.includes('selectedMcpDomains')).toBe(true);
    expect(pageSource.includes('domainsQuery')).toBe(true);
    expect(pageSource.includes('<Checkbox')).toBe(true);
    expect(pageSource.includes("id: 'system'")).toBe(true);
    expect(pageSource.includes("id: 'mcp'")).toBe(true);
    expect(pageSource.includes("id: 'channel'")).toBe(true);
  });

  test('does not send users to the unrelated MCP server management page', () => {
    const pageSource = readSource(new URL('../../../pages/openCapabilities/index.tsx', import.meta.url));

    expect(pageSource.includes("navigate('/mcp')")).toBe(false);
    expect(pageSource.includes('openMcpManager')).toBe(false);
    expect(pageSource.includes('LinkCloud')).toBe(false);
  });

  test('keeps companion access tokens in the MCP capability tab instead of the WebUI panel', () => {
    const webuiPanelSource = readSource(new URL('./WebuiControlPanel.tsx', import.meta.url));
    const pageSource = readSource(new URL('../../../pages/openCapabilities/index.tsx', import.meta.url));

    expect(webuiPanelSource.includes('CompanionAccessTokenPanel')).toBe(false);
    expect(pageSource.includes('CompanionAccessTokenPanel')).toBe(true);
  });
});
