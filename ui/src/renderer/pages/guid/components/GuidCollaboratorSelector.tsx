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
import type { TModelRef } from '@/common/types/orchestrator/orchestratorTypes';
import NomiSelect from '@/renderer/components/base/NomiSelect';
import { encodePair, decodePair, useModelRange } from '@/renderer/pages/orchestrator/useModelRange';
import { iconColors } from '@/renderer/styles/colors';
import ocStyles from '@/renderer/pages/orchestrator/orchestratorComposer.module.css';

export interface GuidCollaboratorSelectorProps {
  /** Currently chosen collaborator (provider, model) pairs. */
  value: TModelRef[];
  onChange: (next: TModelRef[]) => void;
  /** The 主模型 — excluded from the collaborator list (it is always the lead and
   * already in the run's pool). */
  mainModel?: TModelRef | null;
  /** Optional extra class merged onto the trigger button so callers (e.g. the
   * conversation composer) can restyle the pill. */
  className?: string;
}

/** Case-insensitive substring match against an option's text label — mirrors
 * {@link OrchestratorComposer}'s picker filter (Arco types the option as a bare
 * `ReactNode`). */
const filterByLabel = (input: string, option: React.ReactNode): boolean => {
  const children = (option as React.ReactElement<{ children?: React.ReactNode }>)?.props?.children;
  return String(children ?? '')
    .toLowerCase()
    .includes(input.toLowerCase());
};

/**
 * GuidCollaboratorSelector —「协作模型」pill for the homepage 智能编排 entry.
 *
 * Sits next to the 主模型 picker (only in orchestration mode). It edits the
 * ADDITIONAL worker pool the lead/planner may assign per node by difficulty
 * (the 主模型 itself is always available and is excluded here). A compact
 * round pill (matching {@link GuidModelSelector}) opens a popover with a
 * provider-grouped multi-select; the empty state hints that the run then uses
 * just the 主模型. Visuals reuse the OrchestratorComposer popover tokens so the
 * two model surfaces read as one family.
 */
const GuidCollaboratorSelector: React.FC<GuidCollaboratorSelectorProps> = ({ value, onChange, mainModel, className }) => {
  const { t } = useTranslation();
  const { providers, getAvailableModels, formatModelLabel, allPairs, hasModels, isLoading } = useModelRange();
  const [open, setOpen] = useState(false);

  const availableKeys = useMemo(() => new Set(allPairs.map(encodePair)), [allPairs]);
  const mainKey = useMemo(() => {
    if (!mainModel) return null;
    const encodedMain = encodePair(mainModel);
    return availableKeys.has(encodedMain) ? encodedMain : null;
  }, [availableKeys, mainModel]);

  // The 主模型 is ALWAYS part of the run — it is the lead/planner AND a worker the
  // planner can assign to nodes — so it is PINNED into this selection: shown
  // selected and not removable here (change it in the 主模型 picker). The user only
  // adds EXTRA collaborators on top.
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
      const keys = ((v as string[]) ?? []).filter((k) => k !== mainKey);
      onChange(keys.map(decodePair));
    },
    [onChange, mainKey]
  );

  const label =
    value.length > 0
      ? t('guid.orchestration.collaborators.count', { count: value.length })
      : t('guid.orchestration.collaborators.label');

  const panel = (
    <div className={ocStyles.composerPopover}>
      <div className='flex flex-col gap-10px'>
        <div className='flex items-center gap-8px'>
          <Branch theme='outline' size='14' fill='rgb(var(--primary-6))' className='shrink-0' />
          <span className={ocStyles.composerPopoverTitle}>{t('guid.orchestration.collaborators.title')}</span>
        </div>

        {!hasModels ? (
          <div className='text-12px leading-18px text-warning-6'>{t('orchestrator.composer.noModels')}</div>
        ) : (
          <>
            <NomiSelect
              mode='multiple'
              value={encodedValue}
              onChange={handleChange}
              placeholder={t('guid.orchestration.collaborators.placeholder')}
              showSearch
              filterOption={filterByLabel}
              className='w-full'
            >
              {providers.map((p) => {
                const models = getAvailableModels(p);
                if (models.length === 0) return null;
                return (
                  <NomiSelect.OptGroup key={p.id} label={p.name || p.platform}>
                    {models.map((m) => {
                      const ref: TModelRef = { provider_id: p.id, model: m };
                      const key = encodePair(ref);
                      const isMainOpt = key === mainKey;
                      return (
                        <NomiSelect.Option key={key} value={key} disabled={isMainOpt}>
                          {formatModelLabel(p, m)}
                          {isMainOpt ? ` · ${t('guid.orchestration.collaborators.mainTag')}` : ''}
                        </NomiSelect.Option>
                      );
                    })}
                  </NomiSelect.OptGroup>
                );
              })}
            </NomiSelect>
            <div className={ocStyles.composerHint}>
              {value.length === 0
                ? t('guid.orchestration.collaborators.emptyHint')
                : t('guid.orchestration.collaborators.selectedHint', { count: value.length })}
            </div>
          </>
        )}
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
