import { readFileSync } from 'node:fs';
import { describe, expect, test } from 'bun:test';

const readSource = (url: URL) => readFileSync(url, 'utf8');

describe('model modality editing UI', () => {
  test('does not preselect inferred model categories when adding a model', () => {
    const source = readSource(new URL('./AddModelModal.tsx', import.meta.url));

    expect(source.includes('deriveDefaultTasks')).toBe(false);
    expect(source.includes("useState<ModelTask[]>([])")).toBe(true);
    expect(source.includes("setTasks([])")).toBe(true);
    expect(source.includes("tasks: tasks.length > 0 ? tasks : ['chat']")).toBe(false);
    expect(source.includes('buildModelProfileUpsertRequest')).toBe(true);
  });

  test('adds the same empty-default category selector to the provider creation modal', () => {
    const source = readSource(new URL('./AddPlatformModal.tsx', import.meta.url));

    expect(source.includes('MODEL_TASK_ORDER')).toBe(true);
    expect(source.includes("useState<ModelTask[]>([])")).toBe(true);
    expect(source.includes("setTasks([])")).toBe(true);
    expect(source.includes("field={'model_modality'}")).toBe(true);
    expect(source.includes('buildModelProfileUpsertRequest')).toBe(true);
    expect(source.includes('await onSubmit(provider)')).toBe(true);
    expect(source.includes('ipcBridge.modelProfile.upsert.invoke')).toBe(true);
    expect(source.includes("tasks: tasks.length > 0 ? tasks : ['chat']")).toBe(false);
  });

  test('exposes an existing-model category editor backed by model profiles', () => {
    const source = readSource(
      new URL('../../../components/settings/SettingsModal/contents/ModelModalContent.tsx', import.meta.url)
    );

    expect(source.includes('ModelModalityEditor')).toBe(true);
    expect(source.includes('visibleModelTaskBadges')).toBe(true);
    expect(source.includes('buildModelProfileUpsertRequest')).toBe(true);
    expect(source.includes('ipcBridge.modelProfile.upsert.invoke')).toBe(true);
    expect(source.includes('mutateProfiles')).toBe(true);
    expect(source.includes("settings.editModelModality")).toBe(true);
  });
});
