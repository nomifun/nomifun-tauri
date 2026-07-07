import React, { useCallback, useEffect, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Button, Empty, Popconfirm, Switch, Table, Tag } from '@arco-design/web-react';
import { useContainerWidth } from '@renderer/hooks/ui/useContainerWidth';
import { ipcBridge } from '@/common';
import { isHandledAuthExpiredHttpError } from '@/common/adapter/httpBridge';
import type { IWebhook } from '@/common/adapter/ipcBridge';
import { useArcoMessage } from '@/renderer/utils/ui/useArcoMessage';
import ChannelFormModal from './ChannelFormModal';

/**
 * Notification CHANNELS list (webhook endpoints CRUD).
 *
 * Ported from settings/webhook/WebhookManager and beautified for the unified
 * Notify panel: it shares the same ipc surface (`ipcBridge.webhook.*`) but
 * uses the project `useArcoMessage` wrapper (not the global Message) and opens
 * the sibling `ChannelFormModal` for create / edit.
 *
 * Columns: name · platform (localized) · url · enabled · has_secret · actions.
 * Actions: edit / test / delete (with Popconfirm). The list refreshes after
 * every successful create / edit / test / delete.
 */
const ChannelList: React.FC = () => {
  const { t } = useTranslation();
  const [message, ctx] = useArcoMessage();
  const { ref, width } = useContainerWidth<HTMLDivElement>();
  const [channels, setChannels] = useState<IWebhook[]>([]);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [modalVisible, setModalVisible] = useState(false);
  const [editing, setEditing] = useState<IWebhook | null>(null);

  const loadChannels = useCallback(async () => {
    setLoading(true);
    setError(null);
    try {
      const list = await ipcBridge.webhook.list.invoke();
      setChannels(list);
    } catch (e) {
      if (isHandledAuthExpiredHttpError(e)) return;
      setError(String(e));
    } finally {
      setLoading(false);
    }
  }, []);

  useEffect(() => {
    void loadChannels();
  }, [loadChannels]);

  const handleTest = async (id: number) => {
    try {
      await ipcBridge.webhook.test.invoke({ id });
      message.success(t('webhook.messages.testOk'));
    } catch (e) {
      if (isHandledAuthExpiredHttpError(e)) return;
      message.error(t('webhook.messages.testError', { error: String(e) }));
    }
  };

  const handleDelete = async (id: number) => {
    try {
      await ipcBridge.webhook.remove.invoke({ id });
      message.success(t('webhook.messages.deleteOk'));
      void loadChannels();
    } catch (e) {
      if (isHandledAuthExpiredHttpError(e)) return;
      message.error(String(e));
    }
  };

  const handleEdit = (channel: IWebhook) => {
    setEditing(channel);
    setModalVisible(true);
  };

  const handleCreate = () => {
    setEditing(null);
    setModalVisible(true);
  };

  const handleModalClose = () => {
    setModalVisible(false);
    setEditing(null);
  };

  const handleModalSuccess = () => {
    handleModalClose();
    void loadChannels();
  };

  const columns = [
    {
      key: 'name',
      title: t('webhook.columns.name'),
      dataIndex: 'name',
      width: 160,
    },
    {
      key: 'platform',
      title: t('webhook.columns.platform'),
      dataIndex: 'platform',
      width: 120,
      render: (v: string) => (
        <Tag bordered={false} className='!bg-primary-1 !text-primary-6'>
          {t(`webhook.platform.${v}`)}
        </Tag>
      ),
    },
    {
      key: 'url',
      title: t('webhook.columns.url'),
      dataIndex: 'url',
      ellipsis: true,
    },
    {
      key: 'enabled',
      title: t('webhook.columns.enabled'),
      dataIndex: 'enabled',
      width: 80,
      render: (v: boolean) => <Switch size='small' checked={v} disabled />,
    },
    {
      key: 'has_secret',
      title: t('webhook.columns.secret'),
      dataIndex: 'has_secret',
      width: 100,
      render: (v: boolean) => (
        <Tag color={v ? 'green' : 'gray'}>
          {v ? t('webhook.secret.configured') : t('webhook.secret.notConfigured')}
        </Tag>
      ),
    },
    {
      key: 'actions',
      title: t('webhook.columns.actions'),
      width: 180,
      render: (_: unknown, row: IWebhook) => (
        <div className='flex gap-8px'>
          <Button size='mini' onClick={() => handleEdit(row)}>
            {t('webhook.actions.edit')}
          </Button>
          <Button size='mini' onClick={() => void handleTest(row.id)}>
            {t('webhook.actions.test')}
          </Button>
          <Popconfirm title={t('webhook.actions.deleteConfirm')} onOk={() => void handleDelete(row.id)}>
            <Button size='mini' status='danger'>
              {t('webhook.actions.delete')}
            </Button>
          </Popconfirm>
        </div>
      ),
    },
  ];

  // Hide secondary columns on narrow content widths; `tableScrollX` is the
  // horizontal-scroll fallback so the list is never clipped. width === 0 is the
  // first (unmeasured) frame — show all columns to avoid a flash.
  const hiddenColumnKeys = new Set<string>();
  if (width > 0 && width < 760) {
    hiddenColumnKeys.add('has_secret');
  }
  if (width > 0 && width < 600) {
    hiddenColumnKeys.add('platform');
  }
  const visibleColumns = columns.filter((c) => !hiddenColumnKeys.has(c.key));
  const tableScrollX = visibleColumns.reduce((sum, c) => sum + ((c as { width?: number }).width ?? 0), 0) + 180;

  if (error) {
    return (
      <div className='flex flex-col items-start gap-12px'>
        {ctx}
        <div className='text-t-secondary'>{t('webhook.messages.loadError')}</div>
        <Button onClick={() => void loadChannels()}>{t('requirements.retry')}</Button>
      </div>
    );
  }

  return (
    <div ref={ref} className='flex flex-col gap-12px'>
      {ctx}
      <div className='flex w-full flex-wrap items-center justify-end gap-8px'>
        <Button type='primary' onClick={handleCreate}>
          {t('requirements.notify.newChannel')}
        </Button>
      </div>
      <Table
        rowKey='id'
        loading={loading}
        columns={visibleColumns}
        data={channels}
        scroll={{ x: tableScrollX }}
        border={{ wrapper: true, cell: false }}
        pagination={false}
        noDataElement={<Empty description={t('webhook.empty')} />}
      />
      <ChannelFormModal
        visible={modalVisible}
        editing={editing}
        onClose={handleModalClose}
        onSuccess={handleModalSuccess}
      />
    </div>
  );
};

export default ChannelList;
