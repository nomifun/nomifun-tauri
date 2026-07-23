/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 *
 * GuidPresetEditorHost — hosts the editor modal tree (PresetEditDrawer +
 * DeletePresetModal + SkillConfirmModals), the openPresetDetails
 * registration, and the "selected preset example prompts" rendering.
 *
 * Extracted from PresetSelectionArea so the entry page can render these
 * independently of the retired preset card grid.
 */

import coworkSvg from '@/renderer/assets/icons/cowork.svg';
import { useDetectedAgents, usePresetEditor, usePresetList, usePresetTags } from '@/renderer/hooks/preset';
import PresetEditDrawer from '@/renderer/pages/settings/PresetSettings/PresetEditDrawer';
import DeletePresetModal from '@/renderer/pages/settings/PresetSettings/DeletePresetModal';
import SkillConfirmModals from '@/renderer/pages/settings/PresetSettings/SkillConfirmModals';
import { resolveAvatarImageSrc } from '@/renderer/pages/settings/PresetSettings/presetUtils';
import { useArcoMessage } from '@/renderer/utils/ui/useArcoMessage';
import styles from '../index.module.css';
import type { AvailableAgent, EffectiveAgentInfo } from '../types';
import type { Preset } from '@/common/types/agent/presetTypes';
import React, { useCallback, useLayoutEffect, useMemo } from 'react';
import { useTranslation } from 'react-i18next';

export interface GuidPresetEditorHostProps {
  presets: Preset[];
  localeKey: string;
  selectedAgentKey?: string;
  selectedAgentInfo: AvailableAgent | undefined;
  currentEffectiveAgentInfo: EffectiveAgentInfo;
  onSetInput: (text: string) => void;
  onFocusInput: () => void;
  onRegisterOpenDetails: (openDetails: (() => void) | null) => void;
}

const avatarImageMap: Record<string, string> = {
  'cowork.svg': coworkSvg,
  '\u{1F6E0}\u{FE0F}': coworkSvg,
};

