/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import React, { useCallback, useEffect, useRef, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Alert, Button, Message, Modal, Progress, Slider, Spin, Switch, Tag } from '@arco-design/web-react';
import { IconEdit } from '@arco-design/web-react/icon';
import { ipcBridge } from '@/common';
import type { ICompanionLearnRun, ICompanionWeeklyDigest } from '@/common/adapter/ipcBridge';
import CompanionAvatar from '@renderer/pages/companion/CompanionAvatar';
import { customFigureMetaOf } from '@renderer/pages/companion/characters/customMeta';
import { FIGURE_HEIGHTS, SIZE_MIN, SIZE_MAX } from '@renderer/pages/companion/characters/customDesk';
import { CUSTOM_CHARACTER_ID } from '@renderer/pages/companion/characters';
import type { CompanionMood } from '@renderer/pages/companion/characters';
import CharacterPicker from '../CharacterPicker';
import CompanionModelControl from '../CompanionModelControl';
import { figureToCustomPatch } from '../useFigures';
import type { useCompanion } from '../useNomi';

const MOOD_EMOJI: Record<string, string> = {
  happy: '😊',
  content: '😌',
  sleepy: '😪',
  worried: '😟',
  excited: '🤩',
};

interface Props {
  companion: ReturnType<typeof useCompanion>;
  onGoTab: (key: string) => void;
}

