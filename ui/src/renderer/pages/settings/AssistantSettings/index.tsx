/**
 * AssistantSettings — Settings page for managing assistants.
 *
 * Editing permissions by assistant type:
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
 * Builtin and extension assistants are fully read-only. The drawer
 * still renders their skills panel so users can inspect what's bundled,
 * but every editing control (including Save) is disabled.
 */
import { Tabs } from '@arco-design/web-react';
import React, { useCallback, useEffect, useMemo, useRef, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { useLocation, useSearchParams } from 'react-router-dom';
import { useArcoMessage } from '@/renderer/utils/ui/useArcoMessage';
import coworkSvg from '@/renderer/assets/icons/cowork.svg';
import NomiScrollArea from '@/renderer/components/base/NomiScrollArea';
import HubPageShell from '@/renderer/components/layout/HubPageShell';
import { useDetectedAgents, useAssistantEditor, useAssistantList, useAssistantTags } from '@/renderer/hooks/assistant';
import SkillsHubSettings from '../SkillsHubSettings';
import { resolveAvatarImageSrc } from './assistantUtils';
import AssistantEditDrawer from './AssistantEditDrawer';
import AssistantListPanel from './AssistantListPanel';
import DeleteAssistantModal from './DeleteAssistantModal';
import SkillConfirmModals from './SkillConfirmModals';
import TagManagementModal from './TagManagementModal';

type AssistantNavigationState = {
  openAssistantId?: string;
  openAssistantEditor?: boolean;
};
const OPEN_ASSISTANT_EDITOR_INTENT_KEY = 'guid.openAssistantEditorIntent';
type AssistantSkillsTab = 'assistants' | 'skills';

const isAssistantSkillsTab = (value: string | null): value is AssistantSkillsTab =>
  value === 'assistants' || value === 'skills';

const AssistantSettings: React.FC = () => {
  const { t } = useTranslation();
  const [message, messageContext] = useArcoMessage({ maxCount: 10 });
  const location = useLocation();
  const [searchParams, setSearchParams] = useSearchParams();
  const [activeTab, setActiveTab] = useState<AssistantSkillsTab>(() => {
    const param = searchParams.get('tab');
    return isAssistantSkillsTab(param) ? param : 'assistants';
  });
  const navigationState = (location.state as AssistantNavigationState | null) ?? null;
  const highlightId = searchParams.get('highlight');

  useEffect(() => {
    const param = searchParams.get('tab');
    const nextTab = isAssistantSkillsTab(param) ? param : 'assistants';
    if (nextTab !== activeTab) {
      setActiveTab(nextTab);
    }
  }, [activeTab, searchParams]);

  const handleTabChange = useCallback(
    (key: string) => {
      if (!isAssistantSkillsTab(key)) return;
      setActiveTab(key);
      const next = new URLSearchParams(searchParams);
      next.set('tab', key);
      setSearchParams(next, { replace: true });
    },
    [searchParams, setSearchParams]
  );

  const handleHighlightConsumed = useCallback(() => {
    const next = new URLSearchParams(searchParams);
    next.delete('highlight');
    next.set('tab', activeTab);
    setSearchParams(next, { replace: true });
  }, [activeTab, searchParams, setSearchParams]);
  const avatarImageMap: Record<string, string> = useMemo(
    () => ({
      'cowork.svg': coworkSvg,
      '\u{1F6E0}\u{FE0F}': coworkSvg,
    }),
    []
  );

  // Compose hooks
  const {
    assistants,
    activeAssistantId,
    setActiveAssistantId,
    activeAssistant,
    isExtensionAssistant,
    loadAssistants,
    localeKey,
  } = useAssistantList();

  const { availableBackends, refreshAgentDetection } = useDetectedAgents();

  const tags = useAssistantTags();
  const [tagModalVisible, setTagModalVisible] = useState(false);

  const editor = useAssistantEditor({
    localeKey,
    activeAssistant,
    isExtensionAssistant,
    setActiveAssistantId,
    loadAssistants,
    refreshAgentDetection,
    message,
  });

  const editAvatarImage = resolveAvatarImageSrc(editor.editAvatar, avatarImageMap);
  const hasConsumedNavigationIntentRef = useRef(false);

  useEffect(() => {
    if (hasConsumedNavigationIntentRef.current) return;
    const openAssistantFromRoute =
      navigationState?.openAssistantEditor && navigationState.openAssistantId ? navigationState.openAssistantId : null;

    let openAssistantFromSession: string | null = null;
    try {
      const rawIntent = sessionStorage.getItem(OPEN_ASSISTANT_EDITOR_INTENT_KEY);
      if (rawIntent) {
        const parsedIntent = JSON.parse(rawIntent) as { assistantId?: string; openAssistantEditor?: boolean };
        if (parsedIntent.openAssistantEditor && parsedIntent.assistantId) {
          openAssistantFromSession = parsedIntent.assistantId;
        }
      }
    } catch (error) {
      console.error('[AssistantManagement] Failed to parse assistant open intent:', error);
    }

    const targetAssistantId = openAssistantFromRoute ?? openAssistantFromSession;
    if (!targetAssistantId) return;
    if (assistants.length === 0) return;

    const targetAssistant = assistants.find((assistant) => assistant.id === targetAssistantId);
    if (!targetAssistant) return;

    hasConsumedNavigationIntentRef.current = true;
    try {
      sessionStorage.removeItem(OPEN_ASSISTANT_EDITOR_INTENT_KEY);
    } catch (error) {
      console.error('[AssistantManagement] Failed to clear assistant open intent:', error);
    }
    void editor.handleEdit(targetAssistant);
  }, [assistants, editor, navigationState]);

  const assistantManagementContent = (
    <div className='flex flex-col h-full w-full'>
      <NomiScrollArea className='flex-1 min-h-0 pb-16px scrollbar-hide' disableOverflow>
        <AssistantListPanel
          assistants={assistants}
          localeKey={localeKey}
          avatarImageMap={avatarImageMap}
          isExtensionAssistant={isExtensionAssistant}
          onEdit={(assistant) => void editor.handleEdit(assistant)}
          onDuplicate={(assistant) => void editor.handleDuplicate(assistant)}
          onCreate={() => void editor.handleCreate()}
          onToggleEnabled={(assistant, checked) => void editor.handleToggleEnabled(assistant, checked)}
          setActiveAssistantId={setActiveAssistantId}
          highlightId={highlightId}
          onHighlightConsumed={handleHighlightConsumed}
          audienceTags={tags.audienceTags}
          scenarioTags={tags.scenarioTags}
          tagByKey={tags.tagByKey}
          onManageTags={() => setTagModalVisible(true)}
        />

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
            editor.isCreating
              ? false
              : activeAssistant?.source === 'builtin' || isExtensionAssistant(activeAssistant)
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
  );

  return (
    <HubPageShell
      title={t('settings.assistantSkills.title', { defaultValue: 'Assistant & Skill' })}
      subtitle={t('settings.assistantSkills.subtitle', {
        defaultValue: 'Manage assistants and reusable skill packages in one place.',
      })}
      maxWidthClass='md:max-w-1200px'
    >
      {messageContext}
      <Tabs
        activeTab={activeTab}
        onChange={handleTabChange}
        type='line'
        className='flex flex-col flex-1 min-h-0 [&>.arco-tabs-content]:pt-0'
      >
        <Tabs.TabPane
          key='assistants'
          title={t('settings.assistantSkills.assistantsTab', { defaultValue: 'Assistants' })}
        >
          {assistantManagementContent}
        </Tabs.TabPane>
        <Tabs.TabPane key='skills' title={t('settings.assistantSkills.skillsTab', { defaultValue: 'Skills' })}>
          <SkillsHubSettings withWrapper={false} />
        </Tabs.TabPane>
      </Tabs>
    </HubPageShell>
  );
};

export default AssistantSettings;
