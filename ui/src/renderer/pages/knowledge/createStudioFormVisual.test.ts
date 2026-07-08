import { describe, expect, test } from 'bun:test';
import { readFileSync } from 'node:fs';

const studioSource = readFileSync(new URL('./CreateStudio/index.tsx', import.meta.url), 'utf8');
const sourceConfigSource = readFileSync(new URL('./CreateStudio/SourceConfig.tsx', import.meta.url), 'utf8');
const teachingCardSource = readFileSync(new URL('./CreateStudio/TeachingCard.tsx', import.meta.url), 'utf8');
const tagPickerSource = readFileSync(new URL('./CreateStudio/TagPicker.tsx', import.meta.url), 'utf8');

describe('CreateStudio form visual design', () => {
  test('uses a modern card-based form surface with soft focusable controls', () => {
    expect(studioSource.includes('knowledge-studio-config-panel')).toBe(true);
    expect(studioSource.includes('knowledge-studio-basic-card')).toBe(true);
    expect(studioSource.includes('knowledge-studio-field')).toBe(true);
    expect(studioSource.includes('knowledge-studio-input')).toBe(true);
    expect(studioSource.includes('focus-visible:shadow-[0_0_0_3px_rgba(var(--primary-6),0.12)]')).toBe(true);
    expect(studioSource.includes("className='w-full rounded-9px border border-[var(--color-border-2)] bg-[var(--color-fill-1)]")).toBe(false);
  });

  test('turns AI description helpers into real action controls instead of plain text links', () => {
    expect(studioSource.includes('knowledge-studio-ai-actions')).toBe(true);
    expect(studioSource.includes('knowledge-studio-ai-action')).toBe(true);
    expect(studioSource.includes('knowledge-studio-footer-action')).toBe(true);
    expect(studioSource.includes('hover:underline')).toBe(false);
    expect(studioSource.includes('MagicHat')).toBe(false);
    expect(studioSource.includes('<Plus')).toBe(false);
    expect(studioSource.includes('knowledge-studio-action-tag')).toBe(false);
    expect(studioSource.includes('stripLeadingAi')).toBe(false);
    expect(studioSource.includes('knowledge-studio-ai-action inline-flex h-28px items-center gap-5px rounded-8px border border-transparent bg-[var(--color-fill-1)]')).toBe(false);
    expect(studioSource.includes('knowledge-studio-ai-action inline-flex items-center gap-4px border-0 bg-transparent p-0')).toBe(true);
  });

  test('uses soft source and teaching panels instead of dated bordered callouts', () => {
    expect(sourceConfigSource.includes('knowledge-source-panel')).toBe(true);
    expect(sourceConfigSource.includes('knowledge-source-note')).toBe(true);
    expect(teachingCardSource.includes('knowledge-studio-teaching-card')).toBe(true);
    expect(teachingCardSource.includes('border border-[rgba(var(--primary-6),0.5)]')).toBe(false);
    expect(teachingCardSource.includes('linear-gradient(180deg, rgba(var(--primary-6), 0.08), rgba(var(--primary-6), 0.03))')).toBe(false);
  });

  test('uses the standard sliding Switch for the web browser-render option', () => {
    expect(sourceConfigSource.includes("import { Button, Input, Message, Select, Switch } from '@arco-design/web-react';")).toBe(true);
    expect(sourceConfigSource.includes('<Switch')).toBe(true);
    expect(sourceConfigSource.includes("onChange={(checked) => update({ browserRender: checked })}")).toBe(true);
    expect(sourceConfigSource.includes('peer-checked:after:translate-x-16px')).toBe(false);
  });

  test('keeps tag chips and the inline tag input visually consistent with the new form controls', () => {
    expect(tagPickerSource.includes('knowledge-studio-tag-chip')).toBe(true);
    expect(tagPickerSource.includes('knowledge-studio-tag-chip-active')).toBe(true);
    expect(tagPickerSource.includes('knowledge-studio-tag-chip-idle')).toBe(true);
    expect(tagPickerSource.includes('knowledge-studio-tag-chip-check')).toBe(true);
    expect(tagPickerSource.includes('knowledge-studio-tag-chip-label')).toBe(true);
    expect(tagPickerSource.includes("import { CheckOne } from '@icon-park/react';")).toBe(true);
    expect(tagPickerSource.includes('knowledge-studio-tag-chip inline-flex min-h-28px min-w-48px items-center justify-center gap-6px')).toBe(true);
    expect(tagPickerSource.includes('{selected && (')).toBe(true);
    expect(tagPickerSource.includes("'color-mix(in srgb, rgb(var(--primary-6)) 18%, var(--color-bg-2))'")).toBe(true);
    expect(tagPickerSource.includes('border-[rgba(var(--primary-6),0.58)]')).toBe(true);
    expect(tagPickerSource.includes('shadow-[0_0_0_2px_rgba(var(--primary-6),0.14),inset_0_1px_0_rgba(255,255,255,0.08)]')).toBe(true);
    expect(tagPickerSource.includes('knowledge-studio-tag-input')).toBe(true);
    expect(tagPickerSource.includes('border-transparent bg-transparent text-transparent opacity-0')).toBe(false);
    expect(tagPickerSource.includes('bg-[rgba(var(--primary-6),0.12)] text-[var(--color-text-1)] shadow-[inset_0_0_0_1px_rgba(var(--primary-6),0.26)]')).toBe(false);
    expect(tagPickerSource.includes('text-[rgb(var(--primary-6))] shadow-[inset_0_0_0_1px_rgba(var(--primary-6),0.22)]')).toBe(false);
    expect(tagPickerSource.includes('border-dashed')).toBe(false);
  });
});
