/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import React from 'react';
import { useTranslation } from 'react-i18next';
import classNames from 'classnames';
import { Button, Modal, Progress, Tag, Tooltip } from '@arco-design/web-react';
import { CheckOne, Delete, Download, Loading, Pause, PlayOne } from '@icon-park/react';
import type {
  ImageModelCatalogEntry,
  ImageModelComponent,
  ImageModelInstallPhase,
  ImageModelRuntimePhase,
  ImageModelState,
} from '@/common/types/provider/imageModelService';
import type { LocalModelErrorKind } from '@/common/types/provider/localModelService';
import { useArcoMessage } from '@/renderer/utils/ui/useArcoMessage';
import LocalModelCapabilitySummary from './LocalModelCapabilitySummary';
import LocalModelDetails from './LocalModelDetails';
import { detailsForcedOpen } from './localModelCapabilityView';
import { formatLocalModelBytes, formatLocalModelRate } from './localModelView';
import {
  canDeleteImageModel,
  componentProgressFor,
  IMAGE_MODEL_COMPONENTS,
  imageModelPrimaryAction,
  imageModelProgressPercent,
  imageModelProgressTotals,
  stateForImageModel,
  type ImageModelPrimaryAction,
} from './imageModelView';
import { useLocalImageModels } from './useLocalImageModels';

export interface ImageModelsPanelProps {
  controller: ReturnType<typeof useLocalImageModels>;
  className?: string;
}

const phaseColor = (phase: ImageModelInstallPhase): string | undefined => {
  if (phase === 'installed') return 'green';
  if (phase === 'downloading' || phase === 'verifying' || phase === 'extracting') return 'blue';
  if (phase === 'paused') return 'orange';
  if (phase === 'failed') return 'red';
  return undefined;
};