const OverviewTab: React.FC<Props> = ({ companion, onGoTab }) => {
  const { t } = useTranslation();
  const { profile, status, loading, patchCompanion } = companion;
  const [diaries, setDiaries] = useState<ICompanionLearnRun[]>([]);
  const [adjustOpen, setAdjustOpen] = useState(false);
  // First-launch self-evolution disclosure: render-gated by localStorage (per-browser
  // "seen"); the actual default-ON write is gated server-side by a consent KV flag, so
  // existing users are never silently enabled (design §9 privacy red line).
  const [disclosureSeen, setDisclosureSeen] = useState(true);

  useEffect(() => {
    let seen = false;
    try {
      seen = localStorage.getItem('companion.selfEvolution.disclosureSeen') === '1';
    } catch {
      /* private mode */
    }
    setDisclosureSeen(seen);
  }, []);

  const dismissDisclosure = () => {
    setDisclosureSeen(true);
    try {
      localStorage.setItem('companion.selfEvolution.disclosureSeen', '1');
    } catch {
      /* private mode */
    }
  };

  const acknowledgeDisclosure = () => {
    void ipcBridge.companion.applyConsent
      .invoke()
      .then(() => {
        Message.success(t('nomi.disclosure.enabled', { defaultValue: '已开启，桌面伙伴会从你的使用里学习' }));
        void companion.refreshStatus?.();
      })
      .catch((e) => Message.error(String(e)));
    dismissDisclosure();
  };

  useEffect(() => {
    void ipcBridge.companion.listLearnRuns
      .invoke({ limit: 10 })
      .then((runs) => setDiaries(runs.filter((r) => r.summary)))
      .catch(() => {});
  }, [status?.last_learn?.id]);

  const [digest, setDigest] = useState<ICompanionWeeklyDigest | null>(null);
  useEffect(() => {
    const cid = companion.profile?.id;
    if (!cid) return;
    void ipcBridge.companion.weeklyDigest
      .invoke({ companion_id: cid })
      .then(setDigest)
      .catch(() => {});
  }, [companion.profile?.id, status?.last_learn?.id]);

  // ── 自定义形象尺寸滑块（仅 custom 形象）──
  // cf 从 profile 解析（profile 可能为 null，customFigureMetaOf 已防御 → null）。
  const cf = customFigureMetaOf(profile);
  const effectiveFigureHeight = cf ? (cf.sizePx ?? FIGURE_HEIGHTS[cf.sizeTier]) : FIGURE_HEIGHTS.m;
  const [sizeDraft, setSizeDraft] = useState<number>(effectiveFigureHeight);
  // 持久值（或所选伙伴/形象）变化时，把滑块同步回真实值。
  useEffect(() => {
    setSizeDraft(effectiveFigureHeight);
  }, [effectiveFigureHeight]);
  const sizeTimerRef = useRef<ReturnType<typeof setTimeout> | null>(null);
  useEffect(
    () => () => {
      if (sizeTimerRef.current) clearTimeout(sizeTimerRef.current);
    },
    []
  );
  // RFC 7396 合并补丁：在既有 wire custom_figure 上覆盖 size_px（数字=设值，null=清除回档位）。
  // 落库后 patch_companion 广播 companion.config-updated → 桌宠窗口 applyDeskSize 实时缩放。
  const commitFigureSize = useCallback(
    (size_px: number | null) => {
      const base = profile?.appearance.custom_figure;
      if (!base) return;
      void patchCompanion({ appearance: { custom_figure: { ...base, size_px } } });
    },
    [patchCompanion, profile]
  );
  const onFigureSizeChange = useCallback(
    (v: number | number[]) => {
      const next = Array.isArray(v) ? v[0] : v;
      setSizeDraft(next);
      if (sizeTimerRef.current) clearTimeout(sizeTimerRef.current);
      sizeTimerRef.current = setTimeout(() => commitFigureSize(next), 400);
    },
    [commitFigureSize]
  );
  const onFigureSizeReset = useCallback(() => {
    if (sizeTimerRef.current) clearTimeout(sizeTimerRef.current);
    commitFigureSize(null);
  }, [commitFigureSize]);

  if (loading || !status || !profile) {
    return (
      <div className='flex justify-center py-40px'>
        <Spin />
      </div>
    );
  }

  const companionName = profile.name;
  const levelBase = (status.level - 1) ** 2 * 100;
  const levelNext = status.level ** 2 * 100;
  const levelPct = Math.min(100, Math.round(((status.xp - levelBase) / Math.max(1, levelNext - levelBase)) * 100));

  return (
    <div className='flex flex-col gap-16px py-8px'>
      {!disclosureSeen && (
        <Alert
          type='info'
          closable
          onClose={dismissDisclosure}
          style={{ background: 'var(--color-fill-2)', border: '1px solid var(--color-border-2)' }}
          title={t('nomi.disclosure.title', { defaultValue: '让桌面伙伴越用越懂你' })}
          content={
            <div className='flex flex-col gap-8px'>
              <span className='text-12px text-t-secondary'>
                {t('nomi.disclosure.body', {
                  defaultValue:
                    '开启后，伙伴会从你使用平台的行为（工具调用、任务、对话）里学习，自动沉淀技能与记忆。所有数据仅保存在本地，随时可在「数据采集」里查看、清空或一键全关。',
                })}
              </span>
              <div className='flex gap-8px'>
                <Button size='small' type='primary' onClick={acknowledgeDisclosure}>
                  {t('nomi.disclosure.enable', { defaultValue: '开启自学习' })}
                </Button>
                <Button size='small' onClick={dismissDisclosure}>
                  {t('nomi.disclosure.later', { defaultValue: '暂不开启' })}
                </Button>
              </div>
            </div>
          }
        />
      )}
      <div className='flex items-center gap-16px bg-fill-2 rd-10px px-14px py-12px'>
        <div className='flex-1 min-w-0'>
          <div className='text-14px text-t-primary font-500'>{t('nomi.settings.companionEnabled')}</div>
          <div className='text-12px text-t-tertiary mt-2px'>{t('nomi.settings.companionEnabledHint')}</div>
        </div>
        <span className='text-13px text-t-secondary'>
          {profile.appearance.companion_enabled ? t('nomi.overview.companionOn') : t('nomi.overview.companionOff')}
        </span>
        <Switch
          checked={profile.appearance.companion_enabled}
          onChange={(companion_enabled) => void patchCompanion({ appearance: { companion_enabled } })}
        />
      </div>
      {/* 对话模型：唯一事实源入口，直接在总览就地可配（免去跳到聊天页头部找）。
          未配置=暖色警示 + 引导文案；已配置=常规卡 + 全局生效说明。始终可见可改。 */}
      <div
        className='flex flex-col gap-10px rd-10px px-14px py-12px'
        style={
          status.model_configured
            ? { background: 'var(--color-fill-2)' }
            : { background: 'rgb(var(--warning-1))', border: '1px solid rgb(var(--warning-3))' }
        }
      >
        <div className='text-12px text-t-secondary'>
          {status.model_configured
            ? t('nomi.chat.modelConfigHint')
            : t('nomi.overview.modelMissing', { companionName })}
        </div>
        <CompanionModelControl companion={companion} />
      </div>
      {!status.collect_any_enabled && (
        <Alert
          type='info'
          style={{ background: 'var(--color-fill-2)', border: '1px solid var(--color-border-2)' }}
          content={t('nomi.overview.collectOff', { companionName })}
          action={
            <Button size='mini' onClick={() => onGoTab('collect')}>
              {t('nomi.overview.goCollect')}
            </Button>
          }
        />
      )}
      <div className='flex items-center gap-20px flex-wrap'>
        <div className='flex flex-col items-center gap-8px'>
          <button
            type='button'
            onClick={() => setAdjustOpen(true)}
            title={t('nomi.customFigure.adjustFigure')}
            className='group relative flex items-center justify-center rd-16px p-4px cursor-pointer bg-transparent border-none transition-transform hover:scale-[1.02]'
          >
            <CompanionAvatar
              character={profile.character}
              companionId={profile.id}
              customFigure={cf}
              mood={(status.mood as CompanionMood) || 'content'}
              activity='idle'
              size={120}
            />
            <span className='absolute inset-4px flex items-end justify-center rd-16px bg-gradient-to-t from-[rgba(0,0,0,0.45)] to-transparent opacity-0 group-hover:opacity-100 transition-opacity'>
              <span className='mb-8px flex items-center gap-4px text-12px font-600 text-white'>
                <IconEdit /> {t('nomi.customFigure.adjustFigure')}
              </span>
            </span>
          </button>
          <Button size='mini' type='text' icon={<IconEdit />} onClick={() => setAdjustOpen(true)}>
            {t('nomi.customFigure.adjustFigure')}
          </Button>
        </div>
        <div className='flex flex-col gap-8px min-w-220px flex-1'>
          <div className='flex items-center gap-8px'>
            <span className='text-18px font-700 text-t-primary'>
              {companionName} · Lv{status.level} {t(`nomi.levels.l${Math.min(status.level, 5)}`)}
            </span>
            <Tag color='pinkpurple'>
              {MOOD_EMOJI[status.mood] || '😌'} {t(`nomi.moods.${status.mood}`, status.mood)}
            </Tag>
          </div>
          <Progress percent={levelPct} formatText={() => `${status.xp} XP`} color='var(--color-primary)' />
          <div className='flex gap-16px text-13px text-t-secondary flex-wrap'>
            <span>
              {t('nomi.overview.memories')}: <b className='text-t-primary'>{status.memories_active}</b>
            </span>
            <span>
              {t('nomi.overview.newSuggestions')}: <b className='text-t-primary'>{status.suggestions_new}</b>
            </span>
            <span>
              {t('nomi.overview.skillsActive', { defaultValue: '专精技能' })}:{' '}
              <b className='text-t-primary'>{status.skills_active}</b>
            </span>
          </div>
        </div>
      </div>
      {cf && (
        <div className='flex items-center gap-12px bg-fill-2 rd-10px px-14px py-12px flex-wrap'>
          <span className='text-13px text-t-secondary shrink-0'>{t('nomi.customFigure.sizeLabel')}</span>
          <span className='text-12px text-t-tertiary shrink-0'>{t('nomi.customFigure.sizeS')}</span>
          <Slider
            className='flex-1 min-w-160px'
            min={SIZE_MIN}
            max={SIZE_MAX}
            step={4}
            value={sizeDraft}
            onChange={onFigureSizeChange}
          />
          <span className='text-12px text-t-tertiary shrink-0'>{t('nomi.customFigure.sizeL')}</span>
          <span className='text-12px text-t-primary text-right shrink-0' style={{ width: 46 }}>
            {sizeDraft}px
          </span>
          {cf.sizePx != null && (
            <Button size='mini' type='text' onClick={onFigureSizeReset}>
              {t('nomi.customFigure.sizeReset')}
            </Button>
          )}
        </div>
      )}
      {digest && (digest.skills_learned > 0 || digest.memories_added > 0 || digest.learn_runs > 0) && (
        <div className='bg-fill-2 rd-10px px-14px py-12px'>
          <div className='text-14px text-t-primary font-500 mb-6px'>
            {t('nomi.overview.weeklyTitle', { defaultValue: '我这周学到了什么' })}
          </div>
          <div className='flex gap-16px text-13px text-t-secondary flex-wrap'>
            <span>
              {t('nomi.overview.weeklySkills', { defaultValue: '新技能' })}:{' '}
              <b className='text-t-primary'>{digest.skills_learned}</b>
            </span>
            <span>
              {t('nomi.overview.weeklyMemories', { defaultValue: '新记忆' })}:{' '}
              <b className='text-t-primary'>{digest.memories_added}</b>
            </span>
            <span>
              {t('nomi.overview.weeklyRuns', { defaultValue: '学习次数' })}:{' '}
              <b className='text-t-primary'>{digest.learn_runs}</b>
            </span>
          </div>
          {digest.new_skill_names.length > 0 && (
            <div className='mt-6px flex gap-6px flex-wrap'>
              {digest.new_skill_names.map((n) => (
                <Tag key={n} color='arcoblue' size='small'>
                  {n}
                </Tag>
              ))}
            </div>
          )}
        </div>
      )}
      <div>
        <h3 className='m-0 mb-8px text-15px text-t-primary'>{t('nomi.overview.diary', { companionName })}</h3>
        {diaries.length === 0 ? (
          <div className='text-13px text-t-tertiary'>{t('nomi.overview.diaryEmpty', { companionName })}</div>
        ) : (
          <div className='flex flex-col gap-6px'>
            {diaries.map((run) => (
              <div key={run.id} className='text-13px text-t-secondary bg-fill-2 rd-8px px-12px py-8px'>
                <span className='text-t-tertiary mr-8px'>{new Date(run.started_at).toLocaleString()}</span>
                {run.summary}
              </div>
            ))}
          </div>
        )}
      </div>

      <Modal
        title={t('nomi.customFigure.adjustFigure')}
        visible={adjustOpen}
        onCancel={() => setAdjustOpen(false)}
        footer={
          <Button type='primary' onClick={() => setAdjustOpen(false)}>
            {t('nomi.customFigure.done')}
          </Button>
        }
        style={{ width: 600 }}
      >
        <div className='flex flex-col gap-10px'>
          <span className='text-12px text-t-tertiary'>{t('nomi.settings.characterHint')}</span>
          <CharacterPicker
            value={profile.character || 'mochi'}
            figureId={cf?.figureId}
            onSelectCharacter={(character) => void patchCompanion({ character, appearance: { custom_figure: null } })}
            onSelectFigure={(fig) =>
              void patchCompanion({
                character: CUSTOM_CHARACTER_ID,
                appearance: { custom_figure: figureToCustomPatch(fig) },
              })
            }
          />
        </div>
      </Modal>
    </div>
  );
};

export default OverviewTab;
