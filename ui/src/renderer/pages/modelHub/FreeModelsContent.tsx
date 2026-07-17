/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import React from 'react';
import { useTranslation } from 'react-i18next';
import classNames from 'classnames';
import { Button, Switch, Tag, Tooltip } from '@arco-design/web-react';
import {
  ApiApp,
  CloudStorage,
  Heartbeat,
  Info,
  Lightning,
  Refresh,
  RobotOne,
} from '@icon-park/react';
import NomiScrollArea from '@/renderer/components/base/NomiScrollArea';
import { useSettingsViewMode } from '@/renderer/components/settings/SettingsModal/settingsViewContext';
import { useArcoMessage } from '@/renderer/utils/ui/useArcoMessage';
import type {
  ManagedModelHealthResult,
  ManagedModelServiceAvailability,
  ManagedModelServiceStatus,
} from '@/common/types/provider/managedModelService';
import { useFreeModels } from './useFreeModels';

const availabilityColor = (availability: ManagedModelServiceAvailability): string => {
  if (availability === 'ready') return 'green';
  if (availability === 'degraded' || availability === 'unverified') return 'orange';
  return 'gray';
};

const formatRefreshTime = (value: ManagedModelServiceStatus['lastRefresh'], fallback: string): string => {
  if (value == null) return fallback;
  const date = new Date(value);
  return Number.isNaN(date.getTime()) ? fallback : date.toLocaleString();
};

const formatRefreshInterval = (
  value: number | null | undefined,
  fallback: string,
  unit: (key: 'minutes' | 'hours', count: number) => string
): string => {
  if (value == null || !Number.isFinite(value) || value <= 0) return fallback;
  const totalMinutes = Math.max(1, Math.round(value / 60_000));
  if (totalMinutes < 60) return unit('minutes', totalMinutes);
  const hours = totalMinutes / 60;
  return unit('hours', Number.isInteger(hours) ? hours : Number(hours.toFixed(1)));
};

/**
 * Managed source identities are intentionally private implementation details.
 * Never surface backend-provided source strings, URLs, errors, or tooltips.
 */
const managedSourceAlias = (): string => 'oc';

const healthDotClass = (status: ManagedModelHealthResult['status'] | 'checking'): string => {
  if (status === 'healthy') return 'bg-[rgb(var(--success-6))] shadow-[0_0_0_3px_rgba(var(--success-6),0.1)]';
  if (status === 'unhealthy') return 'bg-[rgb(var(--danger-6))] shadow-[0_0_0_3px_rgba(var(--danger-6),0.1)]';
  if (status === 'checking') return 'bg-[rgb(var(--primary-6))] animate-pulse';
  return 'bg-[var(--color-fill-4)]';
};

/**
 * FreeModelsContent — management surface for the built-in
 * `nomifun-free-model` provider. The provider itself is projected into all
 * existing model selectors; this dedicated page owns service/catalog controls
 * so its internal endpoint and credentials never enter generic provider CRUD.
 */
