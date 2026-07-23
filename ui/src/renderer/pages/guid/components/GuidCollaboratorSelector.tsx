/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import React, { useCallback, useMemo, useState } from 'react';
import { useTranslation } from 'react-i18next';
import classNames from 'classnames';
import { Button, Dropdown } from '@arco-design/web-react';
import { Branch, Down } from '@icon-park/react';
import {
  MAX_AGENT_EXECUTION_MODELS,
  type TExecutionModelRef,
} from '@/common/types/agentExecution/agentExecutionTypes';
import NomiSelect from '@/renderer/components/base/NomiSelect';
import {
  encodePair,
  decodePair,
  useExecutionModelPool,
} from '@/renderer/pages/conversation/execution/useExecutionModelPool';
import { iconColors } from '@/renderer/styles/colors';
import ocStyles from '@/renderer/pages/conversation/execution/executionPlanEditor.module.css';
import GuidCollaborationTemplatePicker from './GuidCollaborationTemplatePicker';
import type { AppliedCollaborationTemplate } from '@/renderer/components/collaboration/collaborationTemplateModel';
import { useModelSelectorProviderLabel } from '@/renderer/hooks/agent/useModelSelectorProviderLabel';

export type GuidAppliedCollaborationTemplate = AppliedCollaborationTemplate;

export interface GuidCollaboratorSelectorProps {
  /** Currently chosen collaborator (provider, model) pairs. */
  value: TExecutionModelRef[];
  onChange: (next: TExecutionModelRef[]) => void;
  /** The main model, shown as a pinned participant and not persisted twice. */
  mainModel?: TExecutionModelRef | null;
  selectedTemplate?: GuidAppliedCollaborationTemplate | null;
  workDir?: string;
  onTemplateApply: (template: GuidAppliedCollaborationTemplate) => void;
  onTemplateClear: () => void;
  /** Optional extra class merged onto the trigger button so callers (e.g. the
   * conversation composer) can restyle the pill. */
  className?: string;
}

/** Case-insensitive substring match against an option's text label. */
const filterByLabel = (input: string, option: React.ReactNode): boolean => {
  const children = (option as React.ReactElement<{ children?: React.ReactNode }>)?.props?.children;
  return String(children ?? '')
    .toLowerCase()
    .includes(input.toLowerCase());
};

