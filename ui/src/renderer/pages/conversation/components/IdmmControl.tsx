/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import React, { useEffect, useMemo, useRef, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Button, Input, InputNumber, Popover, Select, Switch, Tooltip } from '@arco-design/web-react';
import { ipcBridge } from '@/common';
import type {
  IdmmRunState,
  IdmmTargetKind,
  IdmmWatchTier,
  IdmmScanScope,
  IdmmWakeStrategy,
  IdmmTendency,
  IdmmBlockedBehavior,
  IdmmCategoryMode,
  IIdmmConfig,
  IIdmmWatchBase,
  IIdmmFaultWatchConfig,
  IIdmmDecisionWatchConfig,
  IIdmmIntervention,
  IIdmmSetParams,
  IIdmmState,
} from '@/common/adapter/ipcBridge';
import { useArcoMessage } from '@/renderer/utils/ui/useArcoMessage';
import { CAPABILITY_COLORS } from '@/renderer/components/capability/CapabilityIcon';
import { IDMM_STATUS_COLOR } from '@/renderer/components/capability/capabilityStatusColors';
import { renderIdmmCapabilityIcon } from '@/renderer/components/capability/idmmCapabilityIcon';
import { applyIdmmStateToSessionCapabilities } from '@/renderer/pages/conversation/SessionList/hooks/useSessionCapabilities';
import { useProvidersQuery } from '@renderer/hooks/agent/useModelProviderList';
import IdmmInterventionRow from './IdmmInterventionRow';
import { isLiveEventForTarget } from './liveEventMatch';
import {
  getWatchBackupValidationErrorKey,
  type IdmmBackupValidationKey,
} from './IdmmControl.validation';

export type IdmmTarget = {
  kind: IdmmTargetKind;
  /** conversation/terminal session id — backend INTEGER (numeric-id spec §1). */
  id: number;
};

/** Draft (pre-creation) IDMM config: held by the parent and applied once the
 * conversation exists (e.g. Guid page applies it right after create). */
export type IdmmDraft = {
  value: IIdmmConfig;
  onChange: (next: IIdmmConfig) => void;
};

type IdmmControlProps = {
  /** What this control supervises: a chat conversation or a terminal session.
   * Omit when using `draft` mode. */
  target?: IdmmTarget;
  /** Controlled draft state — when set, the control never reads or writes the
   * backend IDMM state; `target` is ignored. */
  draft?: IdmmDraft;
  /** When set, the control is disabled and this reason shows as a tooltip. */
  disabledReason?: string;
  /** When the config takes effect, shown at the panel bottom (draft mode). */
  applyNote?: string;
};


/** Shared watch-base defaults (mirrors `WatchBase::default()` on the backend). */
const defaultWatchBase = (): IIdmmWatchBase => ({
  enabled: false,
  tier: 'rule_only',
  scan_interval_secs: 60,
  max_retries: 5,
  scan_scope: 'last_turn',
  max_context_chars: 8000,
  bypass_model: { provider_id: null, model: null },
  budget: { max_interventions_per_hour: 30, min_interval_secs: 20 },
});

/** Default config used to seed the editable form before a save exists.
 * Default values mirror the backend `IdmmConfig::default()` so the form is
 * behavior-equivalent to Phase-1 once enabled (both watches default off). */
export const defaultIdmmConfig = (): IIdmmConfig => ({
  fault_watch: {
    ...defaultWatchBase(),
    wake_action: 'retry',
    use_failover_queue: false,
  },
  decision_watch: {
    ...defaultWatchBase(),
    strategy: {
      tendency: 'balanced',
      on_blocked: 'prefer_continue',
      categories: {
        option_decision: {
          mode: 'auto',
          prefer_recommended: true,
          allow_unmarked_pick: true,
          never_destructive: true,
        },
        open_question: { mode: 'auto', max_answer_chars: 600 },
        permission: { mode: 'auto', only_safe_value: true, escalate_risky: true },
      },
      freeform_policy: null,
    },
    // 纯问答默认开(旁路模型档生效;规则档下该开关被禁用且惰性无影响)。
    answer_open_questions: true,
  },
});

/** Map a watch's runtime tier wording onto the model/rule flag. */
const isModelTier = (tier: IdmmWatchTier): boolean => tier === 'rule_plus_model';

/**
 * Per-session IDMM (Intelligent Decision-Making Mode) control. Rendered next to
 * AutoWorkControl in a conversation or terminal header. A compact button opens a
 * popover with two collapsible watches — 故障值守 (fault) and 决策值守 (decision) —
 * each with its own tier / scan interval / retries / scan scope / bypass model,
 * plus a decision-strategy editor for the decision watch, and the Phase-1
 * decision timeline. The Guid page renders the same control in `draft` mode to
 * pre-configure a conversation before it exists.
 */
