/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import React from 'react';
import classNames from 'classnames';
import { Button, Modal, Progress, Tag, Tooltip } from '@arco-design/web-react';
import { Delete, Download, HeadsetOne, Loading, Pause, PlayOne, Power } from '@icon-park/react';
import { useTranslation } from 'react-i18next';
import type {
  LocalModelErrorKind,
  LocalModelInstallPhase,
  LocalModelRuntimePhase,
  LocalModelState,
} from '@/common/types/provider/localModelService';
import { useArcoMessage } from '@/renderer/utils/ui/useArcoMessage';
import LocalModelCapabilitySummary from './LocalModelCapabilitySummary';
import {
  canDeleteLocalModel,
  formatLocalModelBytes,
  formatLocalModelRate,
  localModelPrimaryAction,
  localModelProgressPercent,
  type LocalModelPrimaryAction,
} from './localModelView';
import type { useLocalAsrModels } from './useLocalAsrModels';
import {
  getSpeechToTextConfig,
  saveSpeechToTextConfig,
} from '@/renderer/services/speechToTextConfig';

const installPhaseColor = (phase: LocalModelInstallPhase): string | undefined => {
  if (phase === 'installed') return 'green';
  if (phase === 'downloading' || phase === 'verifying') return 'blue';
  if (phase === 'failed') return 'red';
  if (phase === 'paused') return 'orange';
  return undefined;
};

const emptyState = (modelId: string): LocalModelState => ({
  modelId,
  installPhase: 'not_installed',
  progress: null,
  installedBytes: 0,
  runtimePhase: 'stopped',
  errorKind: null,
  message: null,
});

const asrEngineLabel = (engine: 'whisper_cpp' | 'fun_asr_llama_cpp'): string =>
  engine === 'fun_asr_llama_cpp' ? 'FunASR · llama.cpp' : 'Whisper · whisper.cpp';

export interface AsrModelsPanelProps {
  controller: ReturnType<typeof useLocalAsrModels>;
}

