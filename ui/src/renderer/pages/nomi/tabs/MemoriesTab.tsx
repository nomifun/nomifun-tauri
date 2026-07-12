/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import React, { useCallback, useEffect, useRef, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Button, Dropdown, Empty, Input, Menu, Message, Modal, Pagination, Radio, Select, Spin, Tag, Tooltip } from '@arco-design/web-react';
import { More, Pin } from '@icon-park/react';
import { ipcBridge } from '@/common';
import type { ICompanionMemory } from '@/common/adapter/ipcBridge';

const KINDS = ['profile', 'preference', 'knowledge', 'episode', 'task', 'affective'] as const;

const KIND_COLORS: Record<string, string> = {
  profile: 'gray',
  preference: 'pinkpurple',
  knowledge: 'green',
  episode: 'orange',
  task: 'red',
  affective: 'purple',
};

type ScopeKind = 'user' | 'companion';

interface CompanionRef {
  id: string;
  name: string;
}

interface MemoriesTabProps {
  /** The companion currently selected on the nomi page; scopes the default view. */
  companionId?: string | null;
  /** Roster, for the scope selector + per-row owner badges. */
  companions?: CompanionRef[];
}

const MemoriesTab: React.FC<MemoriesTabProps> = ({ companionId = null, companions = [] }) => {
  const { t } = useTranslation();
  const [memories, setMemories] = useState<ICompanionMemory[]>([]);
  const [loading, setLoading] = useState(true);
  const [kind, setKind] = useState<string>('');
  const [q, setQ] = useState('');
  const [memStatus, setMemStatus] = useState('active');
  const [page, setPage] = useState(1);
  const [pageSize, setPageSize] = useState(10);
  const [total, setTotal] = useState(0);
  // 'self' = shared + this companion's private (default when a companion is
  // selected); 'all' = every companion's memories (cross-companion view).
  const [scopeMode, setScopeMode] = useState<'self' | 'all'>(companionId ? 'self' : 'all');

  const [addVisible, setAddVisible] = useState(false);
  const [addKind, setAddKind] = useState<string>('knowledge');
  const [addContent, setAddContent] = useState('');
  const [addScopeKind, setAddScopeKind] = useState<ScopeKind>(companionId ? 'companion' : 'user');
  const [addScopeCompanionId, setAddScopeCompanionId] = useState<string>(companionId ?? '');

  const [editTarget, setEditTarget] = useState<ICompanionMemory | null>(null);
  const [editContent, setEditContent] = useState('');
  const [editScopeKind, setEditScopeKind] = useState<ScopeKind>('user');
  const [editScopeCompanionId, setEditScopeCompanionId] = useState<string>('');
  const [deleteTarget, setDeleteTarget] = useState<ICompanionMemory | null>(null);

  const companionName = useCallback(
    (id: string) => companions.find((c) => c.id === id)?.name || id,
    [companions]
  );

  const refreshSeq = useRef(0);

  const refresh = useCallback(async () => {
    const seq = ++refreshSeq.current;
    setLoading(true);
    try {
      const result = await ipcBridge.companion.listMemories.invoke({
        kind: kind || undefined,
        q: q || undefined,
        status: memStatus,
        // 'self' scopes to shared + selected companion's private; 'all' omits
        // the filter so every companion's memories show.
        scope_companion_id: scopeMode === 'self' && companionId ? companionId : undefined,
        limit: pageSize,
        offset: (page - 1) * pageSize,
      });
      // Out-of-order guard: a slow stale response must not clobber the
      // results of a newer query (rapid typing fires overlapping requests).
      if (seq === refreshSeq.current) {
        const maxPage = Math.max(1, Math.ceil(result.total / pageSize));
        setTotal(result.total);
        // A deletion can leave the current page past the end. Keep the existing
        // rows visible while the next request loads the last valid page.
        if (page > maxPage) {
          setPage(maxPage);
          return;
        }
        setMemories(result.items);
      }
    } catch (e) {
      if (seq === refreshSeq.current) Message.error(String(e));
    } finally {
      if (seq === refreshSeq.current) setLoading(false);
    }
  }, [kind, q, memStatus, scopeMode, companionId, page, pageSize]);

  // Debounce refetches slightly so typing does not create overlapping requests.
  useEffect(() => {
    const timer = setTimeout(() => void refresh(), 250);
    return () => clearTimeout(timer);
  }, [refresh]);

  // A new result set always begins at its first page. Page navigation itself
  // changes only `page`, so it keeps the current filters intact.
  useEffect(() => {
    setPage(1);
  }, [kind, q, memStatus, scopeMode, companionId, pageSize]);

  // nomi can save/edit/delete memories mid-chat or from another surface —
  // reflect them live.
  useEffect(() => {
    const unsubs = [
      ipcBridge.companion.onMemoryCreated.on(() => void refresh()),
      ipcBridge.companion.onMemoryUpdated.on(() => void refresh()),
      ipcBridge.companion.onMemoryDeleted.on(() => void refresh()),
    ];
    return () => unsubs.forEach((u) => u());
  }, [refresh]);

  const togglePin = useCallback(
    async (m: ICompanionMemory) => {
      await ipcBridge.companion.updateMemory.invoke({ id: m.id, pinned: !m.pinned });
      void refresh();
    },
    [refresh]
  );

  const toggleArchive = useCallback(
    async (m: ICompanionMemory) => {
      await ipcBridge.companion.updateMemory.invoke({ id: m.id, status: m.status === 'active' ? 'archived' : 'active' });
      void refresh();
    },
    [refresh]
  );

  const remove = useCallback(
    async (m: ICompanionMemory) => {
      await ipcBridge.companion.deleteMemory.invoke({ id: m.id });
      void refresh();
    },
    [refresh]
  );

  const confirmRemove = useCallback(async () => {
    if (!deleteTarget) return;
    await remove(deleteTarget);
    setDeleteTarget(null);
  }, [deleteTarget, remove]);

  const openAdd = useCallback(() => {
    setAddKind('knowledge');
    setAddContent('');
    setAddScopeKind(companionId ? 'companion' : 'user');
    setAddScopeCompanionId(companionId ?? '');
    setAddVisible(true);
  }, [companionId]);

  const add = useCallback(async () => {
    if (!addContent.trim()) return;
    try {
      await ipcBridge.companion.addMemory.invoke({
        kind: addKind,
        content: addContent.trim(),
        // '' (or omitted) = shared; a companion id = private to it.
        scope_companion_id: addScopeKind === 'companion' ? addScopeCompanionId : '',
      });
      setAddVisible(false);
      setAddContent('');
      void refresh();
      Message.success(t('nomi.memories.added'));
    } catch (e) {
      Message.error(String(e));
    }
  }, [addKind, addContent, addScopeKind, addScopeCompanionId, refresh, t]);

  const openEdit = useCallback((m: ICompanionMemory) => {
    setEditTarget(m);
    setEditContent(m.content);
    setEditScopeKind(m.scope_kind === 'companion' ? 'companion' : 'user');
    setEditScopeCompanionId(m.scope_companion_id || '');
  }, []);

  const saveEdit = useCallback(async () => {
    if (!editTarget || !editContent.trim()) return;
    try {
      await ipcBridge.companion.updateMemory.invoke({
        id: editTarget.id,
        content: editContent.trim(),
        scope_kind: editScopeKind,
        scope_companion_id: editScopeKind === 'companion' ? editScopeCompanionId : '',
      });
      setEditTarget(null);
      void refresh();
      Message.success(t('nomi.memories.saved'));
    } catch (e) {
      Message.error(String(e));
    }
  }, [editTarget, editContent, editScopeKind, editScopeCompanionId, refresh, t]);

  // A private scope requires a chosen companion; disable the OK button otherwise.
  const addInvalid = !addContent.trim() || (addScopeKind === 'companion' && !addScopeCompanionId);
  const editInvalid = !editContent.trim() || (editScopeKind === 'companion' && !editScopeCompanionId);

  const scopeSelector = (
    scopeKind: ScopeKind,
    scopeCompanionId: string,
    setScopeKind: (k: ScopeKind) => void,
    setScopeCompanionId: (id: string) => void
  ) => (
    <div className='flex items-center gap-8px flex-wrap'>
      <Radio.Group
        type='button'
        size='small'
        value={scopeKind}
        onChange={(v: ScopeKind) => {
          setScopeKind(v);
          if (v === 'companion' && !scopeCompanionId && companionId) setScopeCompanionId(companionId);
        }}
      >
        <Radio value='user'>{t('nomi.memories.scopeShared')}</Radio>
        <Radio value='companion'>{t('nomi.memories.scopePrivate')}</Radio>
      </Radio.Group>
      {scopeKind === 'companion' && (
        <Select
          size='small'
          style={{ width: 180 }}
          value={scopeCompanionId || undefined}
          onChange={setScopeCompanionId}
          placeholder={t('nomi.memories.scopePickCompanion')}
        >
          {companions.map((c) => (
            <Select.Option key={c.id} value={c.id}>
              {c.name || c.id}
            </Select.Option>
          ))}
        </Select>
      )}
    </div>
  );

  const scopeBadge = (m: ICompanionMemory) =>
    m.scope_kind === 'companion' ? (
      <Tag color='arcoblue' bordered>
        {t('nomi.memories.scopePrivateOf', { name: companionName(m.scope_companion_id) })}
      </Tag>
    ) : (
      <Tag bordered>{t('nomi.memories.scopeShared')}</Tag>
    );

  const memoryActionMenu = (m: ICompanionMemory) => (
    <Menu
      onClickMenuItem={(key) => {
        if (key === 'edit') {
          openEdit(m);
          return;
        }
        if (key === 'archive') {
          void toggleArchive(m);
          return;
        }
        if (key === 'delete') setDeleteTarget(m);
      }}
    >
      <Menu.Item key='edit'>{t('nomi.memories.edit')}</Menu.Item>
      <Menu.Item key='archive'>{m.status === 'active' ? t('nomi.memories.archive') : t('nomi.memories.restore')}</Menu.Item>
      <Menu.Item key='delete' className='!text-[rgb(var(--danger-6))]'>
        {t('nomi.memories.delete')}
      </Menu.Item>
    </Menu>
  );

  const handlePageChange = useCallback(
    (nextPage: number, nextPageSize: number) => {
      const pageSizeChanged = nextPageSize !== pageSize;
      if (pageSizeChanged) setPageSize(nextPageSize);
      setPage(pageSizeChanged ? 1 : nextPage);
    },
    [pageSize]
  );

  const initialLoading = loading && memories.length === 0 && total === 0;

  return (
    <div className='flex flex-col gap-12px py-8px'>
      <div className='flex gap-8px flex-wrap items-center'>
        <Select style={{ width: 140 }} value={kind} onChange={setKind} placeholder={t('nomi.memories.kindAll')}>
          <Select.Option value=''>{t('nomi.memories.kindAll')}</Select.Option>
          {KINDS.map((k) => (
            <Select.Option key={k} value={k}>
              {t(`nomi.kinds.${k}`)}
            </Select.Option>
          ))}
        </Select>
        <Select style={{ width: 110 }} value={memStatus} onChange={setMemStatus}>
          <Select.Option value='active'>{t('nomi.memories.statusActive')}</Select.Option>
          <Select.Option value='archived'>{t('nomi.memories.statusArchived')}</Select.Option>
        </Select>
        {companionId && (
          <Radio.Group type='button' size='small' value={scopeMode} onChange={(v: 'self' | 'all') => setScopeMode(v)}>
            <Radio value='self'>{t('nomi.memories.scopeFilterSelf')}</Radio>
            <Radio value='all'>{t('nomi.memories.scopeFilterAll')}</Radio>
          </Radio.Group>
        )}
        <Input.Search
          style={{ width: 220 }}
          placeholder={t('nomi.memories.searchPlaceholder')}
          value={q}
          onChange={setQ}
          allowClear
        />
        <Button type='primary' onClick={openAdd}>
          {t('nomi.memories.add')}
        </Button>
      </div>
      {initialLoading ? (
        <div className='flex justify-center py-40px'>
          <Spin />
        </div>
      ) : memories.length === 0 ? (
        <Empty description={t('nomi.memories.empty')} />
      ) : (
        <div className='flex flex-col gap-8px transition-opacity duration-150' style={{ opacity: loading ? 0.6 : 1 }}>
          {memories.map((m) => (
            <div
              key={m.id}
              className='group flex items-start gap-10px rounded-12px border border-solid border-[var(--color-border-2)] bg-[var(--color-bg-2)] px-12px py-10px transition-colors hover:bg-fill-2'
            >
              <Tag color={KIND_COLORS[m.kind]}>{t(`nomi.kinds.${m.kind}`)}</Tag>
              <div className='flex-1 min-w-0'>
                <div className='line-clamp-2 text-13px leading-20px text-t-primary break-words'>{m.content}</div>
                <div className='mt-5px flex flex-wrap items-center gap-x-10px gap-y-4px text-11px text-t-tertiary'>
                  {scopeBadge(m)}
                  <span>
                    {t('nomi.memories.strength')} {(m.strength * 100).toFixed(0)}%
                  </span>
                  <span>{new Date(m.updated_at).toLocaleString()}</span>
                  {m.source !== 'learn' && <span>{t(`nomi.memories.source_${m.source}`, m.source)}</span>}
                </div>
              </div>
              <div className='flex items-center gap-4px shrink-0'>
                <Tooltip content={m.pinned ? t('nomi.memories.unpin') : t('nomi.memories.pin')}>
                  <Button
                    size='mini'
                    type={m.pinned ? 'primary' : 'secondary'}
                    icon={<Pin theme='outline' size='12' />}
                    onClick={() => void togglePin(m)}
                  />
                </Tooltip>
                <Dropdown droplist={memoryActionMenu(m)} trigger='click' position='br' getPopupContainer={() => document.body}>
                  <Tooltip content={t('nomi.memories.more')}>
                    <Button size='mini' type='text' icon={<More theme='outline' size='14' />} aria-label={t('nomi.memories.more')} />
                  </Tooltip>
                </Dropdown>
              </div>
            </div>
          ))}
        </div>
      )}

      {total > 0 && (
        <div className='flex flex-wrap items-center justify-between gap-10px pt-2px'>
          <span className='text-12px text-t-tertiary tabular-nums'>{t('nomi.memories.total', { count: total })}</span>
          <Pagination
            current={page}
            pageSize={pageSize}
            total={total}
            showTotal
            sizeCanChange
            sizeOptions={[10, 20, 50]}
            showJumper={total > pageSize}
            onChange={handlePageChange}
          />
        </div>
      )}

      <Modal
        title={t('nomi.memories.add')}
        visible={addVisible}
        onOk={() => void add()}
        onCancel={() => setAddVisible(false)}
        okButtonProps={{ disabled: addInvalid }}
      >
        <div className='flex flex-col gap-12px'>
          <Select value={addKind} onChange={setAddKind}>
            {KINDS.map((k) => (
              <Select.Option key={k} value={k}>
                {t(`nomi.kinds.${k}`)}
              </Select.Option>
            ))}
          </Select>
          {scopeSelector(addScopeKind, addScopeCompanionId, setAddScopeKind, setAddScopeCompanionId)}
          <Input.TextArea
            rows={4}
            value={addContent}
            onChange={setAddContent}
            placeholder={t('nomi.memories.addPlaceholder')}
          />
        </div>
      </Modal>

      <Modal
        title={t('nomi.memories.edit')}
        visible={!!editTarget}
        onOk={() => void saveEdit()}
        onCancel={() => setEditTarget(null)}
        okButtonProps={{ disabled: editInvalid }}
      >
        <div className='flex flex-col gap-12px'>
          {scopeSelector(editScopeKind, editScopeCompanionId, setEditScopeKind, setEditScopeCompanionId)}
          <Input.TextArea rows={5} value={editContent} onChange={setEditContent} />
          <div className='text-11px text-t-tertiary'>{t('nomi.memories.editHint')}</div>
        </div>
      </Modal>

      <Modal
        title={t('nomi.memories.delete')}
        visible={!!deleteTarget}
        onOk={() => void confirmRemove()}
        onCancel={() => setDeleteTarget(null)}
        okButtonProps={{ status: 'danger' }}
      >
        {t('nomi.memories.deleteConfirm')}
      </Modal>
    </div>
  );
};

export default MemoriesTab;