const IdmmControl: React.FC<IdmmControlProps> = ({ target, draft, disabledReason, applyNote }) => {
  const { t } = useTranslation();
  const [message, messageContext] = useArcoMessage({ maxCount: 1 });
  const kind = target?.kind;
  const id = target?.id;
  const [state, setState] = useState<IIdmmState | null>(null);
  const [persistedCfg, setPersistedCfg] = useState<IIdmmConfig>(defaultIdmmConfig);
  const { data: providers } = useProvidersQuery();
  const isDraft = !!draft;
  const draftRef = useRef(draft);
  draftRef.current = draft;
  // Draft mode has no live IIdmmState, so the global backup-provider check is
  // resolved from the global settings instead of `sidecar_provider_resolved`.
  const [draftBackupResolved, setDraftBackupResolved] = useState(false);

  // Which watch sections are expanded in the popover. Default: open whatever is
  // enabled so the user lands on their active config; otherwise both collapsed.
  const [faultOpen, setFaultOpen] = useState(false);
  const [decisionOpen, setDecisionOpen] = useState(false);
  const [strategyOpen, setStrategyOpen] = useState(false);

  // 决策记录时间线:仅非草稿、有 target 时可用。折叠区展开后才拉取(避免每次开
  // popover 都打一次 DB),WS 实时 prepend 本 target 的新记录。
  const [logOpen, setLogOpen] = useState(false);
  const [log, setLog] = useState<IIdmmIntervention[]>([]);
  const [logLoading, setLogLoading] = useState(false);

  const cfg = draft ? draft.value : persistedCfg;
  const updateCfg = (updater: (c: IIdmmConfig) => IIdmmConfig) => {
    const d = draftRef.current;
    if (d) d.onChange(updater(d.value));
    else setPersistedCfg(updater);
  };
  const updateFault = (updater: (w: IIdmmFaultWatchConfig) => IIdmmFaultWatchConfig) =>
    updateCfg((c) => ({ ...c, fault_watch: updater(c.fault_watch) }));
  const updateDecision = (updater: (w: IIdmmDecisionWatchConfig) => IIdmmDecisionWatchConfig) =>
    updateCfg((c) => ({ ...c, decision_watch: updater(c.decision_watch) }));

  const providerOptions = useMemo(() => (providers ?? []).map((p) => ({ label: p.name, value: p.id })), [providers]);
  const modelsForProvider = (providerId?: string | null) => {
    const p = (providers ?? []).find((x) => x.id === providerId);
    return (p?.models ?? []).map((m) => ({ label: m, value: m }));
  };

  // Load live state + seed the form from the persisted config (so user
  // edits survive remount). Persisted config wins; otherwise inherit the
  // enabled flag from live state and keep typed defaults.
  useEffect(() => {
    if (isDraft || !kind || !id) return;
    let cancelled = false;
    void (async () => {
      try {
        const s = await ipcBridge.idmm.getStatus.invoke({ kind, target_id: id });
        if (cancelled) return;
        setState(s);
        applyIdmmStateToSessionCapabilities(s);
        if (s.config) {
          // Persisted config wins — rehydrate the entire form from it.
          setPersistedCfg(s.config);
          setFaultOpen(s.config.fault_watch.enabled);
          setDecisionOpen(s.config.decision_watch.enabled);
          setStrategyOpen(false);
        }
      } catch {
        /* ignore — keep defaults */
      }
    })();
    return () => {
      cancelled = true;
    };
  }, [kind, id, isDraft]);

  // Draft mode: resolve whether a global backup provider exists (the live-state
  // flag is unavailable before the conversation is created). Only expand watches
  // that are already enabled so the first view stays compact.
  useEffect(() => {
    if (!isDraft) return;
    let cancelled = false;
    setFaultOpen(draftRef.current?.value.fault_watch.enabled ?? false);
    setDecisionOpen(draftRef.current?.value.decision_watch.enabled ?? false);
    setStrategyOpen(false);
    void (async () => {
      try {
        const g = await ipcBridge.idmm.getSettings.invoke();
        if (cancelled) return;
        setDraftBackupResolved(Boolean(g.backup_provider_id));
      } catch {
        /* ignore — keep defaults */
      }
    })();
    return () => {
      cancelled = true;
    };
  }, [isDraft]);

  // Live status updates over WebSocket. Status events do NOT carry the
  // persisted config (only GET does), so we only refresh runtime fields here
  // and never clobber the form-bound `cfg` from a status broadcast.
  useEffect(() => {
    if (isDraft || !kind || !id) return;
    const unsub = ipcBridge.idmm.onStatus.on((s) => {
      if (isLiveEventForTarget(s.kind, s.target_id, kind, id)) {
        setState(s);
        applyIdmmStateToSessionCapabilities(s);
      }
    });
    return () => unsub();
  }, [kind, id, isDraft]);

  // 决策记录:折叠区展开时拉取最近 30 条(most-recent-first)。
  useEffect(() => {
    if (isDraft || !kind || !id || !logOpen) return;
    let cancelled = false;
    setLogLoading(true);
    void (async () => {
      try {
        const rows = await ipcBridge.idmm.getLog.invoke({ kind, target_id: id, limit: 30 });
        if (!cancelled) setLog(rows);
      } catch {
        /* ignore — 记录是非关键审计数据 */
      } finally {
        if (!cancelled) setLogLoading(false);
      }
    })();
    return () => {
      cancelled = true;
    };
  }, [kind, id, isDraft, logOpen]);

  // 实时 prepend:WS 推送的新介入若属于本 target 则插到列表最前(仅展开时维护)。
  useEffect(() => {
    if (isDraft || !kind || !id || !logOpen) return;
    const unsub = ipcBridge.idmm.onIntervention.on((rec) => {
      if (rec.target_kind === kind && rec.target_id === String(id)) {
        setLog((prev) => [rec, ...prev].slice(0, 30));
      }
    });
    return () => unsub();
  }, [kind, id, isDraft, logOpen]);

  const clearLog = useMemo(
    () => async () => {
      if (!kind || !id) return;
      try {
        await ipcBridge.idmm.clearLog.invoke({ kind, target_id: id });
        setLog([]);
        message.success(t('idmm.log.clearOk'));
      } catch (e) {
        message.error(String(e));
      }
    },
    [kind, id, message, t]
  );

  const anyEnabled = cfg.fault_watch.enabled || cfg.decision_watch.enabled;
  const enabled = draft ? anyEnabled : (state?.enabled ?? false);
  const runState: IdmmRunState = state?.run_state ?? 'off';

  // A model-tier watch needs a complete local backup model or a resolvable
  // fallback. Surface a hint before mutating the toggle state.
  const globalBackupResolved = draft ? draftBackupResolved : (state?.sidecar_provider_resolved ?? false);
  const watchBackupErrorKey = (w: IIdmmWatchBase): IdmmBackupValidationKey | null =>
    getWatchBackupValidationErrorKey(w, globalBackupResolved);
  const faultBackupErrorKey = watchBackupErrorKey(cfg.fault_watch);
  const decisionBackupErrorKey = watchBackupErrorKey(cfg.decision_watch);

  const dotColor = draft ? (enabled ? CAPABILITY_COLORS.primary : CAPABILITY_COLORS.off) : IDMM_STATUS_COLOR[runState];
  const statusText = draft
    ? enabled
      ? t('guid.advanced.draftOn')
      : t('guid.advanced.draftOff')
    : t(`idmm.state.${runState}`);

  // Persist the whole config (debounced via explicit save on every toggle/edit
  // commit). Draft mode reports upward; persisted mode POSTs and refreshes state.
  const persist = useMemo(
    () => async (nextCfg: IIdmmConfig) => {
      const d = draftRef.current;
      const validationKey = watchBackupErrorKey(nextCfg.fault_watch) ?? watchBackupErrorKey(nextCfg.decision_watch);
      if (validationKey) {
        message.warning(t(validationKey));
        return;
      }
      if (d) {
        d.onChange(nextCfg);
        return;
      }
      if (!kind || !id) return;
      const params: IIdmmSetParams = { kind, target_id: id, ...nextCfg };
      try {
        const s = await ipcBridge.idmm.set.invoke(params);
        setState(s);
        applyIdmmStateToSessionCapabilities(s);
        message.success(t(nextCfg.fault_watch.enabled || nextCfg.decision_watch.enabled ? 'idmm.enabledOk' : 'idmm.disabledOk'));
      } catch (e) {
        message.error(String(e));
      }
    },
    // watchBackupErrorKey closes over draft/globalBackupResolved; recompute when those change.
    // eslint-disable-next-line react-hooks/exhaustive-deps
    [kind, id, message, t, globalBackupResolved, draft]
  );

  // Apply an edit and persist the freshly-computed config in ONE write (never a
  // stale re-report). Draft: report up (that IS the persist; Guid applies on
  // creation). Persisted: update local + POST. Replaces the update()+commit()
  // pair that clobbered draft-mode selects.
  const commitCfg = (updater: (c: IIdmmConfig) => IIdmmConfig) => {
    const d = draftRef.current;
    if (d) {
      void persist(updater(d.value));
      return;
    }
    const next = updater(cfg); // cfg === persistedCfg here; correct base for one edit
    setPersistedCfg(next);
    void persist(next);
  };
  const commitFault = (u: (w: IIdmmFaultWatchConfig) => IIdmmFaultWatchConfig) =>
    commitCfg((c) => ({ ...c, fault_watch: u(c.fault_watch) }));
  const commitDecision = (u: (w: IIdmmDecisionWatchConfig) => IIdmmDecisionWatchConfig) =>
    commitCfg((c) => ({ ...c, decision_watch: u(c.decision_watch) }));

  // Toggle a single watch's enabled flag and persist the resulting config.
  const toggleFault = (next: boolean) => {
    const nextCfg: IIdmmConfig = { ...cfg, fault_watch: { ...cfg.fault_watch, enabled: next } };
    const validationKey = next ? watchBackupErrorKey(nextCfg.fault_watch) : null;
    if (validationKey) {
      setFaultOpen(true);
      message.warning(t(validationKey));
      return;
    }
    if (draftRef.current) draftRef.current.onChange(nextCfg);
    else setPersistedCfg(nextCfg);
    if (next) setFaultOpen(true);
    void persist(nextCfg);
  };
  const toggleDecision = (next: boolean) => {
    const nextCfg: IIdmmConfig = { ...cfg, decision_watch: { ...cfg.decision_watch, enabled: next } };
    const validationKey = next ? watchBackupErrorKey(nextCfg.decision_watch) : null;
    if (validationKey) {
      setDecisionOpen(true);
      message.warning(t(validationKey));
      return;
    }
    if (draftRef.current) draftRef.current.onChange(nextCfg);
    else setPersistedCfg(nextCfg);
    if (next) setDecisionOpen(true);
    void persist(nextCfg);
  };

  const sectionClass = 'flex flex-col gap-8px rounded-12px bg-fill-1 px-12px py-10px';
  const fieldStackClass = 'min-w-0 flex flex-col gap-4px';
  const fieldLabelClass = 'text-t-secondary text-11px leading-15px';
  const subtleInsetClass = 'rounded-7px bg-fill-1 px-8px py-7px';
  const watchTone = (color: string, amount = 12) => `color-mix(in srgb, rgb(${color}) ${amount}%, var(--color-bg-1))`;
  const lineClamp2Style: React.CSSProperties = {
    display: '-webkit-box',
    WebkitBoxOrient: 'vertical',
    WebkitLineClamp: 2,
    overflow: 'hidden',
  };

  // Shared knob rows for a watch base. Once a watch is enabled, its config is
  // read-only until disabled, including pre-creation draft mode.
  const renderBase = (
    base: IIdmmWatchBase,
    update: (updater: (w: IIdmmWatchBase) => IIdmmWatchBase) => void,
    commitUpdate: (u: (w: IIdmmWatchBase) => IIdmmWatchBase) => void,
    locked: boolean,
    backupErrorKey: IdmmBackupValidationKey | null
  ) => {
    const model = isModelTier(base.tier);
    return (
      <>
        <div className={fieldStackClass}>
          <span className={fieldLabelClass}>{t('idmm.tierLabel')}</span>
          <Select
            size='mini'
            value={base.tier}
            disabled={locked}
            className='w-full'
            onChange={(v: IdmmWatchTier) => commitUpdate((w) => ({ ...w, tier: v }))}
            options={[
              { label: t('idmm.tier.ruleOnly'), value: 'rule_only' },
              { label: t('idmm.tier.rulePlusModel'), value: 'rule_plus_model' },
            ]}
          />
          <span className='text-t-tertiary text-11px leading-15px'>
            {t(model ? 'idmm.tier.rulePlusModelDesc' : 'idmm.tier.ruleOnlyDesc')}
          </span>
        </div>

        <div className='grid grid-cols-3 gap-6px'>
          <div className={fieldStackClass}>
            <span className={fieldLabelClass}>{t('idmm.watch.scanInterval')}</span>
            <InputNumber
              size='mini'
              className='w-full'
              min={5}
              max={3600}
              step={5}
              disabled={locked}
              value={base.scan_interval_secs}
              onChange={(v) => update((w) => ({ ...w, scan_interval_secs: typeof v === 'number' ? v : w.scan_interval_secs }))}
              onBlur={() => commitUpdate((w) => w)}
              suffix={t('idmm.watch.secondsSuffix')}
            />
          </div>

          <div className={fieldStackClass}>
            <span className={fieldLabelClass}>{t('idmm.watch.maxRetries')}</span>
            <InputNumber
              size='mini'
              className='w-full'
              min={0}
              max={50}
              disabled={locked}
              value={base.max_retries}
              onChange={(v) => update((w) => ({ ...w, max_retries: typeof v === 'number' ? v : w.max_retries }))}
              onBlur={() => commitUpdate((w) => w)}
            />
          </div>
          <div className={fieldStackClass}>
            <span className={fieldLabelClass}>{t('idmm.watch.scanScope')}</span>
            <Select
              size='mini'
              className='w-full'
              value={base.scan_scope}
              disabled={locked}
              onChange={(v: IdmmScanScope) => commitUpdate((w) => ({ ...w, scan_scope: v }))}
              options={[
                { label: t('idmm.scanScope.lastTurn'), value: 'last_turn' },
                { label: t('idmm.scanScope.lastMessages'), value: 'last_messages' },
                { label: t('idmm.scanScope.fullSession'), value: 'full_session' },
              ]}
            />
          </div>
        </div>

        {model ? (
          <div className={fieldStackClass}>
            <div className='flex items-center justify-between gap-8px'>
              <span className={fieldLabelClass}>{t('idmm.watch.bypassModel')}</span>
              {backupErrorKey ? (
                <span className='rounded-full bg-[rgb(var(--warning-1))] px-6px py-2px text-10px text-[rgb(var(--warning-6))]'>
                  {t(backupErrorKey)}
                </span>
              ) : null}
            </div>
            <div className='grid grid-cols-2 gap-6px'>
              <Select
                size='mini'
                className='w-full'
                placeholder={t('idmm.settings.selectProvider')}
                value={base.bypass_model.provider_id ?? undefined}
                disabled={locked}
                allowClear
                options={providerOptions}
                onChange={(v: string | undefined) =>
                  commitUpdate((w) => ({ ...w, bypass_model: { provider_id: v || null, model: null } }))
                }
              />
              <Select
                size='mini'
                className='w-full'
                placeholder={t('idmm.settings.selectModel')}
                value={base.bypass_model.model ?? undefined}
                disabled={locked || !base.bypass_model.provider_id}
                allowClear
                options={modelsForProvider(base.bypass_model.provider_id)}
                onChange={(v: string | undefined) =>
                  commitUpdate((w) => ({ ...w, bypass_model: { ...w.bypass_model, model: v || null } }))
                }
              />
            </div>
            <span className='text-t-tertiary text-11px leading-16px'>{t('idmm.sessionBackupHint')}</span>
            {backupErrorKey ? (
              <span className='text-11px text-[rgb(var(--warning-6))]'>{t(backupErrorKey)}</span>
            ) : null}
          </div>
        ) : null}
      </>
    );
  };

  // Category-rule row: a labeled mode select + optional boolean switches.
  const modeSelect = (mode: IdmmCategoryMode, onCommit: (m: IdmmCategoryMode) => void, locked: boolean) => (
    <Select
      size='mini'
      className='w-108px shrink-0'
      value={mode}
      disabled={locked}
      onChange={(v: IdmmCategoryMode) => onCommit(v)}
      options={[
        { label: t('idmm.categoryMode.auto'), value: 'auto' },
        { label: t('idmm.categoryMode.askFirst'), value: 'ask_first' },
        { label: t('idmm.categoryMode.off'), value: 'off' },
      ]}
    />
  );

  const boolRow = (label: string, checked: boolean, onCommit: (v: boolean) => void, locked: boolean) => (
    <div className='flex items-center justify-between gap-8px py-1px'>
      <span className='min-w-0 text-t-tertiary text-11px leading-15px'>{label}</span>
      <Switch
        size='small'
        checked={checked}
        disabled={locked}
        onChange={(v) => onCommit(v)}
      />
    </div>
  );

  const faultLocked = cfg.fault_watch.enabled;
  const decisionLocked = cfg.decision_watch.enabled;
  const dw = cfg.decision_watch;
  const strat = dw.strategy;
  const cats = strat.categories;
  const blockedKey =
    strat.on_blocked === 'prefer_continue'
      ? 'preferContinue'
      : strat.on_blocked === 'prefer_pause'
        ? 'preferPause'
        : 'mustAsk';
  const optionModeKey = cats.option_decision.mode === 'ask_first' ? 'askFirst' : cats.option_decision.mode;
  const strategySummary = `${t(`idmm.tendency.${strat.tendency}`)} · ${t(`idmm.onBlocked.${blockedKey}`)} · ${t(`idmm.categoryMode.${optionModeKey}`)}`;

  // When a watch is enabled its config is read-only — changing the tier
  // (规则 / 旁路模型) or strategy after enabling can leave required fields
  // incomplete. The controls are already `disabled`; this banner surfaces WHY
  // and how to edit (turn the watch off first), so users don't think it's broken.
  const lockedNotice = (
    <div className='flex items-start gap-6px rounded-8px bg-fill-1 px-9px py-7px text-t-tertiary text-11px leading-15px'>
      <span aria-hidden className='shrink-0 leading-15px'>🔒</span>
      <span>{t('idmm.locked.notice')}</span>
    </div>
  );

  const categoryBlock = (
    label: string,
    mode: IdmmCategoryMode,
    onCommit: (m: IdmmCategoryMode) => void,
    locked: boolean,
    children?: React.ReactNode
  ) => (
    <div className='flex flex-col gap-5px'>
      <div className='flex items-center justify-between gap-8px'>
        <span className='min-w-0 text-t-secondary text-12px font-500'>{label}</span>
        {modeSelect(mode, onCommit, locked)}
      </div>
      {children ? <div className='flex flex-col gap-4px pl-8px'>{children}</div> : null}
    </div>
  );

  const sectionHeader = (
    titleKey: string,
    descKey: string,
    color: string,
    open: boolean,
    onToggleOpen: () => void,
    watchEnabled: boolean,
    onToggleEnabled: (v: boolean) => void
  ) => (
    <div className='flex items-center justify-between gap-10px'>
      <button
        type='button'
        aria-expanded={open}
        aria-label={t(open ? 'idmm.collapseConfig' : 'idmm.expandConfig')}
        className='min-w-0 flex flex-1 items-center gap-9px border-0 bg-transparent p-0 text-left cursor-pointer select-none'
        onClick={onToggleOpen}
      >
        <span
          className='inline-flex h-28px w-28px shrink-0 items-center justify-center rounded-8px'
          style={{ background: watchTone(color, open || watchEnabled ? 16 : 8) }}
        >
          <span className='inline-block h-7px w-7px rounded-full' style={{ backgroundColor: `rgb(${color})` }} />
        </span>
        <span className='min-w-0 flex flex-col gap-2px'>
          <span className='min-w-0 flex items-center gap-6px'>
            <span className='truncate text-t-primary text-13px font-600'>{t(titleKey)}</span>
            {watchEnabled ? (
              <span className='inline-flex shrink-0 items-center gap-3px rounded-full bg-fill-2 px-6px py-1px text-10px leading-none text-t-tertiary'>
                <span aria-hidden>🔒</span>
                {t('idmm.locked.badge')}
              </span>
            ) : null}
            <span className='inline-flex shrink-0 items-center gap-4px text-t-tertiary text-10px leading-none'>
              <span>{open ? '▼' : '▶'}</span>
              <span className='text-11px leading-14px'>{t(open ? 'idmm.collapseConfig' : 'idmm.expandConfig')}</span>
            </span>
          </span>
          <span className='truncate text-t-tertiary text-11px leading-15px'>{t(descKey)}</span>
        </span>
      </button>
      <Switch size='small' checked={watchEnabled} onChange={onToggleEnabled} />
    </div>
  );

  const panel = (
    <div className='box-border flex w-340px max-h-500px flex-col gap-10px overflow-hidden p-12px'>
      {messageContext}

      <div className='flex flex-col gap-5px'>
        <div className='flex items-center justify-between gap-10px'>
          <span className='inline-flex min-w-0 items-center gap-8px'>
            <span
              className='inline-flex h-24px w-24px shrink-0 items-center justify-center rounded-6px'
              style={{ background: watchTone('var(--primary-6)', 10), color: CAPABILITY_COLORS.primary }}
            >
              {renderIdmmCapabilityIcon({ size: 15, spinning: runState === 'intervening' })}
            </span>
            <span className='min-w-0 text-t-primary text-13px font-600'>{t('idmm.label')}</span>
          </span>
          <span className='inline-flex shrink-0 items-center gap-5px rounded-full bg-fill-1 px-7px py-3px text-11px text-t-secondary'>
            <span className='inline-block h-6px w-6px rounded-full' style={{ backgroundColor: dotColor }} />
            {statusText}
          </span>
        </div>
        <div className='text-t-tertiary text-11px leading-16px' style={lineClamp2Style}>
          {t('idmm.hint')}
        </div>
      </div>

      <div className='flex min-h-0 flex-1 flex-col gap-8px overflow-y-auto'>
        {/* 故障值守 */}
        <div className={sectionClass}>
          {sectionHeader(
            'idmm.watch.faultTitle',
            'idmm.watch.faultDesc',
            'var(--warning-6)',
            faultOpen,
            () => setFaultOpen((v) => !v),
            cfg.fault_watch.enabled,
            toggleFault
          )}
          {faultOpen ? (
            <div className='flex flex-col gap-8px'>
              {faultLocked ? lockedNotice : null}
              {renderBase(
                cfg.fault_watch,
                (u) => updateFault((w) => ({ ...w, ...u(w) })),
                (u) => commitFault((w) => ({ ...w, ...u(w) })),
                faultLocked,
                faultBackupErrorKey
              )}
            </div>
          ) : null}
        </div>

        {/* 决策值守 */}
        <div className={sectionClass}>
          {sectionHeader(
            'idmm.watch.decisionTitle',
            'idmm.watch.decisionDesc',
            'var(--primary-6)',
            decisionOpen,
            () => setDecisionOpen((v) => !v),
            cfg.decision_watch.enabled,
            toggleDecision
          )}
          {decisionOpen ? (
            <div className='flex flex-col gap-9px'>
              {decisionLocked ? lockedNotice : null}
              {renderBase(
                cfg.decision_watch,
                (u) => updateDecision((w) => ({ ...w, ...u(w) })),
                (u) => commitDecision((w) => ({ ...w, ...u(w) })),
                decisionLocked,
                decisionBackupErrorKey
              )}

              <div className='rounded-10px bg-fill-0 px-9px py-8px'>
                <button
                  type='button'
                  aria-expanded={strategyOpen}
                  aria-label={t(strategyOpen ? 'idmm.collapseConfig' : 'idmm.expandConfig')}
                  className='flex w-full items-center justify-between gap-10px border-0 bg-transparent p-0 text-left cursor-pointer select-none'
                  onClick={() => setStrategyOpen((v) => !v)}
                >
                  <span className='min-w-0 flex flex-col gap-2px'>
                    <span className='text-t-primary text-12px font-600'>{t('idmm.strategy.title')}</span>
                    <span className='truncate text-t-tertiary text-11px leading-15px'>{strategySummary}</span>
                  </span>
                  <span className='inline-flex shrink-0 items-center gap-4px text-t-tertiary text-10px leading-none'>
                    <span>{strategyOpen ? '▼' : '▶'}</span>
                    <span className='text-11px leading-14px'>
                      {t(strategyOpen ? 'idmm.collapseConfig' : 'idmm.expandConfig')}
                    </span>
                  </span>
                </button>

                {strategyOpen ? (
                  <div className='mt-9px flex flex-col gap-8px'>
                    <div className='grid grid-cols-2 gap-8px'>
                      <div className={fieldStackClass}>
                        <span className={fieldLabelClass}>{t('idmm.strategy.tendency')}</span>
                        <Select
                          size='mini'
                          className='w-full'
                          value={strat.tendency}
                          disabled={decisionLocked}
                          onChange={(v: IdmmTendency) =>
                            commitDecision((w) => ({ ...w, strategy: { ...w.strategy, tendency: v } }))
                          }
                          options={[
                            { label: t('idmm.tendency.conservative'), value: 'conservative' },
                            { label: t('idmm.tendency.balanced'), value: 'balanced' },
                            { label: t('idmm.tendency.aggressive'), value: 'aggressive' },
                          ]}
                        />
                      </div>

                      <div className={fieldStackClass}>
                        <span className={fieldLabelClass}>{t('idmm.strategy.onBlocked')}</span>
                        <Select
                          size='mini'
                          className='w-full'
                          value={strat.on_blocked}
                          disabled={decisionLocked}
                          onChange={(v: IdmmBlockedBehavior) =>
                            commitDecision((w) => ({ ...w, strategy: { ...w.strategy, on_blocked: v } }))
                          }
                          options={[
                            { label: t('idmm.onBlocked.preferContinue'), value: 'prefer_continue' },
                            { label: t('idmm.onBlocked.preferPause'), value: 'prefer_pause' },
                            { label: t('idmm.onBlocked.mustAsk'), value: 'must_ask' },
                          ]}
                        />
                      </div>
                    </div>

                    <div className='flex flex-col gap-8px'>
                      {categoryBlock(
                        t('idmm.category.optionDecision'),
                        cats.option_decision.mode,
                        (m) =>
                          commitDecision((w) => ({
                            ...w,
                            strategy: {
                              ...w.strategy,
                              categories: {
                                ...w.strategy.categories,
                                option_decision: { ...w.strategy.categories.option_decision, mode: m },
                              },
                            },
                          })),
                        decisionLocked,
                        <>
                          {boolRow(
                            t('idmm.category.preferRecommended'),
                            cats.option_decision.prefer_recommended,
                            (v) =>
                              commitDecision((w) => ({
                                ...w,
                                strategy: {
                                  ...w.strategy,
                                  categories: {
                                    ...w.strategy.categories,
                                    option_decision: {
                                      ...w.strategy.categories.option_decision,
                                      prefer_recommended: v,
                                    },
                                  },
                                },
                              })),
                            decisionLocked
                          )}
                          {boolRow(
                            t('idmm.category.allowUnmarkedPick'),
                            cats.option_decision.allow_unmarked_pick,
                            (v) =>
                              commitDecision((w) => ({
                                ...w,
                                strategy: {
                                  ...w.strategy,
                                  categories: {
                                    ...w.strategy.categories,
                                    option_decision: {
                                      ...w.strategy.categories.option_decision,
                                      allow_unmarked_pick: v,
                                    },
                                  },
                                },
                              })),
                            decisionLocked
                          )}
                          {boolRow(
                            t('idmm.category.neverDestructive'),
                            cats.option_decision.never_destructive,
                            (v) =>
                              commitDecision((w) => ({
                                ...w,
                                strategy: {
                                  ...w.strategy,
                                  categories: {
                                    ...w.strategy.categories,
                                    option_decision: {
                                      ...w.strategy.categories.option_decision,
                                      never_destructive: v,
                                    },
                                  },
                                },
                              })),
                            decisionLocked
                          )}
                        </>
                      )}

                      {categoryBlock(
                        t('idmm.category.openQuestion'),
                        cats.open_question.mode,
                        (m) =>
                          commitDecision((w) => ({
                            ...w,
                            strategy: {
                              ...w.strategy,
                              categories: {
                                ...w.strategy.categories,
                                open_question: { ...w.strategy.categories.open_question, mode: m },
                              },
                            },
                          })),
                        decisionLocked,
                        <div className='flex items-center justify-between gap-8px py-1px'>
                          <span className='min-w-0 text-t-tertiary text-11px leading-15px'>
                            {t('idmm.category.maxAnswerChars')}
                          </span>
                          <InputNumber
                            size='mini'
                            className='w-108px shrink-0'
                            min={50}
                            max={4000}
                            step={50}
                            disabled={decisionLocked}
                            value={cats.open_question.max_answer_chars}
                            onChange={(v) =>
                              updateDecision((w) => ({
                                ...w,
                                strategy: {
                                  ...w.strategy,
                                  categories: {
                                    ...w.strategy.categories,
                                    open_question: {
                                      ...w.strategy.categories.open_question,
                                      max_answer_chars:
                                        typeof v === 'number'
                                          ? v
                                          : w.strategy.categories.open_question.max_answer_chars,
                                    },
                                  },
                                },
                              }))
                            }
                            onBlur={() => commitDecision((w) => w)}
                          />
                        </div>
                      )}

                      {categoryBlock(
                        t('idmm.category.permission'),
                        cats.permission.mode,
                        (m) =>
                          commitDecision((w) => ({
                            ...w,
                            strategy: {
                              ...w.strategy,
                              categories: {
                                ...w.strategy.categories,
                                permission: { ...w.strategy.categories.permission, mode: m },
                              },
                            },
                          })),
                        decisionLocked,
                        <>
                          {boolRow(
                            t('idmm.category.onlySafeValue'),
                            cats.permission.only_safe_value,
                            (v) =>
                              commitDecision((w) => ({
                                ...w,
                                strategy: {
                                  ...w.strategy,
                                  categories: {
                                    ...w.strategy.categories,
                                    permission: {
                                      ...w.strategy.categories.permission,
                                      only_safe_value: v,
                                    },
                                  },
                                },
                              })),
                            decisionLocked
                          )}
                          {boolRow(
                            t('idmm.category.escalateRisky'),
                            cats.permission.escalate_risky,
                            (v) =>
                              commitDecision((w) => ({
                                ...w,
                                strategy: {
                                  ...w.strategy,
                                  categories: {
                                    ...w.strategy.categories,
                                    permission: {
                                      ...w.strategy.categories.permission,
                                      escalate_risky: v,
                                    },
                                  },
                                },
                              })),
                            decisionLocked
                          )}
                        </>
                      )}
                    </div>

                    <div className={fieldStackClass}>
                      <span className={fieldLabelClass}>{t('idmm.strategy.freeform')}</span>
                      <Input.TextArea
                        placeholder={t('idmm.strategy.freeformPlaceholder')}
                        value={strat.freeform_policy ?? ''}
                        disabled={decisionLocked}
                        autoSize={{ minRows: 2, maxRows: 4 }}
                        onChange={(v) =>
                          updateDecision((w) => ({ ...w, strategy: { ...w.strategy, freeform_policy: v || null } }))
                        }
                        onBlur={() => commitDecision((w) => w)}
                      />
                    </div>

                    <div className={`flex items-center justify-between gap-10px ${subtleInsetClass}`}>
                      <span className='inline-flex min-w-0 flex-col'>
                        <span className='text-t-secondary text-12px'>{t('idmm.strategy.answerOpenQuestions')}</span>
                        <span className='text-t-tertiary text-11px leading-15px'>
                          {t('idmm.strategy.answerOpenQuestionsHint')}
                        </span>
                      </span>
                      <Switch
                        size='small'
                        checked={dw.answer_open_questions}
                        disabled={decisionLocked || !isModelTier(dw.tier)}
                        onChange={(v) => commitDecision((w) => ({ ...w, answer_open_questions: v }))}
                      />
                    </div>
                  </div>
                ) : null}
              </div>
            </div>
          ) : null}
        </div>

        {state && state.interventions_count > 0 ? (
          <div className='text-t-tertiary text-11px'>
            {t('idmm.interventions', { count: state.interventions_count })}
          </div>
        ) : null}
        {applyNote ? <div className='text-t-quaternary text-11px leading-15px'>{applyNote}</div> : null}

        {!isDraft && kind && id ? (
          <div className={sectionClass}>
            <div className='flex items-center justify-between gap-10px'>
              <button
                type='button'
                aria-expanded={logOpen}
                className='min-w-0 flex flex-1 items-center gap-9px border-0 bg-transparent p-0 text-left cursor-pointer select-none'
                onClick={() => setLogOpen((v) => !v)}
              >
                <span className='inline-flex h-28px w-28px shrink-0 items-center justify-center rounded-8px bg-fill-2 text-t-tertiary text-10px leading-none'>
                  {logOpen ? '▼' : '▶'}
                </span>
                <span className='truncate text-t-secondary text-13px font-500'>{t('idmm.log.title')}</span>
              </button>
              {logOpen && log.length > 0 ? (
                <span
                  className='shrink-0 text-t-tertiary text-11px cursor-pointer hover:text-[rgb(var(--danger-6))]'
                  onClick={clearLog}
                >
                  {t('idmm.log.clear')}
                </span>
              ) : null}
            </div>
            {logOpen ? (
              logLoading ? (
                <span className='text-t-tertiary text-11px'>{t('idmm.log.loading')}</span>
              ) : log.length === 0 ? (
                <span className='text-t-tertiary text-11px'>{t('idmm.log.empty')}</span>
              ) : (
                <div className='flex max-h-260px flex-col gap-6px overflow-y-auto'>
                  {log.map((rec) => (
                    <IdmmInterventionRow key={rec.id || `${rec.at}-${rec.action}`} rec={rec} />
                  ))}
                </div>
              )
            ) : null}
          </div>
        ) : null}
      </div>
    </div>
  );

  const button = (
    <Button
      size='mini'
      shape='round'
      type='secondary'
      disabled={!!disabledReason}
      className='shrink-0'
    >
      <span className='inline-flex items-center gap-6px leading-none'>
        {/* Icon tinted by run-state (same hue as the session-list IDMM icon); the
            status used to live on a separate dot beside a primary-blue button. */}
        <span className='inline-flex' style={{ color: dotColor, lineHeight: 0 }}>
          {renderIdmmCapabilityIcon({ size: 14, spinning: runState === 'intervening' })}
        </span>
        <span className='text-12px'>{t('idmm.label')}</span>
      </span>
    </Button>
  );

  if (disabledReason) {
    return (
      <Tooltip content={disabledReason}>
        <span className='inline-flex'>{button}</span>
      </Tooltip>
    );
  }

  return (
    <Popover className='idmm-control-popover' trigger='click' position='br' content={panel}>
      {button}
    </Popover>
  );
};

export default IdmmControl;