/** Selects additional models that may participate in a Nomi collaboration. */
const GuidCollaboratorSelector: React.FC<GuidCollaboratorSelectorProps> = ({
  value,
  onChange,
  mainModel,
  selectedTemplate,
  workDir,
  onTemplateApply,
  onTemplateClear,
  className,
}) => {
  const { t } = useTranslation();
  const { providers, getAvailableModels, formatModelLabel, allPairs, hasModels, isLoading } = useExecutionModelPool();
  const [open, setOpen] = useState(false);
  const providerLabel = useModelSelectorProviderLabel();

  const availableKeys = useMemo(() => new Set(allPairs.map(encodePair)), [allPairs]);
  const mainKey = useMemo(() => {
    if (!mainModel) return null;
    const encodedMain = encodePair(mainModel);
    return availableKeys.has(encodedMain) ? encodedMain : null;
  }, [availableKeys, mainModel]);

  // The main model always participates, so show it pinned and persist only the
  // additional collaborator models.
  const encodedValue = useMemo(() => {
    if (isLoading) return [];
    const collab = value.map(encodePair);
    return mainKey ? Array.from(new Set([mainKey, ...collab])) : collab;
  }, [isLoading, value, mainKey]);

  const handleChange = useCallback(
    (v: unknown) => {
      // Strip the pinned 主模型 — it is implicit (owned by the 主模型 picker) and is
      // never persisted as a collaborator. Re-pinned on the next render via
      // `encodedValue`, so it can't be removed from the pool here.
      const collaboratorLimit = MAX_AGENT_EXECUTION_MODELS - (mainKey ? 1 : 0);
      const keys = Array.from(new Set(((v as string[]) ?? []).filter((k) => k !== mainKey))).slice(
        0,
        collaboratorLimit,
      );
      onChange(keys.map(decodePair));
    },
    [onChange, mainKey],
  );

  const label = selectedTemplate
    ? selectedTemplate.name
    : value.length > 0
      ? t('guid.collaboration.models.count', { count: value.length })
      : t('guid.collaboration.models.label');

  const panel = (
    <div className={ocStyles.composerPopover}>
      <div className='flex flex-col gap-10px'>
        <div className='flex items-center gap-8px'>
          <Branch theme='outline' size='14' fill='rgb(var(--primary-6))' className='shrink-0' />
          <span className={ocStyles.composerPopoverTitle}>{t('guid.collaboration.models.title')}</span>
        </div>

        {!hasModels ? (
          <div className='text-12px leading-18px text-warning-6'>
            {t('agentExecution.editor.noModels', {
              defaultValue: '暂无可用模型',
            })}
          </div>
        ) : (
          <>
            <NomiSelect
              mode='multiple'
              value={encodedValue}
              onChange={handleChange}
              disabled={Boolean(selectedTemplate)}
              placeholder={t('guid.collaboration.models.placeholder')}
              showSearch
              filterOption={filterByLabel}
              className='w-full'
            >
              {providers.map((p) => {
                const models = getAvailableModels(p);
                if (models.length === 0) return null;
                return (
                  <NomiSelect.OptGroup key={p.id} label={providerLabel(p)}>
                    {models.map((m) => {
                      const ref: TExecutionModelRef = {
                        provider_id: p.id,
                        model: m,
                      };
                      const key = encodePair(ref);
                      const isMainOpt = key === mainKey;
                      const isSelected = encodedValue.includes(key);
                      const modelLimitReached = encodedValue.length >= MAX_AGENT_EXECUTION_MODELS;
                      return (
                        <NomiSelect.Option
                          key={key}
                          value={key}
                          disabled={isMainOpt || (modelLimitReached && !isSelected)}
                        >
                          {formatModelLabel(p, m)}
                          {isMainOpt ? ` · ${t('guid.collaboration.models.mainTag')}` : ''}
                        </NomiSelect.Option>
                      );
                    })}
                  </NomiSelect.OptGroup>
                );
              })}
            </NomiSelect>
            <div className={ocStyles.composerHint}>
              {selectedTemplate
                ? t('collaboration.template.activeHint', {
                    defaultValue: '将完整使用方案中的 {{count}} 位协作者；不会截断成当前模型选择。',
                    count: selectedTemplate.participantCount,
                  })
                : value.length === 0
                ? t('guid.collaboration.models.emptyHint')
                : t('guid.collaboration.models.selectedHint', {
                    count: value.length,
                  })}
            </div>
          </>
        )}
        <GuidCollaborationTemplatePicker
          visible={open}
          selectedTemplateId={selectedTemplate?.execution_template_id ?? null}
          models={value}
          mainModel={mainModel}
          workDir={workDir}
          onApply={onTemplateApply}
          onClear={onTemplateClear}
        />
      </div>
    </div>
  );

  return (
    <Dropdown trigger='click' popupVisible={open} onVisibleChange={setOpen} droplist={panel} position='tr'>
      <Button
        className={classNames('sendbox-model-btn guid-config-btn', className)}
        shape='round'
        size='small'
        disabled={isLoading}
        data-testid='guid-collaborator-selector'
      >
        <span className='flex items-center gap-6px min-w-0'>
          <Branch theme='outline' size='14' fill={iconColors.secondary} className='shrink-0' />
          <span className='truncate'>{label}</span>
          <Down theme='outline' size='12' fill={iconColors.secondary} className='shrink-0' />
        </span>
      </Button>
    </Dropdown>
  );
};

export default GuidCollaboratorSelector;