const ImageModelsPanel: React.FC<ImageModelsPanelProps> = ({ controller, className }) => {
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
    pause,
    resume,
    remove,
  } = controller;
  const locale = i18n.resolvedLanguage ?? i18n.language;
  const unsupported = Boolean(
    status?.models.some(
      (model) =>
        model.errorKind === 'unsupported_platform' ||
        model.componentProgress.some((progress) => progress.errorKind === 'unsupported_platform')
    )
  );

  const phaseLabel = (phase: ImageModelInstallPhase): string => {
    switch (phase) {
      case 'not_installed':
        return t('settings.modelHub.local.image.phase.notInstalled');
      case 'downloading':
        return t('settings.modelHub.local.image.phase.downloading');
      case 'verifying':
        return t('settings.modelHub.local.image.phase.verifying');
      case 'extracting':
        return t('settings.modelHub.local.image.phase.extracting');
      case 'installed':
        return t('settings.modelHub.local.image.phase.installed');
      case 'paused':
        return t('settings.modelHub.local.image.phase.paused');
      case 'failed':
        return t('settings.modelHub.local.image.phase.failed');
    }
  };

  const errorLabel = (kind: LocalModelErrorKind | null): string => {
    switch (kind) {
      case 'network':
        return t('settings.modelHub.local.image.error.network');
      case 'insufficient_space':
        return t('settings.modelHub.local.image.error.insufficientSpace');
      case 'checksum_mismatch':
        return t('settings.modelHub.local.image.error.checksumMismatch');
      case 'unsupported_platform':
        return t('settings.modelHub.local.image.error.unsupportedPlatform');
      case 'runtime_unavailable':
        return t('settings.modelHub.local.image.error.runtimeUnavailable');
      case 'busy':
        return t('settings.modelHub.local.image.error.busy');
      case 'not_found':
        return t('settings.modelHub.local.image.error.notFound');
      case 'unknown':
      case null:
        return t('settings.modelHub.local.image.error.unknown');
    }
  };

  const componentLabel = (component: ImageModelComponent): string => {
    switch (component) {
      case 'runtime':
        return t('settings.modelHub.local.image.component.runtime');
      case 'diffusion_model':
        return t('settings.modelHub.local.image.component.diffusionModel');
      case 'text_encoder':
        return t('settings.modelHub.local.image.component.textEncoder');
      case 'vae':
        return t('settings.modelHub.local.image.component.vae');
    }
  };

  const runtimeLabel = (phase: ImageModelRuntimePhase | undefined): string => {
    if (unsupported) return t('settings.modelHub.local.image.runtime.unsupported');
    switch (phase) {
      case 'ready':
        return t('settings.modelHub.local.image.runtime.ready');
      case 'busy':
        return t('settings.modelHub.local.image.runtime.busy');
      case 'failed':
        return t('settings.modelHub.local.image.runtime.failed');
      case 'unavailable':
        return status?.artifactsReady
          ? t('settings.modelHub.local.image.runtime.integrityPending')
          : t('settings.modelHub.local.image.runtime.onDemand');
      case undefined:
        return t('settings.modelHub.local.image.runtime.checking');
    }
  };

  const actionLabel = (action: ImageModelPrimaryAction): string => {
    switch (action) {
      case 'install':
        return t('settings.modelHub.local.image.action.install');
      case 'pause':
        return t('settings.modelHub.local.image.action.pause');
      case 'resume':
        return t('settings.modelHub.local.image.action.resume');
      case 'retry':
        return t('settings.modelHub.local.image.action.retry');
      case 'none':
        return t('settings.modelHub.local.image.phase.installed');
    }
  };

  const actionIcon = (action: ImageModelPrimaryAction): React.ReactNode => {
    switch (action) {
      case 'install':
      case 'retry':
        return <Download theme='outline' size='14' />;
      case 'pause':
        return <Pause theme='outline' size='14' />;
      case 'resume':
        return <PlayOne theme='outline' size='14' />;
      case 'none':
        return null;
    }
  };

  const runAction = async (
    action: () => Promise<unknown>,
    successKey: string,
    context: string
  ): Promise<void> => {
    try {
      await action();
      message.success(t(successKey));
    } catch (error) {
      console.error(`Local image model ${context} failed:`, error);
      message.error(t('settings.modelHub.local.image.actionFailed'));
    }
  };

  const invokePrimaryAction = async (
    modelId: string,
    action: ImageModelPrimaryAction
  ): Promise<void> => {
    switch (action) {
      case 'install':
      case 'retry':
        await runAction(
          () => install(modelId),
          'settings.modelHub.local.image.installSuccess',
          'installation'
        );
        return;
      case 'pause':
        await runAction(() => pause(modelId), 'settings.modelHub.local.image.pauseSuccess', 'pause');
        return;
      case 'resume':
        await runAction(() => resume(modelId), 'settings.modelHub.local.image.resumeSuccess', 'resume');
        return;
      case 'none':
        return;
    }
  };

  const confirmRemove = (model: ImageModelCatalogEntry): void => {
    Modal.confirm({
      title: t('settings.modelHub.local.image.deleteConfirmTitle'),
      content: t('settings.modelHub.local.image.deleteConfirmContent', { model: model.name }),
      okText: t('settings.modelHub.local.image.action.delete'),
      cancelText: t('common.cancel'),
      okButtonProps: { status: 'danger' },
      onOk: () =>
        runAction(
          () => remove(model.id),
          'settings.modelHub.local.image.deleteSuccess',
          'deletion'
        ),
    });
  };

  const renderBundleProgress = (state: ImageModelState): React.ReactNode => {
    const totals = imageModelProgressTotals(state);
    const overallPercent = imageModelProgressPercent(totals.downloadedBytes, totals.totalBytes);
    const paused = state.installPhase === 'paused';
    return (
      <div className='mt-11px rd-9px bg-[var(--fill-0)] px-11px py-10px'>
        <div className='flex items-center justify-between gap-8px text-12px font-500 text-t-primary'>
          <span>{t('settings.modelHub.local.image.progress.bundle')}</span>
          <span>
            {overallPercent == null
              ? t('settings.modelHub.local.image.progress.preparing')
              : `${overallPercent.toFixed(1)}%`}
          </span>
        </div>
        {overallPercent != null && (
          <Progress
            className='mt-6px'
            percent={overallPercent}
            showText={false}
            strokeWidth={5}
            color={paused ? 'rgb(var(--warning-6))' : undefined}
          />
        )}
        <div className='mt-5px flex items-center justify-between gap-8px text-11px text-t-secondary'>
          <span>
            {formatLocalModelBytes(totals.downloadedBytes, locale)} /{' '}
            {formatLocalModelBytes(totals.totalBytes, locale)}
          </span>
          {totals.bytesPerSecond > 0 && <span>{formatLocalModelRate(totals.bytesPerSecond, locale)}</span>}
        </div>

        <div className='mt-9px space-y-7px border-t border-solid border-[var(--color-border-2)] pt-8px'>
          {IMAGE_MODEL_COMPONENTS.map((component) => {
            const progress = componentProgressFor(state, component);
            const percent = imageModelProgressPercent(progress.downloadedBytes, progress.totalBytes);
            const working =
              progress.installPhase === 'downloading' ||
              progress.installPhase === 'verifying' ||
              progress.installPhase === 'extracting';
            return (
              <div key={component} className='rd-7px bg-[var(--color-bg-2)] px-9px py-8px'>
                <div className='flex items-center justify-between gap-8px flex-wrap'>
                  <div className='flex items-center gap-6px min-w-0'>
                    {progress.installPhase === 'installed' ? (
                      <CheckOne theme='filled' size='13' className='shrink-0 text-[rgb(var(--success-6))]' />
                    ) : working ? (
                      <Loading theme='outline' size='13' className='shrink-0 animate-spin text-[rgb(var(--primary-6))]' />
                    ) : null}
                    <span className='text-12px font-500 text-t-primary'>{componentLabel(component)}</span>
                    <Tag size='small' color={phaseColor(progress.installPhase)}>
                      {phaseLabel(progress.installPhase)}
                    </Tag>
                  </div>
                  <span className='text-11px text-t-secondary'>
                    {formatLocalModelBytes(progress.downloadedBytes, locale)} /{' '}
                    {formatLocalModelBytes(progress.totalBytes, locale)}
                    {progress.bytesPerSecond > 0
                      ? ` · ${formatLocalModelRate(progress.bytesPerSecond, locale)}`
                      : ''}
                  </span>
                </div>
                {percent != null && progress.installPhase !== 'not_installed' && (
                  <Progress
                    className='mt-6px'
                    percent={percent}
                    showText={false}
                    strokeWidth={3}
                    color={progress.installPhase === 'paused' ? 'rgb(var(--warning-6))' : undefined}
                  />
                )}
                {progress.errorKind && (
                  <div className='mt-5px text-11px leading-17px text-[rgb(var(--danger-6))]'>
                    {errorLabel(progress.errorKind)}
                  </div>
                )}
              </div>
            );
          })}
        </div>
      </div>
    );
  };

  const loadFailed = (catalogError || statusError) && !catalog && !status;
  const installedCount = status?.models.filter((model) => model.installPhase === 'installed').length ?? 0;

  return (
    <div className={classNames('space-y-12px', className)}>
      {messageContext}
      <LocalModelCapabilitySummary
        items={[
          {
            label: t('settings.modelHub.local.image.title'),
            value: t('settings.modelHub.local.capabilityCenter.availableModels', { count: catalog?.length ?? 0 }),
          },
          {
            label: t('settings.modelHub.local.image.readiness.artifacts'),
            value: t('settings.modelHub.local.capabilityCenter.installedModels', { count: installedCount }),
            tone: status?.artifactsReady ? 'success' : 'neutral',
          },
          {
            label: t('settings.modelHub.local.image.readiness.runtime'),
            value: unsupported
              ? t('settings.modelHub.local.image.runtime.unsupported')
              : runtimeLabel(status?.runtimePhase),
            tone: unsupported || status?.runtimePhase === 'failed'
              ? 'danger'
              : status?.inferenceReady
                ? 'success'
                : status?.artifactsReady
                  ? 'warning'
                  : 'neutral',
          },
        ]}
      />

      {unsupported ? (
        <div className='mt-8px rd-8px bg-[rgba(var(--danger-6),0.07)] px-10px py-8px text-12px leading-18px text-[rgb(var(--danger-6))]'>
          {t('settings.modelHub.local.image.readiness.unsupportedHint')}
        </div>
      ) : status?.runtimePhase === 'failed' ? (
        <div className='mt-8px rd-8px bg-[rgba(var(--danger-6),0.07)] px-10px py-8px text-12px leading-18px text-[rgb(var(--danger-6))]'>
          {t('settings.modelHub.local.image.readiness.runtimeFailedHint')}
        </div>
      ) : status?.artifactsReady && !status.inferenceReady ? (
        <div className='mt-8px rd-8px bg-[rgba(var(--warning-6),0.08)] px-10px py-8px text-12px leading-18px text-[rgb(var(--warning-6))]'>
          {t('settings.modelHub.local.image.readiness.integrityPendingHint')}
        </div>
      ) : status?.inferenceReady ? (
        <div className='mt-8px rd-8px bg-[rgba(var(--success-6),0.08)] px-10px py-8px text-12px leading-18px text-[rgb(var(--success-6))]'>
          {status.runtimePhase === 'busy'
            ? t('settings.modelHub.local.image.readiness.generatingHint')
            : t('settings.modelHub.local.image.readiness.creationReadyHint')}
        </div>
      ) : null}

      <div>
        {isLoading && !catalog ? (
          <div className='flex items-center justify-center gap-7px py-24px text-12px text-t-secondary'>
            <Loading theme='outline' size='16' className='animate-spin' />
            {t('settings.modelHub.local.image.loading')}
          </div>
        ) : loadFailed ? (
          <div className='py-24px text-center'>
            <div className='text-13px font-500 text-t-primary'>{t('settings.modelHub.local.image.loadFailed')}</div>
            <div className='mt-4px text-12px text-t-secondary'>{t('settings.modelHub.local.image.loadFailedHint')}</div>
          </div>
        ) : !catalog?.length ? (
          <div className='py-24px text-center'>
            <div className='text-13px font-500 text-t-primary'>{t('settings.modelHub.local.image.empty')}</div>
            <div className='mt-4px text-12px text-t-secondary'>{t('settings.modelHub.local.image.emptyHint')}</div>
          </div>
        ) : (
          <div className='space-y-10px'>
            {catalog.map((model) => {
              const state = stateForImageModel(status?.models, model.id);
              const action = imageModelPrimaryAction(state);
              const actionPending = pendingAction?.endsWith(`:${model.id}`) ?? false;
              const actionDisabled =
                !status ||
                Boolean(statusError) ||
                pendingAction != null ||
                unsupported ||
                status.runtimePhase === 'busy';
              const deleteAllowed = canDeleteImageModel(state);

              return (
                <article
                  key={model.id}
                  className='rd-11px border border-solid border-[var(--color-border-2)] bg-[var(--color-bg-2)] px-13px py-12px shadow-[0_5px_18px_rgba(0,0,0,0.025)]'
                >
                  <div className='flex items-start justify-between gap-12px flex-wrap'>
                    <div className='min-w-0 flex-1'>
                      <div className='flex items-center gap-6px flex-wrap'>
                        <span className='text-14px font-600 text-t-primary'>{model.name}</span>
                        {model.recommended && (
                          <Tag size='small' color='arcoblue'>
                            {t('settings.modelHub.local.image.recommended')}
                          </Tag>
                        )}
                        <Tag size='small' color={phaseColor(state.installPhase)}>
                          {phaseLabel(state.installPhase)}
                        </Tag>
                      </div>
                      <div className='mt-5px text-12px leading-18px text-t-secondary'>
                        {model.id === 'z-image-turbo-q3-k'
                          ? t('settings.modelHub.local.image.catalogDescription')
                          : model.description}
                      </div>
                      <div className='mt-8px flex items-center gap-x-11px gap-y-5px flex-wrap text-11px text-t-secondary'>
                        <span>{model.format}</span>
                        <span className='font-600 text-t-primary'>
                          {t('settings.modelHub.local.image.metadata.downloadApprox', {
                            size: formatLocalModelBytes(model.downloadSizeBytes, locale),
                          })}
                        </span>
                        <span>
                          {t('settings.modelHub.local.image.metadata.memory', {
                            size: formatLocalModelBytes(model.requiredMemoryBytes, locale),
                          })}
                        </span>
                      </div>
                    </div>

                    <div className='flex items-center gap-7px shrink-0'>
                      {action !== 'none' && (
                        <Button
                          size='small'
                          type='primary'
                          icon={actionIcon(action)}
                          loading={actionPending}
                          disabled={actionDisabled}
                          onClick={() => void invokePrimaryAction(model.id, action)}
                        >
                          {actionLabel(action)}
                        </Button>
                      )}
                    </div>
                  </div>

                  <LocalModelDetails
                    forcedOpen={detailsForcedOpen(state.installPhase, Boolean(state.errorKind))}
                  >
                    <div className='flex items-center gap-6px flex-wrap'>
                      {model.components.map((component) => (
                        <Tag key={component} size='small'>
                          {componentLabel(component)}
                        </Tag>
                      ))}
                      <span className='text-11px text-t-secondary'>
                        {t('settings.modelHub.local.image.metadata.license', { license: model.license })}
                      </span>
                      <span className='text-11px text-t-secondary'>
                        {t('settings.modelHub.local.image.metadata.source', { source: model.source })}
                      </span>
                      {deleteAllowed && (
                        <Tooltip content={t('settings.modelHub.local.image.action.delete')}>
                          <Button
                            size='mini'
                            type='text'
                            status='danger'
                            icon={<Delete theme='outline' size='13' />}
                            disabled={
                              pendingAction != null ||
                              Boolean(statusError) ||
                              status?.runtimePhase === 'busy'
                            }
                            onClick={() => confirmRemove(model)}
                            aria-label={t('settings.modelHub.local.image.deleteModelLabel', { model: model.name })}
                          >
                            {t('settings.modelHub.local.image.action.delete')}
                          </Button>
                        </Tooltip>
                      )}
                    </div>
                    {model.notice && (
                      <div className='mt-9px rd-8px border border-solid border-[rgba(var(--warning-6),0.24)] bg-[rgba(var(--warning-6),0.06)] px-10px py-8px'>
                        <div className='text-11px font-600 text-t-primary'>
                          {t('settings.modelHub.local.image.notice.title')}
                        </div>
                        <div className='mt-2px text-11px leading-17px text-t-secondary'>
                          {t('settings.modelHub.local.image.notice.vae')}
                        </div>
                      </div>
                    )}
                    {renderBundleProgress(state)}
                    {state.errorKind && state.errorKind !== 'unsupported_platform' && (
                      <div className='mt-9px rd-7px bg-[rgba(var(--danger-6),0.07)] px-9px py-7px text-12px text-[rgb(var(--danger-6))]'>
                        {errorLabel(state.errorKind)}
                      </div>
                    )}
                  </LocalModelDetails>
                </article>
              );
            })}
          </div>
        )}
      </div>
    </div>
  );
};

export default ImageModelsPanel;
