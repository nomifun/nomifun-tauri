/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import React, { useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Alert, Button, Checkbox, Empty, Message, Select } from '@arco-design/web-react';
import { ipcBridge } from '@/common';
import { httpRequest, isBackendHttpError } from '@/common/adapter/httpBridge';
import { isTauriRuntime } from '@/common/adapter/tauriRuntime';
import type { ICompanionExportResult, ICompanionWithStatus } from '@/common/adapter/ipcBridge';

/** Tagged import result of POST /api/companion/import (backend `ImportOutcome`). */
type ImportOutcome =
  | { kind: 'memory'; imported: number; skipped_duplicates: number }
  | { kind: 'companion'; companion_id: string; name: string; knowledge_names: string[] };

const ZIP_FILTERS = [{ name: 'Zip', extensions: ['zip'] }];

/** Native save dialog (desktop only — the whole tab is gated on isTauriRuntime). */
const pickSavePath = async (defaultName: string): Promise<string | null> => {
  const { save } = await import('@tauri-apps/plugin-dialog');
  return save({ defaultPath: defaultName, filters: ZIP_FILTERS });
};

/** Native open dialog via the existing ipcBridge dialog surface. */
const pickZipPath = async (): Promise<string | null> => {
  const paths = await ipcBridge.dialog.showOpen.invoke({
    properties: ['openFile'],
    filters: ZIP_FILTERS,
  });
  return paths?.[0] ?? null;
};

/** `2026-06-11` — date suffix for default bundle filenames. */
const today = (): string => new Date().toISOString().slice(0, 10);

