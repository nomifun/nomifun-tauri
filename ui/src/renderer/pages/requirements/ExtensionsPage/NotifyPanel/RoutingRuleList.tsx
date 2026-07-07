import React, { useCallback, useEffect, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Checkbox, Empty, Select, Table, Tag } from '@arco-design/web-react';
import { ipcBridge } from '@/common';
import { isHandledAuthExpiredHttpError } from '@/common/adapter/httpBridge';
import type { ITagSetting, ITagSummary, IWebhook } from '@/common/adapter/ipcBridge';
import { useArcoMessage } from '@/renderer/utils/ui/useArcoMessage';

/** The three notifiable terminal events. Defaults to all three when a tag has
 * no explicit `notify_events` set. Values match the requirement status kinds
 * and the `requirements.status.*` i18n keys. */
const EVENT_KINDS = ['done', 'failed', 'needs_review'] as const;
const DEFAULT_EVENTS: string[] = [...EVENT_KINDS];

type RuleRow = ITagSummary & {
  setting?: ITagSetting;
};

/**
 * ROUTING RULES list — one row per tag, rendered as a readable rule:
 *   {当} [标签=<tag>] {的需求} [events 多选] {→ 通知} [channel 选择]
 *
 * - Channel Select binds the tag → channel via `ipcBridge.webhook.setTagSetting`
 *   ({ tag, updates: { webhook_id } }); a "不通知" clear option sends `null`.
 *   Mirrors autowork/TagSessionTab.handleWebhookChange (the `updates` wrapper is
 *   the real setTagSetting signature).
 * - Events multi-select (Checkbox group) writes `notify_events: string[]`.
 *   Unset = all three. A bound channel with zero events never fires — allowed,
 *   surfaced only as a gentle `eventsRequired` hint, never a hard block.
 * - A disabled greyed "全局兜底规则（即将支持）" row is shown as a forward hint;
 *   global rules are intentionally NOT implemented here.
 */
