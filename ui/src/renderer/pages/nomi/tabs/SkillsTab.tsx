/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import React, { useCallback, useEffect, useRef, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Button, Empty, Input, Message, Modal, Pagination, Popconfirm, Select, Spin, Tag, Tooltip } from '@arco-design/web-react';
import { ipcBridge } from '@/common';
import type { ICompanionSkill } from '@/common/adapter/ipcBridge';
import type { useCompanion } from '../useNomi';

interface Props {
  companion: ReturnType<typeof useCompanion>;
}

const STATUS_COLORS: Record<string, string> = { draft: 'orange', active: 'green', archived: 'gray' };

/**
 * 伙伴「技能」Tab —— 看得见 + 能编辑（design §8）。
 * 列出伙伴自进化技能（active/draft/archived + 来源/置信/使用/溯源），支持应用内编辑
 * SKILL.md、审阅草稿（采纳/拒绝），并在伙伴自动学会技能时弹通知。
 */
const SkillsTab: React.FC<Props> = ({ companion }) => {
  const { t } = useTranslation();
  const companionId = companion.profile?.id ?? '';
  const [skills, setSkills] = useState<ICompanionSkill[]>([]);
  const [loading, setLoading] = useState(true);
  const [statusFilter, setStatusFilter] = useState('');
  const [page, setPage] = useState(1);
  const [pageSize, setPageSize] = useState(10);
  const [total, setTotal] = useState(0);

  // In-app SKILL.md editor (Modal).
  const [editName, setEditName] = useState<string | null>(null);
  const [editContent, setEditContent] = useState('');
  const [editDraft, setEditDraft] = useState('');
  const [editMode, setEditMode] = useState(false);
  const [editLoading, setEditLoading] = useState(false);
  const [saving, setSaving] = useState(false);

  // Learn-by-demonstration + gift (T2-B / T3).
  const [teachOpen, setTeachOpen] = useState(false);
  const [teachConv, setTeachConv] = useState('');
  const [giftFor, setGiftFor] = useState<string | null>(null);
  const [giftTarget, setGiftTarget] = useState('');
  const [others, setOthers] = useState<{ id: string; name: string }[]>([]);

  const refreshSeq = useRef(0);

  const refresh = useCallback(async () => {
    if (!companionId) {
      setSkills([]);
      setTotal(0);
      setLoading(false);
      return;
    }
    const seq = ++refreshSeq.current;
    setLoading(true);
    try {
      const result = await ipcBridge.companion.listSkills.invoke({
        companion_id: companionId,
        include_shared: true,
        status: statusFilter || undefined,
        limit: pageSize,
        offset: (page - 1) * pageSize,
      });
      if (seq === refreshSeq.current) {
        const maxPage = Math.max(1, Math.ceil(result.total / pageSize));
        setTotal(result.total);
        if (page > maxPage) {
          setPage(maxPage);
          return;
        }
        setSkills(result.items);
      }
    } catch (e) {
      if (seq === refreshSeq.current) Message.error(String(e));
    } finally {
      if (seq === refreshSeq.current) setLoading(false);
    }
  }, [companionId, page, pageSize, statusFilter]);

  useEffect(() => {
    setPage(1);
  }, [companionId, pageSize, statusFilter]);

  useEffect(() => {
    void refresh();
  }, [refresh]);

  // Live refresh on auto-evolution events for THIS companion; toast on learn.
  useEffect(() => {
    if (!companionId) return;
    const subs = [
      ipcBridge.companion.onSkillDrafted.on((e) => {
        if (e.companion_id === companionId) void refresh();
      }),
      ipcBridge.companion.onSkillLearned.on((e) => {
        if (e.companion_id === companionId) {
          void refresh();
          Message.success(t('nomi.skills.learnedToast', { defaultValue: '伙伴学会了一个新技能' }));
        }
      }),
      ipcBridge.companion.onSkillArchived.on((e) => {
        if (e.companion_id === companionId) void refresh();
      }),
    ];
    return () => subs.forEach((u) => u());
  }, [companionId, refresh, t]);

  const decide = useCallback(
    async (s: ICompanionSkill, accept: boolean) => {
      try {
        await ipcBridge.companion.decideSkill.invoke({ companion_id: companionId, name: s.skill_name, accept });
        void refresh();
      } catch (e) {
        Message.error(String(e));
        void refresh();
      }
    },
    [companionId, refresh]
  );

  const openEditor = useCallback(
    async (s: ICompanionSkill) => {
      setEditName(s.skill_name);
      setEditMode(false);
      setEditLoading(true);
      try {
        const res = await ipcBridge.companion.getSkillContent.invoke({ companion_id: companionId, name: s.skill_name });
        setEditContent(res.content);
        setEditDraft(res.content);
      } catch (e) {
        Message.error(String(e));
        setEditContent('');
        setEditDraft('');
      } finally {
        setEditLoading(false);
      }
    },
    [companionId]
  );

  const save = useCallback(async () => {
    if (!editName) return;
    setSaving(true);
    try {
      await ipcBridge.companion.writeSkillContent.invoke({ companion_id: companionId, name: editName, content: editDraft });
      setEditContent(editDraft);
      setEditMode(false);
      Message.success(t('nomi.skills.saveOk', { defaultValue: '已保存' }));
      void refresh();
    } catch (e) {
      // Backend BadRequest (frontmatter/empty-description) surfaces here.
      Message.error(String(e));
    } finally {
      setSaving(false);
    }
  }, [companionId, editName, editDraft, refresh, t]);

  const statusLabel = (status: string): string => {
    const key = `nomi.skills.status${status.charAt(0).toUpperCase()}${status.slice(1)}`;
    return t(key, { defaultValue: status });
  };

  const teach = useCallback(async () => {
    if (!teachConv.trim()) return;
    try {
      const name = await ipcBridge.companion.draftFromSession.invoke({
        companion_id: companionId,
        conversation_id: teachConv.trim(),
      });
      setTeachOpen(false);
      setTeachConv('');
      void refresh();
      Message.success(
        name
          ? t('nomi.skills.taughtOk', { defaultValue: '已根据示范起草技能，去待审里看看' })
          : t('nomi.skills.taughtNone', { defaultValue: '没能从这个会话提炼出技能' })
      );
    } catch (e) {
      Message.error(String(e));
    }
  }, [companionId, teachConv, refresh, t]);

  const openGift = useCallback(
    async (name: string) => {
      setGiftFor(name);
      setGiftTarget('');
      try {
        const roster = await ipcBridge.companion.listCompanions.invoke();
        setOthers(roster.filter((c) => c.id !== companionId).map((c) => ({ id: c.id, name: c.name })));
      } catch {
        setOthers([]);
      }
    },
    [companionId]
  );

  const gift = useCallback(async () => {
    if (!giftFor || !giftTarget) return;
    try {
      await ipcBridge.companion.giftSkill.invoke({ companion_id: companionId, name: giftFor, to_companion_id: giftTarget });
      setGiftFor(null);
      Message.success(t('nomi.skills.giftedOk', { defaultValue: '已赠送给对方' }));
    } catch (e) {
      Message.error(String(e));
    }
  }, [companionId, giftFor, giftTarget, t]);

  const handlePageChange = useCallback(
    (nextPage: number, nextPageSize: number) => {
      const pageSizeChanged = nextPageSize !== pageSize;
      if (pageSizeChanged) setPageSize(nextPageSize);
      setPage(pageSizeChanged ? 1 : nextPage);
    },
    [pageSize]
  );

  const initialLoading = loading && skills.length === 0 && total === 0;

  return (
    <div className='flex flex-col gap-12px py-8px'>
      <div className='flex gap-8px items-center flex-wrap'>
        <Select
          style={{ width: 130 }}
          value={statusFilter}
          onChange={setStatusFilter}
          placeholder={t('nomi.skills.statusAll', { defaultValue: '全部' })}
        >
          <Select.Option value=''>{t('nomi.skills.statusAll', { defaultValue: '全部' })}</Select.Option>
          <Select.Option value='draft'>{t('nomi.skills.statusDraft', { defaultValue: '待审' })}</Select.Option>
          <Select.Option value='active'>{t('nomi.skills.statusActive', { defaultValue: '已启用' })}</Select.Option>
          <Select.Option value='archived'>{t('nomi.skills.statusArchived', { defaultValue: '已归档' })}</Select.Option>
        </Select>
        <div className='text-12px text-t-tertiary'>
          {t('nomi.skills.hint', { defaultValue: '伙伴从你的重复操作里自动沉淀的技能，可查看、编辑、审阅' })}
        </div>
        <Button size='small' className='ml-auto' onClick={() => setTeachOpen(true)}>
          {t('nomi.skills.teach', { defaultValue: '示范教学' })}
        </Button>
      </div>
      {initialLoading ? (
        <div className='flex justify-center py-40px'>
          <Spin />
        </div>
      ) : skills.length === 0 ? (
        <Empty description={t('nomi.skills.empty', { defaultValue: '还没有技能。多用平台，伙伴会自己学会。' })} />
      ) : (
        <div className='flex flex-col gap-8px transition-opacity duration-150' style={{ opacity: loading ? 0.6 : 1 }}>
          {skills.map((s) => (
            <div
              key={`${s.scope_companion_id}/${s.skill_name}`}
              className='flex items-start gap-10px bg-fill-2 rd-10px px-12px py-10px'
            >
              <Tag color={STATUS_COLORS[s.status] ?? 'gray'}>{statusLabel(s.status)}</Tag>
              <div className='flex-1 min-w-0'>
                <div className='text-13px text-t-primary font-600 break-words'>{s.skill_name}</div>
                {s.description && <div className='text-12px text-t-secondary break-words mt-2px'>{s.description}</div>}
                <div className='mt-4px flex items-center gap-10px text-11px text-t-tertiary flex-wrap'>
                  <span>{t(`nomi.skills.source_${s.source}`, { defaultValue: s.source })}</span>
                  <span>
                    {t('nomi.skills.confidence', { defaultValue: '置信' })} {(s.confidence * 100).toFixed(0)}%
                  </span>
                  <span>
                    {t('nomi.skills.usage', { defaultValue: '已用' })} {s.usage_count}
                  </span>
                  <span>
                    {t('nomi.skills.strength', { defaultValue: '强度' })} {(s.strength * 100).toFixed(0)}%
                  </span>
                  {s.provenance.length > 0 && (
                    <Tooltip content={s.provenance.join(', ')}>
                      <span>
                        {t('nomi.skills.provenance', { defaultValue: '来源' })} {s.provenance.length}
                      </span>
                    </Tooltip>
                  )}
                </div>
              </div>
              <div className='flex items-center gap-4px shrink-0'>
                <Button size='mini' onClick={() => void openEditor(s)}>
                  {t('nomi.skills.view', { defaultValue: '查看' })}
                </Button>
                {s.status === 'active' && (
                  <Button size='mini' onClick={() => void openGift(s.skill_name)}>
                    {t('nomi.skills.gift', { defaultValue: '赠送' })}
                  </Button>
                )}
                {s.status === 'draft' && (
                  <>
                    <Button size='mini' type='primary' onClick={() => void decide(s, true)}>
                      {t('nomi.skills.accept', { defaultValue: '采纳' })}
                    </Button>
                    <Popconfirm
                      title={t('nomi.skills.rejectConfirm', { defaultValue: '拒绝这个技能？' })}
                      onOk={() => void decide(s, false)}
                    >
                      <Button size='mini' status='danger'>
                        {t('nomi.skills.reject', { defaultValue: '拒绝' })}
                      </Button>
                    </Popconfirm>
                  </>
                )}
              </div>
            </div>
          ))}
        </div>
      )}
      {total > 0 && (
        <div className='flex justify-end pt-2px'>
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
        title={editName ?? ''}
        visible={editName !== null}
        onCancel={() => setEditName(null)}
        style={{ width: 720 }}
        footer={
          editMode ? (
            <div className='flex justify-end gap-8px'>
              <Button
                onClick={() => {
                  setEditMode(false);
                  setEditDraft(editContent);
                }}
              >
                {t('nomi.skills.cancel', { defaultValue: '取消' })}
              </Button>
              <Button type='primary' loading={saving} onClick={() => void save()}>
                {t('nomi.skills.save', { defaultValue: '保存' })}
              </Button>
            </div>
          ) : (
            <div className='flex justify-end gap-8px'>
              <Button onClick={() => setEditName(null)}>{t('nomi.skills.close', { defaultValue: '关闭' })}</Button>
              <Button type='primary' onClick={() => setEditMode(true)}>
                {t('nomi.skills.edit', { defaultValue: '编辑' })}
              </Button>
            </div>
          )
        }
      >
        {editLoading ? (
          <div className='flex justify-center py-40px'>
            <Spin />
          </div>
        ) : (
          <Input.TextArea
            value={editMode ? editDraft : editContent}
            onChange={setEditDraft}
            readOnly={!editMode}
            autoSize={{ minRows: 16, maxRows: 36 }}
            className='font-mono text-12px'
          />
        )}
      </Modal>

      <Modal
        title={t('nomi.skills.teach', { defaultValue: '示范教学' })}
        visible={teachOpen}
        onOk={() => void teach()}
        onCancel={() => setTeachOpen(false)}
        okButtonProps={{ disabled: !teachConv.trim() }}
      >
        <div className='flex flex-col gap-8px'>
          <span className='text-12px text-t-secondary'>
            {t('nomi.skills.teachHint', {
              defaultValue: '在一个工作会话里完成一套多步操作后，把它的会话 ID 填到这里，我会据此起草一个技能（进待审）。',
            })}
          </span>
          <Input
            value={teachConv}
            onChange={setTeachConv}
            placeholder={t('nomi.skills.teachPlaceholder', { defaultValue: '会话 ID' })}
          />
        </div>
      </Modal>

      <Modal
        title={t('nomi.skills.gift', { defaultValue: '赠送' })}
        visible={giftFor !== null}
        onOk={() => void gift()}
        onCancel={() => setGiftFor(null)}
        okButtonProps={{ disabled: !giftTarget }}
      >
        <div className='flex flex-col gap-8px'>
          <span className='text-12px text-t-secondary'>
            {t('nomi.skills.giftHint', { defaultValue: '把这个技能赠送给另一个伙伴（对方会获得一份副本）。' })}
          </span>
          <Select
            value={giftTarget || undefined}
            onChange={setGiftTarget}
            placeholder={t('nomi.skills.giftPick', { defaultValue: '选择接收的伙伴' })}
          >
            {others.map((c) => (
              <Select.Option key={c.id} value={c.id}>
                {c.name}
              </Select.Option>
            ))}
          </Select>
        </div>
      </Modal>
    </div>
  );
};

export default SkillsTab;
