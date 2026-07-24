/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import { companion as companionApi, webui } from '@/common/adapter/ipcBridge';
import type { ICompanionWithStatus } from '@/common/adapter/ipcBridge';
import { isBackendHttpError } from '@/common/adapter/httpBridge';
import { useArcoMessage } from '@/renderer/utils/ui/useArcoMessage';
import { Alert, Button, Popconfirm, Select, Spin, Tooltip } from '@arco-design/web-react';
import { Caution, CheckOne, Copy, Delete, Key, Robot } from '@icon-park/react';
import React, { useCallback, useEffect, useMemo, useState } from 'react';
import { useTranslation } from 'react-i18next';
import type { CompanionId } from '@/common/types/ids';

/**
 * CompanionAccessTokenPanel — mint / inspect / revoke a per-companion Remote
 * access token from the MCP capability settings surface.
 *
 * These tokens let an external MCP / REST client (Claude Code, etc.) drive the
 * platform AS the chosen companion. The local-trust-gated backend endpoints
 * (`/api/webui/companions/{id}/access-token`) mint the plaintext exactly ONCE
 * — we mirror the panel's `initialPassword` "treasure shown once" pattern: the
 * value lives only in component state for this session and copies on click.
 *
 * Kept separate from WebUI login controls because these tokens authenticate
 * external MCP / REST clients, not browser login sessions.
 */