const FreeModelsContent: React.FC = () => {
  const { t } = useTranslation();
  const viewMode = useSettingsViewMode();
  const isPageMode = viewMode === 'page';
  const [message, messageContext] = useArcoMessage();
  const {
    status,
    error,
    isLoading,
    mutate,
    pendingAction,
    healthResults,
    healthCheckPending,
    refresh,
    setServiceEnabled,
    setModelEnabled,
    checkAllHealth,
    checkModelHealth,
  } = useFreeModels();

  const run = async (
    action: () => Promise<ManagedModelServiceStatus>,
    successKey: string,
    reportDegraded = false
  ) => {
    try {
      const nextStatus = await action();
      if (reportDegraded && nextStatus.lastError) {
        message.warning(t('settings.modelHub.free.refreshDegraded'));
        return;
      }
      message.success(t(successKey));
    } catch (actionError) {
      console.error('Managed free-model action failed:', actionError);
      message.error(t('settings.modelHub.free.actionFailed'));
    }
  };

  const serviceLabel = !status?.enabled
    ? t('settings.modelHub.free.statusDisabled')
    : status.availability === 'ready'
      ? t('settings.modelHub.free.statusReady')
      : status.availability === 'unverified'
        ? t('settings.modelHub.free.statusUnverified')
      : t('settings.modelHub.free.statusDegraded');

  const serviceAvailability: ManagedModelServiceAvailability = status?.availability ?? 'degraded';
  const hasUpstreamWarning =
    Boolean(status?.enabled) &&
    (Boolean(status?.lastError) ||
      status?.availability === 'degraded' ||
      status?.availability === 'unverified');
  const enabledModelCount = status?.models.filter((model) => model.enabled).length ?? 0;
  const lastRefresh = formatRefreshTime(status?.lastRefresh ?? null, t('settings.modelHub.free.neverRefreshed'));
  const nextRefresh = formatRefreshTime(status?.nextRefresh ?? null, t('settings.modelHub.free.notScheduled'));
  const refreshInterval = formatRefreshInterval(
    status?.refreshIntervalMs,
    t('settings.modelHub.free.intervalUnknown'),
    (key, count) => t(`settings.modelHub.free.intervalUnit.${key}`, { count })
  );
  const serviceBusy = pendingAction != null;
  const healthBusy = healthCheckPending != null;

  const runHealthCheck = async (modelId?: string) => {
    try {
      if (modelId) {
        const result = await checkModelHealth(modelId);
        if (result.status === 'healthy') {
          message.success(
            t('settings.modelHub.free.health.modelHealthy', {
              latency: result.latencyMs ?? 0,
            })
          );
        } else if (result.status === 'unhealthy') {
          message.warning(t('settings.modelHub.free.health.modelUnhealthy'));
        } else {
          message.info(t('settings.modelHub.free.health.modelUnknown'));
        }
        return;
      }

      const result = await checkAllHealth();
      if (result.unhealthy > 0 || result.unknown > 0) {
        message.warning(
          t('settings.modelHub.free.health.batchWarning', {
            healthy: result.healthy,
            total: result.total,
            unhealthy: result.unhealthy,
            unknown: result.unknown,
          })
        );
      } else {
        message.success(
          t('settings.modelHub.free.health.batchSuccess', {
            healthy: result.healthy,
            total: result.total,
          })
        );
      }
    } catch (healthError) {
      console.error('Managed free-model health check failed:', healthError);
      message.error(t('settings.modelHub.free.health.failed'));
    }
  };

  return (
    <div className='flex flex-col bg-2 rd-16px px-24px py-16px'>
      {messageContext}

      <div className='flex-shrink-0 border-b border-[var(--color-border-2)] pb-12px mb-14px flex flex-col gap-10px'>
        <div className='flex items-center justify-between gap-12px flex-wrap'>
          <div className='flex items-center gap-8px min-w-0'>
            <span className='size-28px flex items-center justify-center rd-8px bg-primary-1 text-primary-6 shrink-0'>
              <Lightning theme='outline' size='18' strokeWidth={3} />
            </span>
            <div className='min-w-0'>
              <div className='flex items-center gap-8px flex-wrap'>
                <div className='text-20px font-600 text-t-primary leading-28px'>
                  {t('settings.modelHub.free.title')}
                </div>
                {status && (
                  <Tag size='small' color={availabilityColor(serviceAvailability)}>
                    {serviceLabel}
                  </Tag>
                )}
              </div>
              <div className='mt-2px text-13px leading-18px text-t-secondary'>
                {t('settings.modelHub.free.providerId', { id: status?.providerId ?? 'nomifun-free-model' })}
              </div>
            </div>
          </div>

          <div className='flex items-center gap-12px flex-wrap'>
            <div className='flex items-center gap-8px'>
              <span className='text-13px text-t-secondary'>{t('settings.modelHub.free.serviceEnabled')}</span>
              <Switch
                size='small'
                className='compact-dark-switch'
                aria-label={t('settings.modelHub.free.serviceEnabled')}
                checked={status?.enabled ?? false}
                loading={pendingAction === 'service'}
                disabled={!status || serviceBusy || healthBusy}
                onChange={(enabled) =>
                  void run(
                    () => setServiceEnabled(enabled),
                    enabled
                      ? 'settings.modelHub.free.serviceEnabledSuccess'
                      : 'settings.modelHub.free.serviceDisabledSuccess'
                  )
                }
              />
            </div>
            <Tooltip content={t('settings.modelHub.free.refreshHint')}>
              <Button
                type='outline'
                shape='round'
                size='small'
                icon={<Refresh theme='outline' size='15' />}
                loading={pendingAction === 'refresh'}
                disabled={!status || serviceBusy || healthBusy}
                onClick={() => void run(refresh, 'settings.modelHub.free.refreshSuccess', true)}
                className='rd-100px border-1 border-solid border-[var(--color-border-2)] h-34px px-14px text-t-secondary hover:text-t-primary'
              >
                {t('settings.modelHub.free.refresh')}
              </Button>
            </Tooltip>
          </div>
        </div>

        <div
          className='flex items-start gap-9px rd-10px px-12px py-10px border border-solid'
          style={{
            borderColor: 'rgba(var(--primary-6),0.24)',
            backgroundColor: 'rgba(var(--primary-6),0.06)',
          }}
        >
          <Info theme='outline' size='16' className='mt-1px shrink-0 text-[rgb(var(--primary-6))]' />
          <div className='min-w-0'>
            <div className='text-13px font-600 leading-18px text-t-primary'>{t('settings.modelHub.free.privacyTitle')}</div>
            <div className='mt-2px text-12px leading-18px text-t-secondary'>{t('settings.modelHub.free.privacyNotice')}</div>
          </div>
        </div>
      </div>

      <NomiScrollArea className='flex-1 min-h-0' disableOverflow={isPageMode}>
        {isLoading ? (
          <div className='flex flex-col gap-12px py-4px'>
            {[0, 1, 2].map((item) => (
              <div
                key={item}
                className='h-64px rd-12px bg-[var(--fill-0)] animate-pulse border border-solid border-[var(--color-border-2)]'
              />
            ))}
          </div>
        ) : error || !status ? (
          <div className='flex flex-col items-center justify-center py-44px text-center'>
            <CloudStorage theme='outline' size='44' className='text-t-tertiary mb-12px' />
            <div className='text-16px font-500 text-t-primary'>{t('settings.modelHub.free.loadFailed')}</div>
            <div className='mt-6px max-w-420px text-13px leading-20px text-t-secondary'>
              {t('settings.modelHub.free.loadFailedHint')}
            </div>
            <Button className='mt-14px' type='primary' onClick={() => void mutate()}>
              {t('common.retry')}
            </Button>
          </div>
        ) : (
          <div className='flex flex-col gap-14px'>
            {hasUpstreamWarning && (
              <div className='rd-10px border border-solid border-[rgba(var(--warning-6),0.28)] bg-[rgba(var(--warning-6),0.08)] px-12px py-10px'>
                <div className='text-13px font-500 text-t-primary'>
                  {status.lastError
                    ? t('settings.modelHub.free.upstreamWarningTitle')
                    : status.availability === 'unverified'
                      ? t('settings.modelHub.free.unverifiedTitle')
                    : t('settings.modelHub.free.degradedTitle')}
                </div>
                <div className='mt-3px text-12px leading-18px text-t-secondary'>
                  {status.lastError
                    ? t('settings.modelHub.free.upstreamWarningHint')
                    : status.availability === 'unverified'
                      ? t('settings.modelHub.free.unverifiedHint')
                    : t('settings.modelHub.free.degradedHint')}
                </div>
              </div>
            )}

            <div className='grid grid-cols-1 min-[520px]:grid-cols-2 min-[820px]:grid-cols-4 gap-8px'>
              <div className='rd-10px border border-solid border-[var(--color-border-2)] bg-[var(--color-bg-2)] px-12px py-10px'>
                <div className='text-12px font-500 text-t-secondary'>{t('settings.modelHub.free.serviceStatus')}</div>
                <div className='mt-4px text-13px font-500 text-t-primary'>{serviceLabel}</div>
              </div>
              <div className='rd-10px border border-solid border-[var(--color-border-2)] bg-[var(--color-bg-2)] px-12px py-10px'>
                <div className='text-12px font-500 text-t-secondary'>{t('settings.modelHub.free.availableModels')}</div>
                <div className='mt-4px text-13px font-500 text-t-primary'>
                  {t('settings.modelHub.free.modelCount', {
                    enabled: enabledModelCount,
                    total: status.models.length,
                  })}
                </div>
              </div>
              <div className='rd-10px border border-solid border-[var(--color-border-2)] bg-[var(--color-bg-2)] px-12px py-10px min-w-0'>
                <div className='text-12px font-500 text-t-secondary'>{t('settings.modelHub.free.upstream')}</div>
                <div className='mt-4px text-13px font-500 text-t-primary truncate'>{managedSourceAlias()}</div>
              </div>
              <div className='rd-10px border border-solid border-[var(--color-border-2)] bg-[var(--color-bg-2)] px-12px py-10px'>
                <div className='text-12px font-500 text-t-secondary'>{t('settings.modelHub.free.lastRefresh')}</div>
                <div className='mt-4px text-13px font-500 text-t-primary truncate' title={lastRefresh}>
                  {lastRefresh}
                </div>
              </div>
            </div>

            <div className='rd-10px border border-solid border-[var(--color-border-2)] bg-[var(--fill-0)] px-12px py-8px'>
              <div className='flex items-center justify-between gap-10px flex-wrap'>
                <div className='flex items-center gap-7px min-w-0'>
                  <Refresh theme='outline' size='14' className='text-t-secondary shrink-0' />
                  <span className='text-12px font-500 text-t-primary'>
                    {t('settings.modelHub.free.automaticRefresh')}
                  </span>
                  <Tag size='small' color={status.automaticRefresh ? 'green' : 'gray'}>
                    {status.automaticRefresh
                      ? t('settings.modelHub.free.automaticRefreshOn')
                      : t('settings.modelHub.free.automaticRefreshOff')}
                  </Tag>
                </div>
                <div className='flex items-center gap-x-14px gap-y-4px flex-wrap text-12px text-t-secondary'>
                  <span>{t('settings.modelHub.free.refreshInterval', { interval: refreshInterval })}</span>
                  <span title={nextRefresh}>
                    {t('settings.modelHub.free.nextRefresh', {
                      time: nextRefresh,
                    })}
                  </span>
                </div>
              </div>
              <div className='mt-4px text-12px leading-18px text-t-secondary'>
                {status.automaticRefresh
                  ? t('settings.modelHub.free.automaticRefreshHint')
                  : t('settings.modelHub.free.manualRefreshHint')}
              </div>
            </div>

            <section className='rd-14px border border-solid border-[var(--color-border-2)] bg-[var(--color-bg-2)] p-12px shadow-[0_8px_24px_rgba(0,0,0,0.025)]'>
              <div className='flex items-center justify-between gap-10px flex-wrap'>
                <div className='flex items-center gap-8px min-w-0'>
                  <span className='size-30px rd-9px flex items-center justify-center bg-primary-1 text-primary-6 shrink-0'>
                    <ApiApp theme='outline' size='16' strokeWidth={3} />
                  </span>
                  <div className='min-w-0'>
                    <div className='flex items-center gap-7px flex-wrap'>
                      <h3 className='m-0 text-15px font-650 leading-20px text-t-primary'>
                        {t('settings.modelHub.free.catalogTitle')}
                      </h3>
                      <Tag
                        size='small'
                        bordered={false}
                        className='!rd-6px !px-7px !text-10px !bg-[rgba(var(--success-6),0.1)] !text-[rgb(var(--success-6))]'
                      >
                        {t('settings.modelHub.free.freeBadge')}
                      </Tag>
                    </div>
                    <div className='mt-2px text-12px leading-17px text-t-secondary'>
                      {t('settings.modelHub.free.modelCount', {
                        enabled: enabledModelCount,
                        total: status.models.length,
                      })}
                    </div>
                  </div>
                </div>
                <div className='flex items-center gap-9px flex-wrap'>
                  <span className='text-12px text-t-secondary'>
                    {t('settings.modelHub.free.protocolVersion', { version: status.protocolVersion })}
                  </span>
                  <Tooltip content={t('settings.modelHub.free.health.checkAllHint')}>
                    <Button
                      type='outline'
                      shape='round'
                      size='mini'
                      icon={<Heartbeat theme='outline' size='14' />}
                      loading={healthCheckPending === 'all'}
                      disabled={!status.enabled || healthBusy || serviceBusy || enabledModelCount === 0}
                      className='!h-28px !px-10px rd-100px border-[var(--color-border-2)] text-t-secondary hover:text-t-primary'
                      onClick={() => void runHealthCheck()}
                    >
                      {t('settings.modelHub.free.health.checkAll')}
                    </Button>
                  </Tooltip>
                </div>
              </div>

              {status.models.length === 0 ? (
                <div className='mt-14px rd-12px border border-dashed border-[var(--color-border-3)] px-14px py-30px text-center'>
                  <div className='text-14px text-t-primary'>{t('settings.modelHub.free.empty')}</div>
                  <div className='mt-4px text-12px text-t-secondary'>{t('settings.modelHub.free.emptyHint')}</div>
                </div>
              ) : (
                <div
                  className='mt-10px grid gap-7px'
                  style={{ gridTemplateColumns: 'repeat(auto-fill, minmax(min(235px, 100%), 1fr))' }}
                >
                  {status.models.map((model) => {
                    const modelPending = pendingAction === `model:${model.id}`;
                    const modelAvailable = status.enabled && model.enabled;
                    const healthResult = modelAvailable ? healthResults[model.id] : undefined;
                    const checking =
                      modelAvailable &&
                      (healthCheckPending === 'all' || healthCheckPending === model.id);
                    const healthStatus = checking ? 'checking' : (healthResult?.status ?? 'unknown');
                    const healthLabel = checking
                      ? t('settings.modelHub.free.health.checking')
                      : healthResult?.status === 'healthy'
                        ? t('settings.modelHub.free.health.healthy')
                        : healthResult?.status === 'unhealthy'
                          ? t('settings.modelHub.free.health.unhealthy')
                          : t('settings.modelHub.free.health.unknown');
                    const healthTitle = checking
                      ? t('settings.modelHub.free.health.checkingHint')
                      : healthResult?.status === 'healthy'
                        ? t('settings.modelHub.free.health.healthyHint', {
                            latency: healthResult.latencyMs ?? 0,
                          })
                        : healthResult?.status === 'unhealthy'
                          ? t('settings.modelHub.free.health.unhealthyHint')
                          : t('settings.modelHub.free.health.unknownHint');
                    return (
                      <div
                        key={model.id}
                        className={classNames(
                          'group relative min-w-0 overflow-hidden rd-11px border border-solid px-10px py-9px',
                          'transition-all duration-180',
                          modelAvailable
                            ? 'border-[var(--color-border-2)] bg-[var(--color-bg-2)] hover:border-[rgba(var(--primary-6),0.38)] hover:bg-primary-1 hover:shadow-[0_5px_16px_rgba(0,0,0,0.05)]'
                            : 'border-[var(--color-border-2)] bg-[var(--fill-0)] opacity-72'
                        )}
                      >
                        <span
                          aria-hidden='true'
                          className={classNames(
                            'absolute left-0 top-9px h-24px w-2px rd-r-3px transition-colors',
                            modelAvailable ? 'bg-[rgb(var(--primary-6))]' : 'bg-[var(--color-fill-4)]'
                          )}
                        />

                        <div className='flex items-center gap-8px'>
                          <span
                            className={classNames(
                              'size-28px rd-8px flex items-center justify-center shrink-0 transition-colors',
                              modelAvailable
                                ? 'bg-primary-1 text-primary-6'
                                : 'bg-[var(--color-bg-2)] text-t-tertiary'
                            )}
                          >
                            <RobotOne theme='outline' size='15' strokeWidth={3} />
                          </span>

                          <div className='min-w-0 flex-1'>
                            <div className='flex items-center justify-between gap-8px'>
                              <div className='min-w-0'>
                                <div
                                  className='truncate text-13px font-600 leading-18px text-t-primary'
                                  title={model.name || model.id}
                                >
                                  {model.name || model.id}
                                </div>
                                {model.name && model.name !== model.id && (
                                  <div className='truncate font-mono text-11px leading-15px text-t-secondary' title={model.id}>
                                    {model.id}
                                  </div>
                                )}
                              </div>
                              <div className='shrink-0' onClick={(event) => event.stopPropagation()}>
                                <Switch
                                  size='small'
                                  className='compact-dark-switch'
                                  aria-label={t('settings.modelHub.free.modelToggleLabel', {
                                    model: model.name || model.id,
                                  })}
                                  checked={model.enabled}
                                  loading={modelPending}
                                  disabled={!status.enabled || serviceBusy || healthBusy}
                                  onChange={(enabled) =>
                                    void run(
                                      () => setModelEnabled(model.id, enabled),
                                      enabled
                                        ? 'settings.modelHub.free.modelEnabledSuccess'
                                        : 'settings.modelHub.free.modelDisabledSuccess'
                                    )
                                  }
                                />
                              </div>
                            </div>
                          </div>
                        </div>

                        <div className='mt-7px flex items-center justify-between gap-8px pl-36px text-11px leading-15px text-t-secondary'>
                          <span className='truncate'>
                            {t('settings.modelHub.free.source', {
                              source: managedSourceAlias(),
                            })}
                          </span>
                          <Tooltip content={healthTitle}>
                            <span
                              role={modelAvailable && !healthBusy ? 'button' : undefined}
                              tabIndex={modelAvailable && !healthBusy ? 0 : undefined}
                              aria-label={t('settings.modelHub.free.health.checkModelLabel', {
                                model: model.name || model.id,
                              })}
                              className={classNames(
                                'shrink-0 flex items-center gap-5px rd-6px px-3px -mx-3px outline-none',
                                modelAvailable && !healthBusy
                                  ? 'cursor-pointer hover:text-t-primary focus-visible:ring-2 focus-visible:ring-[rgba(var(--primary-6),0.28)]'
                                  : 'cursor-default'
                              )}
                              onClick={() => {
                                if (modelAvailable && !healthBusy) void runHealthCheck(model.id);
                              }}
                              onKeyDown={(event) => {
                                if (
                                  modelAvailable &&
                                  !healthBusy &&
                                  (event.key === 'Enter' || event.key === ' ')
                                ) {
                                  event.preventDefault();
                                  void runHealthCheck(model.id);
                                }
                              }}
                            >
                              <span
                                aria-hidden='true'
                                className={classNames(
                                  'size-6px shrink-0 rd-full',
                                  healthDotClass(healthStatus)
                                )}
                              />
                              <span>{healthLabel}</span>
                              {!checking && healthResult?.status === 'healthy' && healthResult.latencyMs != null && (
                                <span className='text-t-secondary'>{healthResult.latencyMs}ms</span>
                              )}
                            </span>
                          </Tooltip>
                        </div>
                      </div>
                    );
                  })}
                </div>
              )}
            </section>

          </div>
        )}
      </NomiScrollArea>
    </div>
  );
};

export default FreeModelsContent;