/** Companion name → filename-safe fragment. */
const safeName = (s: string): string => s.replace(/[\\/:*?"<>|\s]+/g, '-').replace(/^-+|-+$/g, '') || 'companion';

/** Backend 400 messages pass through verbatim; everything else falls back to String(e). */
const errText = (e: unknown): string => {
  if (isBackendHttpError(e) && e.backendMessage) return e.backendMessage;
  return e instanceof Error ? e.message : String(e);
};

interface Props {
  companions: ICompanionWithStatus[];
}

/**
 * 迁移 Tab（共享域，spec §4.7/§4.8）：记忆中枢导出 / 伙伴包导出 / 迁移包导入。
 * 全部走路径式后端 API（绝对路径由原生对话框产出），Web 模式整 Tab 降级占位。
 */
const MigrateTab: React.FC<Props> = ({ companions }) => {
  const { t } = useTranslation();
  const [includeEvents, setIncludeEvents] = useState(false);
  const [selectedCompanionId, setSelectedCompanionId] = useState<string | undefined>(undefined);
  const [exportingMemory, setExportingMemory] = useState(false);
  const [exportingCompanion, setExportingCompanion] = useState(false);
  const [importingBundle, setImportingBundle] = useState(false);
  const [importingKb, setImportingKb] = useState(false);
  /** Knowledge-base names from an imported companion bundle with no local match. */
  const [unmatchedNames, setUnmatchedNames] = useState<string[]>([]);

  const effectiveCompanionId =
    selectedCompanionId && companions.some((p) => p.id === selectedCompanionId) ? selectedCompanionId : companions[0]?.id;

  // ── 1. memory hub export ──
  const exportMemory = async () => {
    const dest = await pickSavePath(`nomifun-memory-${today()}.zip`);
    if (!dest) return;
    setExportingMemory(true);
    try {
      const res = await ipcBridge.companion.exportMemory.invoke({ dest_path: dest, include_events: includeEvents });
      Message.success(t('nomi.migrate.exportMemoryOk', { path: res.dest_path }));
    } catch (e) {
      Message.error(errText(e));
    } finally {
      setExportingMemory(false);
    }
  };

  // ── 2. companion bundle export ──
  const exportCompanion = async () => {
    const companion = companions.find((p) => p.id === effectiveCompanionId);
    if (!companion) return;
    const dest = await pickSavePath(`nomifun-companion-${safeName(companion.name)}-${today()}.zip`);
    if (!dest) return;
    setExportingCompanion(true);
    try {
      // Collect the names of the knowledge bases bound to this companion — by spec
      // §4.8 the frontend supplies them (the companion crate never reaches into the
      // knowledge domain). No binding / lookup failure → export without refs.
      let knowledgeNames: string[] = [];
      try {
        const [binding, bases] = await Promise.all([
          ipcBridge.knowledge.getBinding.invoke({ kind: 'companion', target_id: companion.id }),
          ipcBridge.knowledge.listBases.invoke(),
        ]);
        const nameById = new Map(bases.map((b) => [b.id, b.name]));
        knowledgeNames = binding.kb_ids
          .map((id) => nameById.get(id))
          .filter((n): n is string => Boolean(n));
      } catch {
        /* export proceeds without knowledge refs */
      }
      // ipcBridge.companion.exportCompanion's body mapper only forwards dest_path, so the
      // frontend-collected knowledge_names go through httpRequest directly.
      const res = await httpRequest<ICompanionExportResult>('POST', `/api/companion/export/companions/${companion.id}`, {
        dest_path: dest,
        knowledge_names: knowledgeNames,
      });
      Message.success(t('nomi.migrate.exportCompanionOk', { path: res.dest_path }));
    } catch (e) {
      Message.error(errText(e));
    } finally {
      setExportingCompanion(false);
    }
  };

  // ── 3a. memory/companion bundle import (backend dispatches on manifest.kind) ──
  const importBundle = async () => {
    const src = await pickZipPath();
    if (!src) return;
    setImportingBundle(true);
    setUnmatchedNames([]);
    try {
      const outcome = (await ipcBridge.companion.importCompanionBundle.invoke({ src_path: src })) as ImportOutcome;
      if (outcome.kind === 'memory') {
        Message.success(
          t('nomi.migrate.importMemoryOk', { imported: outcome.imported, skipped: outcome.skipped_duplicates })
        );
      } else {
        // useCompanions refreshes the roster via the companion.created WS event.
        Message.success(t('nomi.migrate.importCompanionOk', { name: outcome.name }));
        await rebuildKnowledgeBinding(outcome);
      }
    } catch (e) {
      Message.error(errText(e));
    } finally {
      setImportingBundle(false);
    }
  };

  /** Match the bundle's knowledge_names against local bases and rebuild the companion binding. */
  const rebuildKnowledgeBinding = async (outcome: Extract<ImportOutcome, { kind: 'companion' }>) => {
    if (!outcome.knowledge_names.length) return;
    try {
      const bases = await ipcBridge.knowledge.listBases.invoke();
      const idByName = new Map(bases.map((b) => [b.name, b.id]));
      const matchedIds: string[] = [];
      const missing: string[] = [];
      for (const name of outcome.knowledge_names) {
        const id = idByName.get(name);
        if (id) matchedIds.push(id);
        else missing.push(name);
      }
      if (matchedIds.length) {
        await ipcBridge.knowledge.setBinding.invoke({
          kind: 'companion',
          target_id: outcome.companion_id,
          enabled: true,
          writeback: false,
          writeback_mode: 'staged',
          writeback_eagerness: 'conservative',
          channel_write_enabled: false,
          kb_ids: matchedIds,
        });
        Message.success(t('nomi.migrate.bindingRebuilt', { count: matchedIds.length }));
      }
      setUnmatchedNames(missing);
    } catch (e) {
      // Rebuild failed — surface every name so the user can bind manually.
      setUnmatchedNames(outcome.knowledge_names);
      Message.error(errText(e));
    }
  };

  // ── 3b. knowledge-base bundle import ──
  const importKnowledge = async () => {
    const src = await pickZipPath();
    if (!src) return;
    setImportingKb(true);
    try {
      const base = await ipcBridge.knowledge.importBase.invoke({ src_path: src });
      Message.success(t('nomi.migrate.importKnowledgeOk', { name: base.name }));
    } catch (e) {
      Message.error(errText(e));
    } finally {
      setImportingKb(false);
    }
  };

  if (!isTauriRuntime()) {
    return (
      <div className='py-40px'>
        <Empty description={t('nomi.migrate.desktopOnly')} />
      </div>
    );
  }

  return (
    <div className='flex flex-col gap-16px py-8px'>
      {/* 1. memory hub */}
      <section className='bg-fill-2 rd-10px px-14px py-12px'>
        <div className='text-14px text-t-primary font-500'>{t('nomi.migrate.memoryTitle')}</div>
        <div className='text-12px text-t-tertiary mt-2px'>{t('nomi.migrate.memoryDesc')}</div>
        <div className='flex items-center gap-16px mt-10px flex-wrap'>
          <Checkbox checked={includeEvents} onChange={(checked: boolean) => setIncludeEvents(checked)}>
            <span className='text-13px text-t-secondary'>{t('nomi.migrate.includeEvents')}</span>
          </Checkbox>
          <Button type='primary' loading={exportingMemory} onClick={() => void exportMemory()}>
            {t('nomi.migrate.exportMemory')}
          </Button>
        </div>
      </section>

      {/* 2. companion bundle */}
      <section className='bg-fill-2 rd-10px px-14px py-12px'>
        <div className='text-14px text-t-primary font-500'>{t('nomi.migrate.companionTitle')}</div>
        <div className='text-12px text-t-tertiary mt-2px'>{t('nomi.migrate.companionDesc')}</div>
        <div className='flex items-center gap-12px mt-10px flex-wrap'>
          <Select
            style={{ width: 220 }}
            placeholder={t('nomi.migrate.companionPlaceholder')}
            value={effectiveCompanionId}
            options={companions.map((p) => ({ label: p.name, value: p.id }))}
            onChange={(v: string) => setSelectedCompanionId(v)}
          />
          <Button type='primary' disabled={!effectiveCompanionId} loading={exportingCompanion} onClick={() => void exportCompanion()}>
            {t('nomi.migrate.exportCompanion')}
          </Button>
        </div>
      </section>

      {/* 3. import */}
      <section className='bg-fill-2 rd-10px px-14px py-12px'>
        <div className='text-14px text-t-primary font-500'>{t('nomi.migrate.importTitle')}</div>
        <div className='text-12px text-t-tertiary mt-2px'>{t('nomi.migrate.importDesc')}</div>
        <div className='flex items-center gap-12px mt-10px flex-wrap'>
          <Button loading={importingBundle} onClick={() => void importBundle()}>
            {t('nomi.migrate.importBundle')}
          </Button>
          <Button loading={importingKb} onClick={() => void importKnowledge()}>
            {t('nomi.migrate.importKnowledge')}
          </Button>
        </div>
        {unmatchedNames.length > 0 && (
          <Alert
            type='warning'
            className='mt-10px'
            content={
              <div>
                <div>{t('nomi.migrate.unmatchedTitle')}</div>
                <div className='mt-4px font-500'>{unmatchedNames.join(', ')}</div>
              </div>
            }
          />
        )}
      </section>
    </div>
  );
};

export default MigrateTab;
