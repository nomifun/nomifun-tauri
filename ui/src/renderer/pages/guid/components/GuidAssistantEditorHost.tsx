/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 *
 * GuidAssistantEditorHost — hosts the editor modal tree (AssistantEditDrawer +
 * DeleteAssistantModal + SkillConfirmModals), the openAssistantDetails
 * registration, and the "selected assistant example prompts" rendering.
 *
 * Extracted from AssistantSelectionArea so the entry page can render these
 * independently of the retired assistant card grid.
 */

import coworkSvg from '@/renderer/assets/icons/cowork.svg';
import { useDetectedAgents, useAssistantEditor, useAssistantList, useAssistantTags } from '@/renderer/hooks/assistant';
import AssistantEditDrawer from '@/renderer/pages/settings/AssistantSettings/AssistantEditDrawer';
import DeleteAssistantModal from '@/renderer/pages/settings/AssistantSettings/DeleteAssistantModal';
import SkillConfirmModals from '@/renderer/pages/settings/AssistantSettings/SkillConfirmModals';
import { resolveAvatarImageSrc } from '@/renderer/pages/settings/AssistantSettings/assistantUtils';
import { useArcoMessage } from '@/renderer/utils/ui/useArcoMessage';
import styles from '../index.module.css';
import type { AvailableAgent, EffectiveAgentInfo } from '../types';
import type { Assistant } from '@/common/types/agent/assistantTypes';
import React, { useCallback, useLayoutEffect, useMemo } from 'react';
import { useTranslation } from 'react-i18next';

export interface GuidAssistantEditorHostProps {
  assistants: Assistant[];
  localeKey: string;
  selectedAgentKey?: string;
  selectedAgentInfo: AvailableAgent | undefined;
  currentEffectiveAgentInfo: EffectiveAgentInfo;
  onSetInput: (text: string) => void;
  onFocusInput: () => void;
  onRegisterOpenDetails: (openDetails: (() => void) | null) => void;
}

const resolveAssistantCandidateIds = (assistantId: string): string[] => {
  const stripped = assistantId.replace(/^builtin-/, '');
  return Array.from(new Set([assistantId, `builtin-${stripped}`, stripped]));
};

const avatarImageMap: Record<string, string> = {
  'cowork.svg': coworkSvg,
  '\u{1F6E0}\u{FE0F}': coworkSvg,
};

const GuidAssistantEditorHost: React.FC<GuidAssistantEditorHostProps> = ({
  assistants,
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

  // Internal useAssistantList owns the drawer editor's working state.
  const { activeAssistantId, setActiveAssistantId, activeAssistant, isExtensionAssistant, loadAssistants } =
    useAssistantList();
  const { availableBackends, refreshAgentDetection } = useDetectedAgents();
  const tags = useAssistantTags();

  const editor = useAssistantEditor({
    localeKey,
    activeAssistant,
    isExtensionAssistant,
    setActiveAssistantId,
    loadAssistants,
    refreshAgentDetection,
    message: agentMessage,
  });

  const editAvatarImage = resolveAvatarImageSrc(editor.editAvatar, avatarImageMap);

  // ── openAssistantDetails registration ──
  const openAssistantDetails = useCallback(() => {
    const assistantId = selectedAgentInfo?.custom_agent_id
      ?? (selectedAgentKey?.startsWith('custom:') ? selectedAgentKey.slice(7) : null);
    if (!assistantId) {
      agentMessage.warning(
        t('common.failed', { defaultValue: 'Failed' }) +
          `: ${t('settings.editAssistant', { defaultValue: 'Assistant Details' })}`
      );
      return;
    }

    const candidates = resolveAssistantCandidateIds(assistantId);
    const targetAssistant = assistants.find((assistant) => candidates.includes(assistant.id));
    if (!targetAssistant) {
      agentMessage.warning(
        t('common.failed', { defaultValue: 'Failed' }) +
          `: ${t('settings.editAssistant', { defaultValue: 'Assistant Details' })}`
      );
      return;
    }

    void editor.handleEdit(targetAssistant);
  }, [agentMessage, assistants, editor, selectedAgentInfo?.custom_agent_id, selectedAgentKey, t]);

  useLayoutEffect(() => {
    onRegisterOpenDetails(openAssistantDetails);
  }, [onRegisterOpenDetails, openAssistantDetails]);

  // ── Resolved agent (shared between description block and promptsNode) ──
  const resolvedAgent = useMemo(() => {
    if (!selectedAgentInfo?.custom_agent_id) return null;
    return assistants.find((a) => a.id === selectedAgentInfo.custom_agent_id) ?? null;
  }, [assistants, selectedAgentInfo?.custom_agent_id]);

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
          onClick={openAssistantDetails}
        >
          {t('settings.editAssistant', { defaultValue: '助手详情' })}
        </span>
      </div>
    );
  }, [resolvedAgent, localeKey, openAssistantDetails, t]);

  // ── Example prompts rendering ──
  const promptsNode = useMemo(() => {
    if (!resolvedAgent) return null;
    const prompts = resolvedAgent.prompts_i18n?.[localeKey] || resolvedAgent.prompts_i18n?.['en-US'] || resolvedAgent.prompts;
    if (!prompts || prompts.length === 0) return null;
    return (
      <div className='mt-16px'>
        <div className={styles.assistantPromptHint}>
          {t('guid.promptExamplesHint', { defaultValue: 'Try these example prompts:' })}
        </div>
        <div className='flex flex-wrap gap-8px mt-12px'>
          {prompts.map((prompt: string, index: number) => (
            <div
              key={index}
              className={`${styles.assistantPromptChip} px-12px py-6px text-2 text-13px rd-16px cursor-pointer transition-colors shadow-sm`}
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
      <AssistantEditDrawer
        editVisible={editor.editVisible}
        setEditVisible={editor.setEditVisible}
        isCreating={editor.isCreating}
        editName={editor.editName}
        setEditName={editor.setEditName}
        editDescription={editor.editDescription}
        setEditDescription={editor.setEditDescription}
        editAvatar={editor.editAvatar}
        setEditAvatar={editor.setEditAvatar}
        editAvatarImage={editAvatarImage}
        editAgent={editor.editAgent}
        setEditAgent={editor.setEditAgent}
        editModels={editor.editModels}
        setEditModels={editor.setEditModels}
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
          editor.isCreating ? false : activeAssistant?.source === 'builtin' || isExtensionAssistant(activeAssistant)
        }
        localeKey={localeKey}
        activeAssistant={activeAssistant}
        activeAssistantId={activeAssistantId}
        isExtensionAssistant={isExtensionAssistant}
        availableBackends={availableBackends}
        handleSave={editor.handleSave}
        onImportAgentSkills={editor.handleImportAgentSkills}
        handleDeleteClick={editor.handleDeleteClick}
        handleDuplicate={(assistant) => void editor.handleDuplicate(assistant)}
      />
      <DeleteAssistantModal
        visible={editor.deleteConfirmVisible}
        onCancel={() => editor.setDeleteConfirmVisible(false)}
        onConfirm={editor.handleDeleteConfirm}
        activeAssistant={activeAssistant}
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

export default GuidAssistantEditorHost;