const CompanionAccessTokenPanel: React.FC = () => {
  const { t } = useTranslation();
  const [message, messageHolder] = useArcoMessage();

  // Companion roster + the picked companion.
  const [companions, setCompanions] = useState<ICompanionWithStatus[]>([]);
  const [companionsLoading, setCompanionsLoading] = useState(false);
  const [selectedId, setSelectedId] = useState<CompanionId | null>(null);

  // Per-companion token status (configured?), the minted one-time plaintext, the
  // mint-time no-model warning, and in-flight flags for each action.
  const [configured, setConfigured] = useState<boolean | null>(null);
  const [statusLoading, setStatusLoading] = useState(false);
  const [minting, setMinting] = useState(false);
  const [revoking, setRevoking] = useState(false);
  const [plaintext, setPlaintext] = useState<string | null>(null);
  const [warning, setWarning] = useState<string | null>(null);

  const selectedCompanion = useMemo(
    () => companions.find((c) => c.companion_id === selectedId) ?? null,
    [companions, selectedId]
  );

  // Load the companion roster once. Reuse the existing companions-list adapter
  // (no new endpoint) — default the picker to the first companion.
  useEffect(() => {
    let alive = true;
    setCompanionsLoading(true);
    companionApi.listCompanions
      .invoke()
      .then((list) => {
        if (!alive) return;
        setCompanions(list);
        setSelectedId((current) => current ?? list[0]?.companion_id ?? null);
      })
      .catch((error) => {
        console.error('List companions for access token failed:', error);
      })
      .finally(() => {
        if (alive) setCompanionsLoading(false);
      });
    return () => {
      alive = false;
    };
  }, []);

  // Refresh the token status whenever the selected companion changes. The
  // one-time plaintext + warning are session/companion-scoped — clear them so a
  // stale token never trails onto a different companion.
  useEffect(() => {
    setPlaintext(null);
    setWarning(null);
    setConfigured(null);
    if (!selectedId) return;
    let alive = true;
    setStatusLoading(true);
    webui.companionAccessToken.status
      .invoke({ companionId: selectedId })
      .then((res) => {
        if (alive) setConfigured(res.configured);
      })
      .catch((error) => {
        console.error('Companion access-token status failed:', error);
        if (alive) setConfigured(null);
      })
      .finally(() => {
        if (alive) setStatusLoading(false);
      });
    return () => {
      alive = false;
    };
  }, [selectedId]);

  const friendlyError = useCallback(
    (error: unknown, fallback: string) =>
      isBackendHttpError(error) && error.backendMessage ? error.backendMessage : fallback,
    []
  );

  const handleCopy = useCallback(
    (text: string) => {
      void navigator.clipboard.writeText(text);
      message.success(t('common.copySuccess'));
    },
    [message, t]
  );

  const handleMint = useCallback(async () => {
    if (!selectedId) return;
    setMinting(true);
    try {
      const res = await webui.companionAccessToken.mint.invoke({ companionId: selectedId });
      // Plaintext is returned exactly once — keep it only in session state.
      setPlaintext(res.token);
      setWarning(res.warning ?? null);
      setConfigured(true);
      message.success(t('settings.webui.companionToken.minted'));
    } catch (error) {
      console.error('Mint companion access token failed:', error);
      message.error(friendlyError(error, t('settings.webui.companionToken.mintFailed')));
    } finally {
      setMinting(false);
    }
  }, [selectedId, message, t, friendlyError]);

  const handleRevoke = useCallback(async () => {
    if (!selectedId) return;
    setRevoking(true);
    try {
      await webui.companionAccessToken.revoke.invoke({ companionId: selectedId });
      setConfigured(false);
      setPlaintext(null);
      setWarning(null);
      message.success(t('settings.webui.companionToken.revoked'));
    } catch (error) {
      console.error('Revoke companion access token failed:', error);
      message.error(friendlyError(error, t('settings.webui.companionToken.revokeFailed')));
    } finally {
      setRevoking(false);
    }
  }, [selectedId, message, t, friendlyError]);

  const hasCompanions = companions.length > 0;

  return (
    <div className='flex flex-col gap-8px'>
      {messageHolder}

      {/* 标题 / Section header */}
      <div className='flex items-center gap-6px px-2px'>
        <Key theme='outline' size='14' className='text-[rgb(var(--primary-6))] shrink-0' />
        <span className='text-12px font-500 text-t-tertiary'>{t('settings.webui.companionToken.title')}</span>
      </div>
      <div className='text-12px text-t-tertiary px-2px leading-relaxed'>
        {t('settings.webui.companionToken.desc')}
      </div>

      {!hasCompanions ? (
        <div className='rd-10px border border-line bg-fill-1 px-10px py-8px text-12px text-t-tertiary leading-relaxed'>
          {companionsLoading ? t('common.loading') : t('settings.webui.companionToken.noCompanions')}
        </div>
      ) : (
        <div className='flex flex-col gap-8px'>
          {/* 伙伴选择 + 状态徽标 / Companion picker + status badge */}
          <div className='flex items-center gap-8px'>
            <Select
              size='small'
              className='flex-1 min-w-0'
              value={selectedId ?? undefined}
              loading={companionsLoading}
              onChange={(value) => setSelectedId(value)}
              placeholder={t('settings.webui.companionToken.selectPlaceholder')}
            >
              {companions.map((c) => (
                <Select.Option key={c.companion_id} value={c.companion_id}>
                  <span className='inline-flex items-center gap-6px'>
                    <Robot theme='outline' size='13' className='text-t-tertiary shrink-0' />
                    <span className='truncate'>{c.name}</span>
                  </span>
                </Select.Option>
              ))}
            </Select>
            <div className='shrink-0'>
              {statusLoading ? (
                <Spin size={14} />
              ) : configured ? (
                <span className='inline-flex items-center gap-3px text-12px text-success whitespace-nowrap'>
                  <CheckOne theme='filled' size='13' fill='currentColor' />
                  {t('settings.webui.companionToken.statusActive')}
                </span>
              ) : (
                <span className='text-12px text-t-tertiary whitespace-nowrap'>
                  {t('settings.webui.companionToken.statusNone')}
                </span>
              )}
            </div>
          </div>

          {/* 一次性明文令牌块（仅本次铸造可见，镜像初始密码块）/ One-time plaintext token */}
          {plaintext && (
            <div className='flex flex-col gap-4px'>
              <div className='inline-flex items-center gap-8px rd-100px border border-[rgb(var(--primary-6))]/40 bg-[rgb(var(--primary-1))] px-10px py-4px min-w-0'>
                <Key theme='outline' size='14' className='text-[rgb(var(--primary-6))] shrink-0' />
                <span className='text-13px text-t-primary font-mono truncate flex-1'>{plaintext}</span>
                <Tooltip content={t('common.copy')}>
                  <Button
                    type='text'
                    size='mini'
                    className='rd-100px !px-6px inline-flex items-center !h-24px'
                    onClick={() => handleCopy(plaintext)}
                  >
                    <Copy size={14} />
                  </Button>
                </Tooltip>
              </div>
              <div className='text-12px text-warning px-2px leading-relaxed'>
                {t('settings.webui.companionToken.shownOnceHint')}
              </div>
            </div>
          )}

          {/* 无模型告警 / No-model warning */}
          {warning && (
            <Alert
              type='warning'
              showIcon
              icon={<Caution theme='outline' size='15' fill='currentColor' />}
              content={<span className='text-12px leading-relaxed'>{warning}</span>}
              className='!rd-10px !py-6px'
            />
          )}

          {/* 操作区：生成 / 吊销 / Actions: mint / revoke */}
          <div className='flex items-center gap-8px'>
            <Button
              type='primary'
              size='small'
              long
              loading={minting}
              disabled={!selectedId || revoking}
              onClick={() => void handleMint()}
            >
              <span className='inline-flex items-center gap-4px'>
                <Key theme='outline' size='14' fill='currentColor' />
                {configured
                  ? t('settings.webui.companionToken.regenerate')
                  : t('settings.webui.companionToken.generate')}
              </span>
            </Button>
            {configured && (
              <Popconfirm
                position='top'
                title={t('settings.webui.companionToken.revokeConfirm')}
                okText={t('settings.webui.companionToken.revoke')}
                cancelText={t('common.cancel')}
                onOk={() => void handleRevoke()}
              >
                <Button
                  type='outline'
                  status='danger'
                  size='small'
                  loading={revoking}
                  disabled={minting}
                >
                  <span className='inline-flex items-center gap-4px'>
                    <Delete theme='outline' size='14' fill='currentColor' />
                    {t('settings.webui.companionToken.revoke')}
                  </span>
                </Button>
              </Popconfirm>
            )}
          </div>

          {selectedCompanion && !selectedCompanion.status.model_configured && !warning && configured === false && (
            <div className='text-12px text-t-tertiary px-2px leading-relaxed'>
              {t('settings.webui.companionToken.noModelHint')}
            </div>
          )}
        </div>
      )}
    </div>
  );
};

export default CompanionAccessTokenPanel;
