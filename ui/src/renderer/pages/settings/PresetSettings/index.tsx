/**
 * PresetSettings — Settings page for managing presets.
 *
 * Editing permissions by preset type:
 *
 * | Field          | Builtin | Extension | Custom |
 * |----------------|---------|-----------|--------|
 * | Save button    |  no     |  no       |  yes   |
 * | Name           |  no     |  no       |  yes   |
 * | Description    |  no     |  no       |  yes   |
 * | Avatar         |  no     |  no       |  yes   |
 * | Main Agent     |  no     |  no       |  yes   |
 * | Prompt editing |  no     |  no       |  yes   |
 * | Delete         |  no     |  no       |  yes   |
 *
 * Builtin and extension presets are fully read-only. The drawer
 * still renders their skills panel so users can inspect what's bundled,
 * but every editing control (including Save) is disabled.
 */
import React, { useCallback, useEffect, useMemo, useRef, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { useLocation, useSearchParams } from 'react-router-dom';
import { useArcoMessage } from '@/renderer/utils/ui/useArcoMessage';
import coworkSvg from '@/renderer/assets/icons/cowork.svg';
import NomiScrollArea from '@/renderer/components/base/NomiScrollArea';
import HubPageShell from '@/renderer/components/layout/HubPageShell';
import { useDetectedAgents, usePresetEditor, usePresetList, usePresetTags } from '@/renderer/hooks/preset';
import { resolveAvatarImageSrc } from './presetUtils';
import PresetEditDrawer from './PresetEditDrawer';
import PresetListPanel from './PresetListPanel';
import DeletePresetModal from './DeletePresetModal';
import SkillConfirmModals from './SkillConfirmModals';
import TagManagementModal from './TagManagementModal';

type PresetNavigationState = {
  openPresetId?: string;
  openPresetEditor?: boolean;
};
const OPEN_PRESET_EDITOR_INTENT_KEY = 'guid.openPresetEditorIntent';

const PresetSettings: React.FC = () => {
  const { t } = useTranslation();
  const [message, messageContext] = useArcoMessage({ maxCount: 10 });
  const location = useLocation();
  const [searchParams, setSearchParams] = useSearchParams();
  const navigationState = (location.state as PresetNavigationState | null) ?? null;
  const highlightId = searchParams.get('highlight');

  const handleHighlightConsumed = useCallback(() => {
    const next = new URLSearchParams(searchParams);
    next.delete('highlight');
    setSearchParams(next, { replace: true });
  }, [searchParams, setSearchParams]);
  const avatarImageMap: Record<string, string> = useMemo(
    () => ({
      'cowork.svg': coworkSvg,
      '\u{1F6E0}\u{FE0F}': coworkSvg,
    }),
    []
  );

  // Compose hooks
  const {
    presets,
    activePresetId,
    setActivePresetId,
    activePreset,
    isExtensionPreset,
    loadPresets,
    localeKey,
  } = usePresetList();

  const { availableBackends, refreshAgentDetection } = useDetectedAgents();

  const tags = usePresetTags();
  const [tagModalVisible, setTagModalVisible] = useState(false);

  const editor = usePresetEditor({
    localeKey,
    activePreset,
    isExtensionPreset,
    setActivePresetId,
    loadPresets,
    refreshAgentDetection,
    message,
  });

  const editAvatarImage = resolveAvatarImageSrc(editor.editAvatar, avatarImageMap);
  const hasConsumedNavigationIntentRef = useRef(false);

  useEffect(() => {
    if (hasConsumedNavigationIntentRef.current) return;
    const openPresetFromRoute =
      navigationState?.openPresetEditor && navigationState.openPresetId ? navigationState.openPresetId : null;

    let openPresetFromSession: string | null = null;
    try {
      const rawIntent = sessionStorage.getItem(OPEN_PRESET_EDITOR_INTENT_KEY);
      if (rawIntent) {
        const parsedIntent = JSON.parse(rawIntent) as { presetId?: string; openPresetEditor?: boolean };
        if (parsedIntent.openPresetEditor && parsedIntent.presetId) {
          openPresetFromSession = parsedIntent.presetId;
        }
      }
    } catch (error) {
      console.error('[PresetManagement] Failed to parse preset open intent:', error);
    }

    const targetPresetId = openPresetFromRoute ?? openPresetFromSession;
    if (!targetPresetId) return;
    if (presets.length === 0) return;

    const targetPreset = presets.find((preset) => preset.preset_id === targetPresetId);
    if (!targetPreset) return;

    hasConsumedNavigationIntentRef.current = true;
    try {
      sessionStorage.removeItem(OPEN_PRESET_EDITOR_INTENT_KEY);
    } catch (error) {
      console.error('[PresetManagement] Failed to clear preset open intent:', error);
    }
    void editor.handleEdit(targetPreset);
  }, [presets, editor, navigationState]);

  return (
    <HubPageShell
      title={t('settings.presetsHub.title', { defaultValue: 'Presets' })}
      subtitle={t('settings.presetsHub.subtitle', {
        defaultValue: 'Save reusable agent, model, skill and knowledge configurations for one-click startup.',
      })}
      maxWidthClass='md:max-w-1200px'
    >
      {messageContext}
    <div className='flex flex-col h-full w-full'>
      <NomiScrollArea className='flex-1 min-h-0 pb-16px scrollbar-hide' disableOverflow>
        <PresetListPanel
          presets={presets}
          localeKey={localeKey}
          avatarImageMap={avatarImageMap}
          isExtensionPreset={isExtensionPreset}
          onEdit={(preset) => void editor.handleEdit(preset)}
          onDuplicate={(preset) => void editor.handleDuplicate(preset)}
          onCreate={() => void editor.handleCreate()}
          onToggleEnabled={(preset, checked) => void editor.handleToggleEnabled(preset, checked)}
          setActivePresetId={setActivePresetId}
          highlightId={highlightId}
          onHighlightConsumed={handleHighlightConsumed}
          audienceTags={tags.audienceTags}
          scenarioTags={tags.scenarioTags}
          tagById={tags.tagById}
          onManageTags={() => setTagModalVisible(true)}
        />

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
            editor.isCreating
              ? false
              : activePreset?.source === 'builtin' || isExtensionPreset(activePreset)
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
          message={message}
        />
      </NomiScrollArea>

      <TagManagementModal
        visible={tagModalVisible}
        onClose={() => setTagModalVisible(false)}
        audienceTags={tags.audienceTags}
        scenarioTags={tags.scenarioTags}
        localeKey={localeKey}
        onCreate={tags.createTag}
        onRename={tags.renameTag}
        onDelete={tags.deleteTag}
        message={message}
      />
    </div>
    </HubPageShell>
  );
};

export default PresetSettings;
