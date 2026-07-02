/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import { readFileSync } from 'node:fs';
import { describe, expect, test } from 'bun:test';

const readSource = (url: URL) => readFileSync(url, 'utf8');

describe('Guid resource cards placement', () => {
  test('renders resource cards in the centered stage and companion poster in the scroll discovery area', () => {
    const source = readSource(new URL('../GuidPage.tsx', import.meta.url));

    const inputIndex = source.indexOf('<GuidInputCard');
    const resourceIndex = source.indexOf('<GuidResourceCards', inputIndex);
    const primaryStageIndex = source.indexOf('className={styles.guidPrimaryStage}');
    const discoveryAreaIndex = source.indexOf('className={styles.guidDiscoveryArea}');
    const companionPreviewIndex = source.indexOf('<GuidCompanionPosterPreview', discoveryAreaIndex);
    const editorHostIndex = source.indexOf('<GuidAssistantEditorHost', inputIndex);
    const discoveryAreaEndIndex = source.indexOf('{/* SummonDrawer', discoveryAreaIndex);
    const discoveryAreaSource = source.slice(discoveryAreaIndex, discoveryAreaEndIndex);

    expect(primaryStageIndex).toBeGreaterThan(-1);
    expect(inputIndex).toBeGreaterThan(-1);
    expect(resourceIndex).toBeGreaterThan(inputIndex);
    expect(editorHostIndex).toBeGreaterThan(resourceIndex);
    expect(discoveryAreaIndex).toBeGreaterThan(editorHostIndex);
    expect(companionPreviewIndex).toBeGreaterThan(discoveryAreaIndex);
    expect(discoveryAreaSource.includes('activeSkillCount={activeSkillCount}')).toBe(false);
    expect(discoveryAreaSource.includes('workspaceDir={guidInput.dir}')).toBe(false);
    expect(discoveryAreaSource.includes('currentModelName={modelSelection.current_model')).toBe(false);
    expect(source.includes('onFillPrompt')).toBe(false);
  });

  test('contains docs, promo video, and contact feedback cards without recent prompt data access', () => {
    const source = readSource(new URL('./GuidResourceCards.tsx', import.meta.url));

    expect(source.includes('https://www.nomifun.com/docs')).toBe(true);
    expect(source.includes('https://www.bilibili.com/video/BV1kwKZ6UE5X/')).toBe(true);
    expect(source.includes('https://youtu.be/AsEToBDFR9s')).toBe(true);
    expect(source.includes('https://youtu.be/gEDo5H0H0Pg')).toBe(false);
    expect(source.includes('https://www.nomifun.com/contact')).toBe(true);
    expect(source.includes('https://github.com/nomifun/nomifun-tauri/issues')).toBe(false);
    expect(source.includes('RECENT_PROMPT_LIMIT')).toBe(false);
    expect(source.includes('getConversationMessages')).toBe(false);
    expect(source.includes('useConversationHistoryContext')).toBe(false);
    expect(source.includes('useSWR')).toBe(false);
    expect(source.includes('onFillPrompt')).toBe(false);
  });

  test('companion poster renders real companion figures instead of status data', () => {
    const source = readSource(new URL('./GuidCompanionPosterPreview.tsx', import.meta.url));

    expect(source.includes('conversation.companionPoster.title')).toBe(true);
    expect(source.includes('CompanionAvatar')).toBe(true);
    expect(source.includes('useCompanions')).toBe(true);
    expect(source.includes('customFigureMetaOf')).toBe(true);
    expect(source.includes('activeSkillCount')).toBe(false);
    expect(source.includes('workspaceDir')).toBe(false);
    expect(source.includes('currentModelName')).toBe(false);
    expect(source.includes('guidCompanionStatusGrid')).toBe(false);
  });
});