const AsrModelsPanel: React.FC<AsrModelsPanelProps> = ({ controller }) => {
  const { t, i18n } = useTranslation();
  const [message, messageContext] = useArcoMessage();
  const {
    catalog,
    status,
    catalogError,
    statusError,
    isLoading,
    pendingAction,
    install,
    cancel,
    remove,
    setActive,
  } = controller;
  const locale = i18n.resolvedLanguage ?? i18n.language;

  const phaseLabel = (phase: LocalModelInstallPhase): string => {
    switch (phase) {
      case 'not_installed':
        return t('settings.modelHub.local.phase.notInstalled');
      case 'downloading':
        return t('settings.modelHub.local.phase.downloading');
      case 'verifying':
        return t('settings.modelHub.local.phase.verifying');
      case 'installed':
        return t('settings.modelHub.local.phase.installed');
      case 'paused':
        return t('settings.modelHub.local.phase.paused');
      case 'failed':
        return t('settings.modelHub.local.phase.failed');
    }
  };

  const runtimeLabel = (phase: LocalModelRuntimePhase): string => {
    switch (phase) {
      case 'stopped':
        return t('settings.modelHub.local.runtime.stopped');
      case 'starting':
        return t('settings.modelHub.local.runtime.starting');
      case 'ready':
        return t('settings.modelHub.local.runtime.ready');
      case 'stopping':
        return t('settings.modelHub.local.runtime.stopping');
      case 'failed':
        return t('settings.modelHub.local.runtime.failed');
    }
  };

  const errorLabel = (kind: LocalModelErrorKind | null): string => {
    switch (kind) {
      case 'network':
        return t('settings.modelHub.local.error.network');
      case 'insufficient_space':
        return t('settings.modelHub.local.error.insufficientSpace');
      case 'checksum_mismatch':
        return t('settings.modelHub.local.error.checksumMismatch');
      case 'unsupported_platform':
        return t('settings.modelHub.local.error.unsupportedPlatform');
      case 'runtime_unavailable':
        return t('settings.modelHub.local.error.runtimeUnavailable');
      case 'busy':
        return t('settings.modelHub.local.error.busy');
      case 'not_found':
        return t('settings.modelHub.local.error.notFound');
      case 'unknown':
      case null:
        return t('settings.modelHub.local.error.unknown');
    }
  };

  const actionLabel = (action: LocalModelPrimaryAction): string => {
    switch (action) {
      case 'install':
        return t('settings.modelHub.local.action.install');
      case 'cancel':
        return t('settings.modelHub.local.action.cancel');
      case 'resume':
        return t('settings.modelHub.local.action.resume');
      case 'retry':
        return t('settings.modelHub.local.action.retry');
      case 'activate':
        return t('settings.modelHub.local.action.activate');
      case 'deactivate':
        return t('settings.modelHub.local.action.deactivate');
      case 'none':
        return t('settings.modelHub.local.phase.verifying');
    }
  };

  const actionIcon = (action: LocalModelPrimaryAction): React.ReactNode => {
    switch (action) {
      case 'install':
      case 'retry':
        return <Download theme='outline' size='14' />;
      case 'cancel':
        return <Pause theme='outline' size='14' />;
      case 'resume':
      case 'activate':
        return <PlayOne theme='outline' size='14' />;
      case 'deactivate':
        return <Power theme='outline' size='14' />;
      case 'none':
        return <Loading theme='outline' size='14' className='animate-spin' />;
    }
  };

  const runAction = async (
    action: () => Promise<unknown>,
    successKey: string,
    logContext: string
  ): Promise<void> => {
    try {
      await action();
      message.success(t(successKey));
    } catch (error) {
      console.error(`Local ASR model ${logContext} failed:`, error);
      message.error(t('settings.modelHub.local.asr.actionFailed'));
    }
  };

  const syncLocalSpeechSelection = async (modelId: string, enabled: boolean): Promise<void> => {
    const current = getSpeechToTextConfig();
    if (enabled) {
      await saveSpeechToTextConfig({
        ...current,
        enabled: true,
        provider: 'local',
        provider_id: undefined,
        model: modelId,
      });
      return;
    }
    if (current.provider === 'local' && current.model === modelId) {
      await saveSpeechToTextConfig({
        ...current,
        enabled: false,
        model: undefined,
      });
    }
  };

  const invokePrimaryAction = async (modelId: string, action: LocalModelPrimaryAction): Promise<void> => {
    switch (action) {
      case 'install':
      case 'resume':
      case 'retry':
        await runAction(
          () => install(modelId),
          action === 'resume'
            ? 'settings.modelHub.local.asr.resumeSuccess'
            : 'settings.modelHub.local.asr.installSuccess',
          'install'
        );
        return;
      case 'cancel':
        await runAction(() => cancel(modelId), 'settings.modelHub.local.asr.cancelSuccess', 'cancel');
        return;
      case 'activate':
        await runAction(
          async () => {
            const next = await setActive(modelId, true);
            try {
              await syncLocalSpeechSelection(modelId, true);
            } catch (error) {
              await setActive(modelId, false);
              throw error;
            }
            return next;
          },
          'settings.modelHub.local.asr.activateSuccess',
          'activation'
        );
        return;
      case 'deactivate':
        await runAction(
          async () => {
            const next = await setActive(modelId, false);
            try {
              await syncLocalSpeechSelection(modelId, false);
            } catch (error) {
              await setActive(modelId, true);
              throw error;
            }
            return next;
          },
          'settings.modelHub.local.asr.deactivateSuccess',
          'deactivation'
        );
        return;
      case 'none':
        return;
    }
  };

  const confirmRemove = (modelId: string, modelName: string): void => {
    Modal.confirm({
      title: t('settings.modelHub.local.asr.deleteConfirmTitle'),
      content: t('settings.modelHub.local.asr.deleteConfirmContent', { model: modelName }),
      okText: t('settings.modelHub.local.action.delete'),
      cancelText: t('common.cancel'),
      okButtonProps: { status: 'danger' },
      onOk: () =>
        runAction(
          async () => {
            const current = getSpeechToTextConfig();
            const removed = await remove(modelId);
            if (current.provider === 'local' && current.model === modelId) {
              try {
                await saveSpeechToTextConfig({
                  ...current,
                  enabled: false,
                  model: undefined,
                });
              } catch (error) {
                console.error('Failed to clear removed local ASR speech selection:', error);
              }
            }
            return removed;
          },
          'settings.modelHub.local.asr.deleteSuccess',
          'deletion'
        ),
    });
  };

  const renderProgress = (state: LocalModelState): React.ReactNode => {
    const progress = state.progress;
    if (!progress) return null;
    const percent = localModelProgressPercent(progress);
    return (
      <div className='mt-10px rd-8px bg-[var(--fill-0)] px-10px py-9px'>
        <div className='mb-6px flex items-center justify-between gap-8px text-12px text-t-secondary'>
          <span>
            {progress.component === 'runtime'
              ? t('settings.modelHub.local.progress.runtime')
              : progress.component === 'asr_auxiliary'
                ? t('settings.modelHub.local.progress.asrAuxiliary')
                : t('settings.modelHub.local.progress.model')}
          </span>
          <span>{percent == null ? t('settings.modelHub.local.progress.preparing') : `${percent.toFixed(1)}%`}</span>
        </div>
        {percent != null && <Progress percent={percent} showText={false} strokeWidth={5} />}
        <div className='mt-5px flex items-center justify-between gap-8px text-11px text-t-secondary'>
          <span>
            {formatLocalModelBytes(progress.downloadedBytes, locale)} /{' '}
            {formatLocalModelBytes(progress.totalBytes, locale)}
          </span>
          {progress.bytesPerSecond > 0 && <span>{formatLocalModelRate(progress.bytesPerSecond, locale)}</span>}
        </div>
      </div>
    );
  };

  const loadFailed = (catalogError || statusError) && !catalog && !status;
  const installedCount = status?.models.filter((model) => model.installPhase === 'installed').length ?? 0;
  const runtime = status?.runtime;

  return (
    <div>
      {messageContext}
      <LocalModelCapabilitySummary
        items={[
          {
            label: t('settings.modelHub.local.asr.catalogTitle'),
            value: t('settings.modelHub.local.capabilityCenter.availableModels', { count: catalog?.length ?? 0 }),
          },
          {
            label: t('settings.modelHub.local.capabilityCenter.installedModels', { count: installedCount }),
            value: status?.activeModelId ?? t('settings.modelHub.local.capabilityCenter.runtimeOnDemand'),
            tone: status?.activeModelId ? 'success' : 'neutral',
          },
          {
            label: t('settings.modelHub.local.runtime.title'),
            value: runtime ? runtimeLabel(runtime.phase) : t('settings.modelHub.local.runtime.checking'),
            tone: runtime?.phase === 'failed' ? 'danger' : runtime?.phase === 'ready' ? 'success' : 'neutral',
          },
        ]}
      />

      <div className='mt-12px'>
        {isLoading && !catalog ? (
          <div className='flex items-center justify-center gap-8px py-48px text-13px text-t-secondary'>
            <Loading theme='outline' size='18' className='animate-spin' />
            {t('settings.modelHub.local.asr.loading')}
          </div>
        ) : loadFailed ? (
          <div className='flex flex-col items-center justify-center py-48px text-center'>
            <HeadsetOne theme='outline' size='40' className='text-t-tertiary mb-12px' />
            <div className='text-15px font-500 text-t-primary'>{t('settings.modelHub.local.asr.loadFailed')}</div>
            <div className='mt-5px text-12px text-t-secondary'>
              {t('settings.modelHub.local.asr.loadFailedHint')}
            </div>
          </div>
        ) : !catalog?.length ? (
          <div className='flex flex-col items-center justify-center py-48px text-center'>
            <HeadsetOne theme='outline' size='40' className='text-t-tertiary mb-12px' />
            <div className='text-15px font-500 text-t-primary'>{t('settings.modelHub.local.asr.empty')}</div>
            <div className='mt-5px text-12px text-t-secondary'>{t('settings.modelHub.local.asr.emptyHint')}</div>
          </div>
        ) : (
          <div className='space-y-12px'>
            <div className='flex items-center justify-between gap-12px'>
              <div>
                <div className='text-15px font-600 text-t-primary'>
                  {t('settings.modelHub.local.asr.catalogTitle')}
                </div>
                <div className='mt-3px text-12px leading-18px text-t-secondary'>
                  {t('settings.modelHub.local.asr.subtitle')}
                </div>
              </div>
              <Tag size='small'>{t('settings.modelHub.local.modelCount', { count: catalog.length })}</Tag>
            </div>

            {catalog.map((model) => {
              const state = status?.models.find((candidate) => candidate.modelId === model.id) ?? emptyState(model.id);
              const isActive = Boolean(status?.enabled && status.activeModelId === model.id);
              const primaryAction = localModelPrimaryAction(state, isActive);
              const actionPending = pendingAction?.endsWith(`:${model.id}`) ?? false;
              const otherTransferActive = status?.models.some(
                (candidate) =>
                  candidate.modelId !== model.id &&
                  (candidate.installPhase === 'downloading' || candidate.installPhase === 'verifying')
              );
              const startsTransfer =
                primaryAction === 'install' || primaryAction === 'resume' || primaryAction === 'retry';
              const runtimeBlocksInstall =
                status?.runtime.errorKind === 'unsupported_platform' ||
                status?.runtime.errorKind === 'runtime_unavailable';
              const actionDisabled =
                !status ||
                Boolean(statusError) ||
                primaryAction === 'none' ||
                pendingAction != null ||
                (startsTransfer && runtimeBlocksInstall) ||
                (Boolean(otherTransferActive) && startsTransfer);
              const deleteAllowed = canDeleteLocalModel(state, isActive);

              return (
                <section
                  key={model.id}
                  className={classNames(
                    'rd-12px border border-solid px-14px py-13px transition-colors shadow-[0_5px_18px_rgba(0,0,0,0.025)]',
                    isActive
                      ? 'border-[rgba(var(--primary-6),0.45)] bg-[rgba(var(--primary-6),0.025)]'
                      : 'border-[var(--color-border-2)] bg-[var(--color-bg-2)]'
                  )}
                >
                  <div className='flex items-start justify-between gap-12px flex-wrap'>
                    <div className='min-w-0 flex-1'>
                      <div className='flex items-center gap-7px flex-wrap'>
                        <span className='text-15px font-600 text-t-primary'>{model.name}</span>
                        {model.recommended && (
                          <Tag size='small' color='arcoblue'>
                            {model.engine === 'fun_asr_llama_cpp'
                              ? t('settings.modelHub.local.asr.chineseRecommended')
                              : t('settings.modelHub.local.recommended')}
                          </Tag>
                        )}
                        <Tag size='small' color={model.engine === 'fun_asr_llama_cpp' ? 'purple' : undefined}>
                          {asrEngineLabel(model.engine)}
                        </Tag>
                        {isActive && (
                          <Tag size='small' color='green'>
                            {t('settings.modelHub.local.active')}
                          </Tag>
                        )}
                        <Tag size='small' color={installPhaseColor(state.installPhase)}>
                          {phaseLabel(state.installPhase)}
                        </Tag>
                      </div>
                      <div className='mt-5px text-13px leading-20px text-t-secondary'>{model.description}</div>
                      <div className='mt-9px flex items-center gap-x-12px gap-y-5px flex-wrap text-12px text-t-secondary'>
                        <span>{model.modelSize}</span>
                        <span>{model.quantization}</span>
                        <span>
                          {t('settings.modelHub.local.metadata.download', {
                            size: formatLocalModelBytes(model.downloadSizeBytes, locale),
                          })}
                        </span>
                        <span>
                          {t('settings.modelHub.local.metadata.memory', {
                            size: formatLocalModelBytes(model.requiredMemoryBytes, locale),
                          })}
                        </span>
                      </div>
                    </div>

                    <Button
                      size='small'
                      type={primaryAction === 'deactivate' ? 'secondary' : 'primary'}
                      icon={actionIcon(primaryAction)}
                      loading={actionPending}
                      disabled={actionDisabled}
                      onClick={() => void invokePrimaryAction(model.id, primaryAction)}
                    >
                      {actionLabel(primaryAction)}
                    </Button>
                  </div>

                  <div className='mt-10px flex items-center gap-6px flex-wrap'>
                    {model.languages.map((language) => (
                      <Tag key={language} size='small'>
                        {language}
                      </Tag>
                    ))}
                    <span className='text-11px text-t-secondary'>{model.license}</span>
                    <span className='text-11px text-t-secondary'>
                      {t('settings.modelHub.local.metadata.source', { source: model.source })}
                    </span>
                    {deleteAllowed && (
                      <Tooltip content={t('settings.modelHub.local.action.delete')}>
                        <Button
                          size='mini'
                          type='text'
                          status='danger'
                          icon={<Delete theme='outline' size='13' />}
                          disabled={pendingAction != null || Boolean(statusError)}
                          onClick={() => confirmRemove(model.id, model.name)}
                          aria-label={t('settings.modelHub.local.deleteModelLabel', { model: model.name })}
                        >
                          {t('settings.modelHub.local.action.delete')}
                        </Button>
                      </Tooltip>
                    )}
                  </div>

                  {renderProgress(state)}
                  {state.installPhase === 'verifying' && !state.progress && (
                    <div className='mt-9px flex items-center gap-6px text-12px text-t-secondary'>
                      <Loading theme='outline' size='13' className='animate-spin' />
                      {t('settings.modelHub.local.progress.verifyingHint')}
                    </div>
                  )}
                  {state.errorKind && (
                    <div className='mt-9px rd-7px bg-[rgba(var(--danger-6),0.07)] px-9px py-7px text-12px text-[rgb(var(--danger-6))]'>
                      {errorLabel(state.errorKind)}
                      {state.message ? ` · ${state.message}` : ''}
                    </div>
                  )}
                </section>
              );
            })}
          </div>
        )}
      </div>
    </div>
  );
};

export default AsrModelsPanel;