const GuidPresetEditorHost: React.FC<GuidPresetEditorHostProps> = ({
  presets,
  localeKey,
  selectedAgentKey,
  selectedAgentInfo,
  currentEffectiveAgentInfo,
  onSetInput,
  onFocusInput,
  onRegisterOpenDetails,
}) => {
  const { t } = useTranslation();
  const [agentMessage, agentMessageContext] = useArcoMessage({ maxCount: 10 });

  // Internal usePresetList owns the drawer editor's working state.
  const { activePresetId, setActivePresetId, activePreset, isExtensionPreset, loadPresets } =
    usePresetList();
  const { availableBackends, refreshAgentDetection } = useDetectedAgents();
  const tags = usePresetTags();

  const editor = usePresetEditor({
    localeKey,
    activePreset,
    isExtensionPreset,
    setActivePresetId,
    loadPresets,
    refreshAgentDetection,
    message: agentMessage,
  });

  const editAvatarImage = resolveAvatarImageSrc(editor.editAvatar, avatarImageMap);

  // ── openPresetDetails registration ──
  const openPresetDetails = useCallback(() => {
    const presetId = selectedAgentInfo?.preset_id
      ?? (selectedAgentKey?.startsWith('preset:') ? selectedAgentKey.slice(7) : null);
    if (!presetId) {
      agentMessage.warning(
        t('common.failed', { defaultValue: 'Failed' }) +
          `: ${t('settings.editPreset', { defaultValue: 'Preset Details' })}`
      );
      return;
    }

    const targetPreset = presets.find((preset) => preset.preset_id === presetId);
    if (!targetPreset) {
      agentMessage.warning(
        t('common.failed', { defaultValue: 'Failed' }) +
          `: ${t('settings.editPreset', { defaultValue: 'Preset Details' })}`
      );
      return;
    }

    void editor.handleEdit(targetPreset);
  }, [agentMessage, presets, editor, selectedAgentInfo?.preset_id, selectedAgentKey, t]);

  useLayoutEffect(() => {
    onRegisterOpenDetails(openPresetDetails);
  }, [onRegisterOpenDetails, openPresetDetails]);

  // ── Resolved agent (shared between description block and promptsNode) ──
  const resolvedAgent = useMemo(() => {
    if (!selectedAgentInfo?.preset_id) return null;
    return presets.find((preset) => preset.preset_id === selectedAgentInfo.preset_id) ?? null;
  }, [presets, selectedAgentInfo?.preset_id]);

  // ── Description + details link block ──
  const descriptionNode = useMemo(() => {
    if (!resolvedAgent) return null;
    const description = resolvedAgent.description_i18n?.[localeKey] || resolvedAgent.description;
    if (!description) return null;
    return (
      <div className='flex flex-col gap-6px'>
        <p className='text-13px text-3 leading-relaxed mb-0'>{description}</p>
        <span
          className='text-12px text-primary-6 cursor-pointer hover:underline inline-block w-fit'
          onClick={openPresetDetails}
        >
          {t('settings.editPreset', { defaultValue: '设定详情' })}
        </span>
      </div>
    );
  }, [resolvedAgent, localeKey, openPresetDetails, t]);

  // ── Example prompts rendering ──
  const promptsNode = useMemo(() => {
    if (!resolvedAgent) return null;
    const prompts = resolvedAgent.examples;
    if (!prompts || prompts.length === 0) return null;
    return (
      <div className='mt-16px'>
        <div className={styles.presetPromptHint}>
          {t('guid.promptExamplesHint', { defaultValue: 'Try these example prompts:' })}
        </div>
        <div className='flex flex-wrap gap-8px mt-12px'>
          {prompts.map((prompt: string, index: number) => (
            <div
              key={index}
              className={`${styles.presetPromptChip} px-12px py-6px text-2 text-13px rd-16px cursor-pointer transition-colors shadow-sm`}
              onClick={() => {
                onSetInput(prompt);
                onFocusInput();
              }}
            >
              {prompt}
            </div>
          ))}
        </div>
      </div>
    );
  }, [resolvedAgent, localeKey, onFocusInput, onSetInput, t]);

  // ── Fallback notice ──
  const fallbackNotice = currentEffectiveAgentInfo.isFallback ? (
    <div
      className='mb-12px px-12px py-8px rd-8px text-12px flex items-center gap-8px'
      style={{
        background: 'rgb(var(--warning-1))',
        border: '1px solid rgb(var(--warning-3))',
        color: 'rgb(var(--warning-6))',
      }}
    >
      <span>
        {t('guid.agentFallbackNotice', {
          original:
            currentEffectiveAgentInfo.originalType.charAt(0).toUpperCase() +
            currentEffectiveAgentInfo.originalType.slice(1),
          fallback:
            currentEffectiveAgentInfo.agent_type.charAt(0).toUpperCase() +
            currentEffectiveAgentInfo.agent_type.slice(1),
          defaultValue: `${currentEffectiveAgentInfo.originalType.charAt(0).toUpperCase() + currentEffectiveAgentInfo.originalType.slice(1)} is unavailable, using ${currentEffectiveAgentInfo.agent_type.charAt(0).toUpperCase() + currentEffectiveAgentInfo.agent_type.slice(1)} instead.`,
        })}
      </span>
    </div>
  ) : null;

  // ── Modal tree ──
  const modalTree = (
    <>
      {agentMessageContext}
      <PresetEditDrawer
        editVisible={editor.editVisible}
        setEditVisible={editor.setEditVisible}
        isCreating={editor.isCreating}
        editName={editor.editName}
        setEditName={editor.setEditName}
        editDescription={editor.editDescription}
        setEditDescription={editor.setEditDescription}
        editRoutingDescription={editor.editRoutingDescription}
        setEditRoutingDescription={editor.setEditRoutingDescription}
        editAvatar={editor.editAvatar}
        setEditAvatar={editor.setEditAvatar}
        editAvatarImage={editAvatarImage}
        editAgents={editor.editAgents}
        setEditAgents={editor.setEditAgents}
        editModels={editor.editModels}
        setEditModels={editor.setEditModels}
        editTargets={editor.editTargets}
        setEditTargets={editor.setEditTargets}
        fallbackAllowed={editor.fallbackAllowed}
        setFallbackAllowed={editor.setFallbackAllowed}
        autoSelectable={editor.autoSelectable}
        setAutoSelectable={editor.setAutoSelectable}
        knowledgePolicy={editor.knowledgePolicy}
        setKnowledgePolicy={editor.setKnowledgePolicy}
        knowledgeBaseIds={editor.knowledgeBaseIds}
        setKnowledgeBaseIds={editor.setKnowledgeBaseIds}
        editContext={editor.editContext}
        setEditContext={editor.setEditContext}
        promptViewMode={editor.promptViewMode}
        setPromptViewMode={editor.setPromptViewMode}
        availableSkills={editor.availableSkills}
        selectedSkills={editor.selectedSkills}
        setSelectedSkills={editor.setSelectedSkills}
        pendingSkills={editor.pendingSkills}
        customSkills={editor.customSkills}
        setDeletePendingSkillName={editor.setDeletePendingSkillName}
        setDeleteCustomSkillName={editor.setDeleteCustomSkillName}
        builtinAutoSkills={editor.builtinAutoSkills}
        disabledBuiltinSkills={editor.disabledBuiltinSkills}
        setDisabledBuiltinSkills={editor.setDisabledBuiltinSkills}
        editAudienceTags={editor.editAudienceTags}
        setEditAudienceTags={editor.setEditAudienceTags}
        editScenarioTags={editor.editScenarioTags}
        setEditScenarioTags={editor.setEditScenarioTags}
        audienceTags={tags.audienceTags}
        scenarioTags={tags.scenarioTags}
        onCreateTag={tags.createTag}
        readOnly={
          editor.isCreating ? false : activePreset?.source === 'builtin' || isExtensionPreset(activePreset)
        }
        localeKey={localeKey}
        activePreset={activePreset}
        activePresetId={activePresetId}
        isExtensionPreset={isExtensionPreset}
        availableBackends={availableBackends}
        handleSave={editor.handleSave}
        onImportAgentSkills={editor.handleImportAgentSkills}
        handleDeleteClick={editor.handleDeleteClick}
        handleDuplicate={(preset) => void editor.handleDuplicate(preset)}
      />
      <DeletePresetModal
        visible={editor.deleteConfirmVisible}
        onCancel={() => editor.setDeleteConfirmVisible(false)}
        onConfirm={editor.handleDeleteConfirm}
        activePreset={activePreset}
        avatarImageMap={avatarImageMap}
      />
      <SkillConfirmModals
        deletePendingSkillName={editor.deletePendingSkillName}
        setDeletePendingSkillName={editor.setDeletePendingSkillName}
        pendingSkills={editor.pendingSkills}
        setPendingSkills={editor.setPendingSkills}
        deleteCustomSkillName={editor.deleteCustomSkillName}
        setDeleteCustomSkillName={editor.setDeleteCustomSkillName}
        customSkills={editor.customSkills}
        setCustomSkills={editor.setCustomSkills}
        selectedSkills={editor.selectedSkills}
        setSelectedSkills={editor.setSelectedSkills}
        message={agentMessage}
      />
    </>
  );

  return (
    <>
      {fallbackNotice}
      {(descriptionNode || promptsNode) && (
        <div className='mt-16px max-w-700px mx-auto w-full'>
          {descriptionNode}
          {promptsNode}
        </div>
      )}
      {modalTree}
    </>
  );
};

export default GuidPresetEditorHost;
