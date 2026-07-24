/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import React, { useEffect, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Button, Popconfirm, Spin } from '@arco-design/web-react';
import { Refresh } from '@icon-park/react';
import { ipcBridge } from '@/common';
import type { IIdmmIntervention } from '@/common/adapter/ipcBridge';
import { useArcoMessage } from '@/renderer/utils/ui/useArcoMessage';
import IdmmInterventionRow from '@/renderer/pages/conversation/components/IdmmInterventionRow';

const ACTIVITY_LIMIT = 50;

/**
 * Global IDMM activity overview. Lists the most recent CROSS-SESSION
 * interventions (all conversation/terminal targets, most-recent-first) so the
 * user can audit decision-making behaviour in one place — the per-session
 * timeline lives in `IdmmControl`'s popover. Rows reuse the shared
 * `IdmmInterventionRow` so both surfaces stay identical.
 *
 * The backing records honour the same aggressive eviction the per-target log
 * does (per-target cap / TTL / global cap), so this is a recent window, not a
 * full history. WS-pushed interventions prepend live.
 */
const IdmmActivityContent: React.FC = () => {
  const { t } = useTranslation();
  const [message, messageContext] = useArcoMessage();
  const [rows, setRows] = useState<IIdmmIntervention[]>([]);
  const [loading, setLoading] = useState(true);
  const [clearing, setClearing] = useState(false);

  const load = React.useCallback(async () => {
    setLoading(true);
    try {
      const data = await ipcBridge.idmm.getActivity.invoke({ limit: ACTIVITY_LIMIT });
      setRows(data);
    } catch {
      /* ignore — 决策活动是非关键审计数据 */
    } finally {
      setLoading(false);
    }
  }, []);

  useEffect(() => {
    void load();
  }, [load]);

  // 实时 prepend:任意 target 的新介入都插到列表最前(窗口封顶 ACTIVITY_LIMIT)。
  useEffect(() => {
    const unsub = ipcBridge.idmm.onIntervention.on((rec) => {
      setRows((prev) => [rec, ...prev].slice(0, ACTIVITY_LIMIT));
    });
    return () => unsub();
  }, []);

  const clearAll = async () => {
    setClearing(true);
    try {
      await ipcBridge.idmm.clearActivity.invoke();
      setRows([]);
      message.success(t('idmm.activity.clearOk'));
    } catch (e) {
      message.error(String(e));
    } finally {
      setClearing(false);
    }
  };

  return (
    <div className='flex flex-col gap-12px'>
      {messageContext}
      <div className='flex items-start justify-between gap-12px'>
        <div className='flex flex-col gap-2px'>
          <span className='text-t-primary text-14px font-600'>{t('idmm.activity.title')}</span>
          <span className='text-t-tertiary text-12px leading-18px'>{t('idmm.activity.desc')}</span>
        </div>
        <div className='flex items-center gap-8px shrink-0'>
          <Button
            size='small'
            icon={<Refresh theme='outline' size='14' fill='currentColor' />}
            loading={loading}
            onClick={() => void load()}
          >
            {t('idmm.activity.refresh')}
          </Button>
          <Popconfirm title={t('idmm.activity.clearConfirm')} onOk={() => void clearAll()}>
            <Button size='small' status='danger' disabled={clearing || rows.length === 0}>
              {t('idmm.activity.clearAll')}
            </Button>
          </Popconfirm>
        </div>
      </div>

      {loading && rows.length === 0 ? (
        <div className='flex items-center justify-center py-40px'>
          <Spin />
        </div>
      ) : rows.length === 0 ? (
        <div className='py-40px text-center text-t-tertiary text-12px'>{t('idmm.activity.empty')}</div>
      ) : (
        <div className='flex flex-col gap-6px'>
          {rows.map((rec) => (
            <IdmmInterventionRow key={rec.intervention_id} rec={rec} />
          ))}
        </div>
      )}
    </div>
  );
};

export default IdmmActivityContent;
