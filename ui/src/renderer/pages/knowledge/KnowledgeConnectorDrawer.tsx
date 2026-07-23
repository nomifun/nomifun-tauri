/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import React, { useCallback, useEffect, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Button, Drawer, Empty, Input, Message, Popconfirm, Select, Spin, Tag } from '@arco-design/web-react';
import { Check, Delete, Refresh } from '@icon-park/react';
import { ipcBridge } from '@/common';
import type { IConnectorCredentialSummary, IKnowledgeBase } from '@/common/adapter/ipcBridge';
import { getBaseSource, knowledgeErrorText, notifySourceFetchResult } from './useKnowledge';
import type { ConnectorCredentialId } from '@/common/types/ids';

interface KnowledgeConnectorDrawerProps {
  visible: boolean;
  onClose: () => void;
  base: IKnowledgeBase;
  /** Refresh the base after attach/sync/detach. */
  onChanged: () => void;
}

const FEISHU = 'feishu';

/**
 * Per-base source-connector entry point (Feishu first). Manages connector
 * credentials (create / test / delete — all probed server-side), attaches a
 * Feishu wiki space as this base's source, and triggers the sync that pulls
 * remote docs into `snapshots/`.
 */
const KnowledgeConnectorDrawer: React.FC<KnowledgeConnectorDrawerProps> = ({ visible, onClose, base, onChanged }) => {
  const { t } = useTranslation();
  const source = getBaseSource(base);
  const connected = source?.kind === FEISHU;

  const [creds, setCreds] = useState<IConnectorCredentialSummary[]>([]);
  const [credsLoading, setCredsLoading] = useState(false);

  // New-credential form.
  const [name, setName] = useState('');
  const [appId, setAppId] = useState('');
  const [appSecret, setAppSecret] = useState('');
  const [creating, setCreating] = useState(false);

  // Attach form.
  const [credId, setCredId] = useState<ConnectorCredentialId | undefined>(undefined);
  const [spaceId, setSpaceId] = useState('');
  const [attaching, setAttaching] = useState(false);
  const [syncing, setSyncing] = useState(false);
  const [testingId, setTestingId] = useState<ConnectorCredentialId | null>(null);

  const refreshCreds = useCallback(async () => {
    setCredsLoading(true);
    try {
      const all = await ipcBridge.knowledge.listCredentials.invoke();
      setCreds(all.filter((c) => c.kind === FEISHU));
    } catch (e) {
      Message.error(knowledgeErrorText(e));
    } finally {
      setCredsLoading(false);
    }
  }, []);

  useEffect(() => {
    if (visible) void refreshCreds();
  }, [visible, refreshCreds]);

  // Default the attach selector to the first credential.
  useEffect(() => {
    if (!credId && creds.length > 0) setCredId(creds[0].credentialId);
  }, [creds, credId]);

  const handleCreateCredential = async () => {
    if (creating) return;
    if (!name.trim() || !appId.trim() || !appSecret.trim()) {
      Message.warning(t('knowledge.connector.credFormIncomplete'));
      return;
    }
    setCreating(true);
    try {
      // The server probes the app_id/app_secret before storing — a bad secret fails here.
      const created = await ipcBridge.knowledge.createCredential.invoke({
        kind: FEISHU,
        name: name.trim(),
        payload: { app_id: appId.trim(), app_secret: appSecret.trim() },
      });
      Message.success(t('knowledge.connector.credCreateOk'));
      setName('');
      setAppId('');
      setAppSecret('');
      await refreshCreds();
      setCredId(created.credentialId);
    } catch (e) {
      Message.error(knowledgeErrorText(e));
    } finally {
      setCreating(false);
    }
  };

  const handleTest = async (id: ConnectorCredentialId) => {
    setTestingId(id);
    try {
      const identity = await ipcBridge.knowledge.testCredential.invoke({ credential_id: id });
      Message.success(
        t('knowledge.connector.testOk', { scopes: identity.scopes_available.join(', ') || '—' })
      );
    } catch (e) {
      Message.error(knowledgeErrorText(e));
    } finally {
      setTestingId(null);
    }
  };

  const handleDeleteCredential = async (id: ConnectorCredentialId) => {
    try {
      await ipcBridge.knowledge.deleteCredential.invoke({ credential_id: id });
      if (credId === id) setCredId(undefined);
      await refreshCreds();
    } catch (e) {
      Message.error(knowledgeErrorText(e));
    }
  };

  /** Attach a Feishu wiki space as this base's source, then sync immediately. */
  const handleConnect = async () => {
    if (attaching || !credId || !spaceId.trim()) {
      if (!credId || !spaceId.trim()) Message.warning(t('knowledge.connector.attachIncomplete'));
      return;
    }
    setAttaching(true);
    try {
      await ipcBridge.knowledge.setSource.invoke({
        knowledge_base_id: base.knowledge_base_id,
        source: {
          kind: FEISHU,
          mode: 'snapshot',
          entries: [],
          credentialRef: credId,
          scope: { space_id: spaceId.trim() },
        },
      });
      Message.success(t('knowledge.connector.attachOk'));
      onChanged();
      await handleSync();
    } catch (e) {
      Message.error(knowledgeErrorText(e));
    } finally {
      setAttaching(false);
    }
  };

  const handleSync = async () => {
    if (syncing) return;
    setSyncing(true);
    try {
      const summary = await ipcBridge.knowledge.syncSource.invoke({ knowledge_base_id: base.knowledge_base_id });
      notifySourceFetchResult(t, summary, t('knowledge.connector.syncOk', { fetched: summary.fetched }));
      onChanged();
    } catch (e) {
      Message.error(knowledgeErrorText(e));
    } finally {
      setSyncing(false);
    }
  };

  const handleDetach = async () => {
    try {
      await ipcBridge.knowledge.setSource.invoke({ knowledge_base_id: base.knowledge_base_id, source: null });
      Message.success(t('knowledge.connector.detachOk'));
      onChanged();
    } catch (e) {
      Message.error(knowledgeErrorText(e));
    }
  };

  return (
    <Drawer
      width={460}
      title={t('knowledge.connector.title')}
      visible={visible}
      onCancel={onClose}
      onOk={onClose}
      footer={null}
    >
      <div className='flex flex-col gap-20px'>
        {/* Feishu card */}
        <div className='rd-8px border border-solid border-border-2 bg-fill-1 p-14px'>
          <div className='flex items-center gap-8px'>
            <span className='text-15px font-[600] text-t-primary'>{t('knowledge.connector.feishuName')}</span>
            {connected && (
              <Tag size='small' color='green'>
                {t('knowledge.connector.connected')}
              </Tag>
            )}
          </div>
          <div className='mt-4px text-12px text-t-tertiary'>{t('knowledge.connector.feishuDesc')}</div>
        </div>

        {/* Current connection */}
        {connected && (
          <div className='flex flex-col gap-8px rd-8px border border-solid border-border-2 p-14px'>
            <span className='text-13px font-[500] text-t-primary'>{t('knowledge.connector.currentBinding')}</span>
            <span className='text-12px text-t-tertiary break-all'>
              {t('knowledge.connector.spaceLabel')}: {String((source?.scope as { space_id?: string })?.space_id ?? '—')}
            </span>
            <span className='text-12px text-t-tertiary'>
              {source?.lastFetchedAt
                ? t('knowledge.source.lastFetched', { time: new Date(source.lastFetchedAt).toLocaleString() })
                : t('knowledge.source.neverFetched')}
            </span>
            <div className='flex gap-8px'>
              <Button
                size='small'
                type='primary'
                loading={syncing}
                icon={<Refresh theme='outline' size='14' />}
                onClick={() => void handleSync()}
              >
                {t('knowledge.connector.syncNow')}
              </Button>
              <Popconfirm title={t('knowledge.connector.detachConfirm')} onOk={() => void handleDetach()}>
                <Button size='small' status='danger'>
                  {t('knowledge.connector.detach')}
                </Button>
              </Popconfirm>
            </div>
          </div>
        )}

        {/* Credentials */}
        <div className='flex flex-col gap-8px'>
          <span className='text-13px font-[500] text-t-primary'>{t('knowledge.connector.credentials')}</span>
          <Spin loading={credsLoading} className='w-full'>
            {creds.length === 0 ? (
              <Empty description={t('knowledge.connector.noCredentials')} className='!my-12px' />
            ) : (
              <div className='flex flex-col gap-6px'>
                {creds.map((c) => (
                  <div
                    key={c.credentialId}
                    className='flex items-center justify-between gap-8px rd-6px bg-fill-1 px-10px py-6px text-13px'
                  >
                    <span className='truncate text-t-secondary' title={c.name}>
                      {c.name}
                    </span>
                    <span className='flex shrink-0 gap-4px'>
                      <Button size='mini' loading={testingId === c.credentialId} onClick={() => void handleTest(c.credentialId)}>
                        {t('knowledge.connector.test')}
                      </Button>
                      <Popconfirm title={t('knowledge.connector.credDeleteConfirm')} onOk={() => void handleDeleteCredential(c.credentialId)}>
                        <Button size='mini' status='danger' type='text' icon={<Delete theme='outline' size='12' />} />
                      </Popconfirm>
                    </span>
                  </div>
                ))}
              </div>
            )}
          </Spin>

          {/* New credential form */}
          <div className='mt-4px flex flex-col gap-6px rd-8px border border-dashed border-border-2 p-12px'>
            <span className='text-12px text-t-tertiary'>{t('knowledge.connector.addCredential')}</span>
            <Input size='small' placeholder={t('knowledge.connector.credName')} value={name} onChange={setName} />
            <Input size='small' placeholder='App ID' value={appId} onChange={setAppId} />
            <Input.Password size='small' placeholder='App Secret' value={appSecret} onChange={setAppSecret} />
            <Button
              size='small'
              loading={creating}
              icon={<Check theme='outline' size='14' />}
              onClick={() => void handleCreateCredential()}
            >
              {t('knowledge.connector.credSaveAndTest')}
            </Button>
          </div>
        </div>

        {/* Attach */}
        {!connected && (
          <div className='flex flex-col gap-8px rd-8px border border-solid border-border-2 p-14px'>
            <span className='text-13px font-[500] text-t-primary'>{t('knowledge.connector.attachTitle')}</span>
            <Select
              size='small'
              placeholder={t('knowledge.connector.selectCredential')}
              value={credId}
              onChange={setCredId}
              options={creds.map((c) => ({ label: c.name, value: c.credentialId }))}
            />
            <Input
              size='small'
              placeholder={t('knowledge.connector.spacePlaceholder')}
              value={spaceId}
              onChange={setSpaceId}
            />
            <Button
              type='primary'
              size='small'
              loading={attaching || syncing}
              disabled={!credId || !spaceId.trim()}
              onClick={() => void handleConnect()}
            >
              {t('knowledge.connector.connectAndSync')}
            </Button>
          </div>
        )}
      </div>
    </Drawer>
  );
};

export default KnowledgeConnectorDrawer;