const RoutingRuleList: React.FC = () => {
  const { t } = useTranslation();
  const [message, ctx] = useArcoMessage();
  const [tags, setTags] = useState<ITagSummary[]>([]);
  const [channels, setChannels] = useState<IWebhook[]>([]);
  const [settings, setSettings] = useState<Record<string, ITagSetting>>({});
  const [loading, setLoading] = useState(false);

  const loadData = useCallback(async () => {
    setLoading(true);
    try {
      // Tags are the spine of this list; the channel list is secondary (only the
      // per-tag picker needs it). Load independently so a transient channel-list
      // failure never blanks the whole panel.
      const [tagList, channelList] = await Promise.all([
        ipcBridge.requirements.tags.invoke(),
        ipcBridge.webhook.list.invoke().catch(() => [] as IWebhook[]),
      ]);
      setTags(tagList);
      setChannels(channelList);

      // No list-all endpoint for tag settings exists — fetch per tag (best-effort).
      const next: Record<string, ITagSetting> = {};
      await Promise.all(
        tagList.map(async (tg) => {
          try {
            next[tg.tag] = await ipcBridge.webhook.getTagSetting.invoke({ tag: tg.tag });
          } catch {
            // A tag may not have a setting yet — that is fine.
          }
        })
      );
      setSettings(next);
    } catch (e) {
      if (isHandledAuthExpiredHttpError(e)) return;
      message.error(String(e));
    } finally {
      setLoading(false);
    }
  }, [message]);

  useEffect(() => {
    void loadData();
  }, [loadData]);

  const handleChannelChange = async (tag: string, webhookId: number | undefined) => {
    try {
      const result = await ipcBridge.webhook.setTagSetting.invoke({
        tag,
        updates: { webhook_id: webhookId ?? null },
      });
      setSettings((prev) => ({ ...prev, [tag]: result }));
      message.success(t('webhook.messages.updateOk'));
    } catch (e) {
      if (isHandledAuthExpiredHttpError(e)) return;
      message.error(String(e));
    }
  };

  const handleEventsChange = async (tag: string, events: string[]) => {
    try {
      const result = await ipcBridge.webhook.setTagSetting.invoke({
        tag,
        updates: { notify_events: events },
      });
      setSettings((prev) => ({ ...prev, [tag]: result }));
      message.success(t('webhook.messages.updateOk'));
    } catch (e) {
      if (isHandledAuthExpiredHttpError(e)) return;
      message.error(String(e));
    }
  };

  // Display the loaded setting's events VERBATIM — the backend's `get_tag_setting`
  // already substitutes the all-three default for tags with no setting row, so an
  // empty array here is an explicit "notify on nothing" choice and must show as
  // zero-checked (and surface the `eventsRequired` hint). Only fall back to the
  // default when no setting object has been loaded for the tag at all.
  const eventsFor = (setting?: ITagSetting): string[] =>
    setting ? setting.notify_events : DEFAULT_EVENTS;

  const tableData: RuleRow[] = tags.map((tg) => ({ ...tg, setting: settings[tg.tag] }));

  const eventOptions = EVENT_KINDS.map((k) => ({
    label: t(`requirements.status.${k}`),
    value: k,
  }));

  const channelOptions = [
    { label: t('requirements.notify.clearChannel'), value: -1 },
    ...channels.map((c) => ({ label: c.name, value: c.id })),
  ];

  const columns = [
    {
      key: 'rule',
      title: t('requirements.notify.rulesTitle'),
      render: (_: unknown, row: RuleRow) => {
        const boundId = row.setting?.webhook_id ?? undefined;
        const selectedEvents = eventsFor(row.setting);
        const noEvents = selectedEvents.length === 0;
        return (
          <div className='flex flex-wrap items-center gap-x-8px gap-y-10px'>
            <span className='text-t-secondary text-13px'>{t('requirements.notify.ruleWhen')}</span>
            <Tag bordered={false} className='!bg-primary-1 !text-primary-6'>
              {row.tag}
            </Tag>
            <span className='text-t-secondary text-13px'>{t('requirements.notify.ruleReq')}</span>

            {/* Events multi-select */}
            <Checkbox.Group
              value={selectedEvents}
              options={eventOptions}
              onChange={(v) => void handleEventsChange(row.tag, v as string[])}
            />

            <span className='text-t-secondary text-13px'>{t('requirements.notify.ruleThen')}</span>

            {/* Channel select (clearChannel option => null bind) */}
            <Select
              size='small'
              placeholder={t('requirements.notify.selectChannel')}
              value={boundId}
              style={{ width: 180 }}
              options={channelOptions}
              onChange={(v) => void handleChannelChange(row.tag, v === -1 ? undefined : (v as number))}
            />

            {/* Gentle hint: bound channel but no events => never fires. */}
            {boundId != null && noEvents ? (
              <Tag size='small' color='orange'>
                {t('requirements.notify.eventsRequired')}
              </Tag>
            ) : null}
          </div>
        );
      },
    },
  ];

  return (
    <div className='flex flex-col gap-12px'>
      {ctx}
      <Table
        rowKey='tag'
        loading={loading}
        columns={columns}
        data={tableData}
        border={{ wrapper: true, cell: false }}
        pagination={false}
        noDataElement={<Empty description={t('autowork.tagSessions.empty')} />}
      />
      {/* Forward-looking, disabled hint row for the future global fallback rule. */}
      <div
        className='flex items-center gap-8px rd-6px px-12px py-10px text-13px text-t-tertiary'
        style={{ background: 'var(--color-fill-1)', opacity: 0.7 }}
      >
        <Tag size='small' color='gray'>
          {t('requirements.notify.ruleAnyTag')}
        </Tag>
        <span>{t('requirements.notify.futureGlobalRule')}</span>
      </div>
    </div>
  );
};

export default RoutingRuleList;
