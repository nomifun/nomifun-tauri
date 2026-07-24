/**
 * PresetEditDrawer — Drawer for creating/editing an preset.
 * Contains name/avatar fields, agent selector, rules editor, and skills section.
 */
import type { PresetListItem, BuiltinAutoSkill, SkillInfo } from './types';
import type { AvailableBackend } from '@/renderer/hooks/preset';
import type {
  CreatePresetTagRequest,
  ModelPreference,
  PresetKnowledgePolicy,
  PresetReference,
  PresetTag,
  PresetTarget,
} from '@/common/types/agent/presetTypes';
import type { PresetTagId } from '@/common/types/ids';
import type { ImportedAgentSkill } from '@/renderer/pages/settings/skill/AgentSkillImportDrawer';
import type { AgentSkillImportRow } from '@/renderer/pages/settings/skill/agentSkillImportUtils';
import AgentSkillImportDrawer from '@/renderer/pages/settings/skill/AgentSkillImportDrawer';
import {
  resolveSkillDisplay,
  type LocalizableSkill,
} from '@/renderer/pages/settings/skill/skillDisplay';
import PresetTagPicker, { type PresetTagPickerHandle } from './PresetTagPicker';
import { createPresetTagDraftLifecycle } from './presetTagDraftLifecycle';
import EmojiPicker from '@/renderer/components/chat/EmojiPicker';
import MarkdownView from '@/renderer/components/Markdown';
import NomiSelect from '@/renderer/components/base/NomiSelect';
import { useModelProviderList } from '@/renderer/hooks/agent/useModelProviderList';
import { useModelSelectorProviderLabel } from '@/renderer/hooks/agent/useModelSelectorProviderLabel';
import { useKnowledgeBases } from '@/renderer/pages/knowledge/useKnowledge';
import { Avatar, Button, Checkbox, Collapse, Drawer, Input, Select, Tag, Typography } from '@arco-design/web-react';
import { Close, Delete, Info, Plus, Robot } from '@icon-park/react';
import React, { useEffect, useMemo, useRef, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { useNavigate } from 'react-router-dom';
import {
  parseAgentId,
  parseKnowledgeBaseId,
  parseProviderId,
  type AgentId,
  type KnowledgeBaseId,
} from '@/common/types/ids';

const ANY_PROVIDER_TOKEN = '*';

const LocalizedSkillContent: React.FC<{
  skill: LocalizableSkill;
  localeKey: string;
  badge?: React.ReactNode;
}> = ({ skill, localeKey, badge }) => {
  const display = resolveSkillDisplay(skill, localeKey);
  return (
    <div className='flex-1 min-w-0'>
      <div className='flex items-center gap-6px'>
        <div className='text-13px font-medium text-t-primary'>{display.name}</div>
        {badge}
      </div>
      {display.description && (
        <div className='text-12px text-t-secondary mt-2px line-clamp-2'>{display.description}</div>
      )}
    </div>
  );
};

type PresetEditDrawerProps = {
  // Drawer visibility
  editVisible: boolean;
  setEditVisible: (v: boolean) => void;
  isCreating: boolean;

  // Identity fields
  editName: string;
  setEditName: (v: string) => void;
  editDescription: string;
  setEditDescription: (v: string) => void;
  editRoutingDescription: string;
  setEditRoutingDescription: (v: string) => void;
  editAvatar: string;
  setEditAvatar: (v: string) => void;
  editAvatarImage: string | undefined;
  editAgents: AgentId[];
  setEditAgents: (v: AgentId[]) => void;
  editModels: ModelPreference[];
  setEditModels: (v: ModelPreference[]) => void;
  editTargets: PresetTarget[];
  setEditTargets: (v: PresetTarget[]) => void;
  fallbackAllowed: boolean;
  setFallbackAllowed: (v: boolean) => void;
  autoSelectable: boolean;
  setAutoSelectable: (v: boolean) => void;
  knowledgePolicy: PresetKnowledgePolicy;
  setKnowledgePolicy: (v: PresetKnowledgePolicy) => void;
  knowledgeBaseIds: KnowledgeBaseId[];
  setKnowledgeBaseIds: (v: KnowledgeBaseId[]) => void;

  // Rules / prompt
  editContext: string;
  setEditContext: (v: string) => void;
  promptViewMode: 'edit' | 'preview';
  setPromptViewMode: (v: 'edit' | 'preview') => void;

  // Skills state
  availableSkills: SkillInfo[];
  selectedSkills: string[];
  setSelectedSkills: (v: string[]) => void;
  pendingSkills: Array<{ name: string; description: string }>;
  customSkills: string[];
  setDeletePendingSkillName: (v: string | null) => void;
  setDeleteCustomSkillName: (v: string | null) => void;

  // Builtin auto-injected skills
  builtinAutoSkills: BuiltinAutoSkill[];
  disabledBuiltinSkills: string[];
  setDisabledBuiltinSkills: (v: string[]) => void;

  // Tag pickers (audience / scenario)
  editAudienceTags: PresetTagId[];
  setEditAudienceTags: (v: PresetTagId[]) => void;
  editScenarioTags: PresetTagId[];
  setEditScenarioTags: (v: PresetTagId[]) => void;
  audienceTags: PresetTag[];
  scenarioTags: PresetTag[];
  onCreateTag: (req: CreatePresetTagRequest) => Promise<PresetTag>;
  /** When true (built-in / extension), the tag pickers render read-only. */
  readOnly: boolean;
  localeKey: string;

  // Active preset info
  activePreset: PresetListItem | null;
  activePresetId: PresetReference | null;
  isExtensionPreset: (preset: PresetListItem | null | undefined) => boolean;

  // Agent backend options
  availableBackends: AvailableBackend[];

  // Handlers
  handleSave: () => void;
  onImportAgentSkills: (rows: AgentSkillImportRow[]) => Promise<ImportedAgentSkill[]>;
  handleDeleteClick: () => void;
  /** Duplicate the active preset. Used by the builtin readonly banner so
   *  users can create an editable copy from inside the editor. */
  handleDuplicate: (preset: PresetListItem) => void;
};

const PresetEditDrawer: React.FC<PresetEditDrawerProps> = ({
  editVisible,
  setEditVisible,
  isCreating,
  editName,
  setEditName,
  editDescription,
  setEditDescription,
  editRoutingDescription,
  setEditRoutingDescription,
  editAvatar,
  setEditAvatar,
  editAvatarImage,
  editAgents,
  setEditAgents,
  editModels,
  setEditModels,
  editTargets,
  setEditTargets,
  fallbackAllowed,
  setFallbackAllowed,
  autoSelectable,
  setAutoSelectable,
  knowledgePolicy,
  setKnowledgePolicy,
  knowledgeBaseIds,
  setKnowledgeBaseIds,
  editContext,
  setEditContext,
  promptViewMode,
  setPromptViewMode,
  availableSkills,
  selectedSkills,
  setSelectedSkills,
  pendingSkills,
  customSkills: _customSkills,
  setDeletePendingSkillName,
  setDeleteCustomSkillName,
  builtinAutoSkills,
  disabledBuiltinSkills,
  setDisabledBuiltinSkills,
  editAudienceTags,
  setEditAudienceTags,
  editScenarioTags,
  setEditScenarioTags,
  audienceTags,
  scenarioTags,
  onCreateTag,
  readOnly,
  localeKey,
  activePreset,
  activePresetId: _activePresetId,
  isExtensionPreset,
  availableBackends,
  handleSave,
  onImportAgentSkills,
  handleDeleteClick,
  handleDuplicate,
}) => {
  const { t } = useTranslation();
  const navigate = useNavigate();
  const textareaWrapperRef = useRef<HTMLDivElement>(null);
  const audiencePickerRef = useRef<PresetTagPickerHandle>(null);
  const scenarioPickerRef = useRef<PresetTagPickerHandle>(null);
  const [drawerWidth, setDrawerWidth] = useState(500);
  const [rulesExpanded, setRulesExpanded] = useState(false);
  const [agentImportVisible, setAgentImportVisible] = useState(false);

  const { resetPendingTagDrafts, closeDrawer, handleDrawerSave } = useMemo(
    () => createPresetTagDraftLifecycle(audiencePickerRef, scenarioPickerRef, setEditVisible, handleSave),
    [handleSave, setEditVisible]
  );

  useEffect(() => {
    if (!editVisible) {
      resetPendingTagDrafts();
    }
  }, [editVisible, resetPendingTagDrafts]);

  // Auto focus textarea when drawer opens in edit mode
  useEffect(() => {
    if (editVisible && promptViewMode === 'edit') {
      const timer = setTimeout(() => {
        const textarea = textareaWrapperRef.current?.querySelector('textarea');
        textarea?.focus();
      }, 100);
      return () => clearTimeout(timer);
    }
  }, [editVisible, promptViewMode]);

  // Responsive drawer width
  useEffect(() => {
    const updateDrawerWidth = () => {
      if (typeof window === 'undefined') return;
      const nextWidth = Math.min(1024, Math.max(480, Math.floor(window.innerWidth * 0.5)));
      setDrawerWidth(nextWidth);
    };

    updateDrawerWidth();
    window.addEventListener('resize', updateDrawerWidth);
    return () => window.removeEventListener('resize', updateDrawerWidth);
  }, []);

  // Whether skills section should be visible.
  // All non-extension presets expose a skills panel: user/custom can edit,
  // builtins show a read-only toggle list. The backend has already filtered
  // extension presets into their own source class.
  const showSkills = isCreating || (activePreset !== null && activePreset.source !== 'extension');

  const agentOptions = availableBackends;

  const { providers, getAvailableModels } = useModelProviderList();
  const providerLabel = useModelSelectorProviderLabel();
  const modelOptions = useMemo(() => {
    const options = new Map<string, { value: string; label: string }>();
    for (const provider of providers) {
      for (const modelName of getAvailableModels(provider)) {
        const value = `${provider.id}::${modelName}`;
        options.set(value, { value, label: `${providerLabel(provider)} · ${modelName}` });
      }
    }
    for (const item of editModels) {
      const value = `${item.provider_id ?? ANY_PROVIDER_TOKEN}::${item.model}`;
      if (!options.has(value)) options.set(value, { value, label: item.model });
    }
    return Array.from(options.values());
  }, [providers, getAvailableModels, editModels, providerLabel]);
  const selectedModelValues = editModels.map((item) => `${item.provider_id ?? ANY_PROVIDER_TOKEN}::${item.model}`);
  const { bases: knowledgeBases } = useKnowledgeBases();

  const targetOptions: Array<{ value: PresetTarget; label: string }> = [
    { value: 'conversation', label: t('settings.presetTargetConversation', { defaultValue: 'Agent conversation' }) },
    { value: 'execution_step', label: t('settings.presetTargetExecutionStep', { defaultValue: '协作任务' }) },
    { value: 'companion', label: t('settings.presetTargetCompanion', { defaultValue: 'Companion' }) },
    { value: 'public_companion', label: t('settings.presetTargetPublicCompanion', { defaultValue: 'Public companion' }) },
    { value: 'cron', label: t('settings.presetTargetCron', { defaultValue: 'Scheduled task' }) },
  ];

  const customSkillItems = availableSkills.filter((skill) => skill.source === 'custom');
  const builtinSkillItems = availableSkills.filter((skill) => skill.source === 'builtin');
  const extensionSkillItems = availableSkills.filter((skill) => skill.source === 'extension');
  const customActiveCount = selectedSkills.filter(
    (name) =>
      pendingSkills.some((skill) => skill.name === name) || customSkillItems.some((skill) => skill.name === name)
  ).length;
  const builtinActiveCount = selectedSkills.filter((name) =>
    builtinSkillItems.some((skill) => skill.name === name)
  ).length;
  const extensionActiveCount = selectedSkills.filter((name) =>
    extensionSkillItems.some((skill) => skill.name === name)
  ).length;
  const autoInjectedActiveCount = builtinAutoSkills.filter(
    (skill) => !disabledBuiltinSkills.includes(skill.name)
  ).length;
  const customStatusDotColor = customActiveCount > 0 ? 'rgb(var(--success-6))' : 'var(--color-text-4)';
  const builtinStatusDotColor = builtinActiveCount > 0 ? 'rgb(var(--success-6))' : 'var(--color-text-4)';
  const extensionStatusDotColor = extensionActiveCount > 0 ? 'rgb(var(--success-6))' : 'var(--color-text-4)';
  const autoInjectedStatusDotColor = autoInjectedActiveCount > 0 ? 'rgb(var(--success-6))' : 'var(--color-text-4)';
  const totalSkillsCount =
    pendingSkills.length +
    customSkillItems.length +
    builtinSkillItems.length +
    extensionSkillItems.length +
    builtinAutoSkills.length;
  const totalActiveSkillsCount =
    selectedSkills.filter(
      (name) =>
        pendingSkills.some((skill) => skill.name === name) || availableSkills.some((skill) => skill.name === name)
    ).length + autoInjectedActiveCount;
  const isBuiltin = activePreset?.source === 'builtin';
  const isRuleEditable = !readOnly;
  const isSkillsEditable = !readOnly;
  const rulesContainerHeight = rulesExpanded
    ? '420px'
    : isRuleEditable && promptViewMode === 'edit'
      ? '260px'
      : '220px';

  return (
    <Drawer
      title={
        <>
          <span>
            {isCreating
              ? t('settings.createPreset', { defaultValue: 'Create Preset' })
              : t('settings.editPreset', { defaultValue: 'Preset Details' })}
          </span>
          <div
            onClick={(e) => {
              e.stopPropagation();
              closeDrawer();
            }}
            className='absolute right-4 top-2 cursor-pointer text-t-secondary hover:text-t-primary transition-colors p-1'
            style={{ zIndex: 10, WebkitAppRegion: 'no-drag' } as React.CSSProperties}
          >
            <Close size={18} />
          </div>
        </>
      }
      closable={false}
      visible={editVisible}
      placement='right'
      width={drawerWidth}
      zIndex={1200}
      getPopupContainer={() => document.body}
      autoFocus={false}
      onCancel={closeDrawer}
      headerStyle={{ background: 'var(--color-bg-1)' }}
      bodyStyle={{ background: 'var(--color-bg-1)' }}
      footer={
        <div className='flex items-center justify-between w-full'>
          <div className='flex items-center gap-8px'>
            {!readOnly && <Button
              type='primary'
              onClick={handleDrawerSave}
              data-testid='btn-save-preset'
              className='w-[100px] rounded-[100px]'
            >
              {isCreating ? t('common.create', { defaultValue: 'Create' }) : t('common.save', { defaultValue: 'Save' })}
            </Button>}
            <Button
              onClick={closeDrawer}
              className='w-[100px] rounded-[100px] bg-fill-2'
            >
              {t('common.cancel', { defaultValue: 'Cancel' })}
            </Button>
          </div>
          {!isCreating && activePreset?.source !== 'builtin' && !isExtensionPreset(activePreset) && (
            <Button
              status='danger'
              onClick={handleDeleteClick}
              data-testid='btn-delete-preset'
              className='rounded-[100px]'
              style={{ backgroundColor: 'rgb(var(--danger-1))' }}
            >
              {t('common.delete', { defaultValue: 'Delete' })}
            </Button>
          )}
        </div>
      }
    >
      <div className='flex flex-col h-full overflow-hidden' data-testid='preset-edit-drawer'>
        <div className='flex flex-col flex-1 gap-16px bg-fill-2 rounded-16px p-20px overflow-y-auto'>
          {/* Catalog-owned presets stay immutable; duplicate to customize. */}
          {isBuiltin && activePreset && (
            <div
              className='flex items-start gap-8px p-12px rd-8px bg-[rgba(var(--primary-6),0.06)] border border-solid border-[rgba(var(--primary-6),0.18)]'
              data-testid='preset-builtin-readonly-banner'
            >
              <Info theme='outline' size={16} className='mt-2px text-primary-6 flex-shrink-0' />
              <div className='text-13px leading-20px text-t-primary'>
                <span>
                  {t('settings.presetBuiltinReadonlyTip', {
                    defaultValue:
                      'This preset is maintained by NomiFun. To customize it, ',
                  })}
                </span>
                <a
                  className='text-primary-6 hover:text-primary-7 underline-offset-2 hover:underline cursor-pointer'
                  onClick={(e) => {
                    e.preventDefault();
                    handleDuplicate(activePreset);
                  }}
                  data-testid='link-duplicate-from-banner'
                >
                  {t('settings.presetBuiltinReadonlyDuplicateLink', { defaultValue: 'duplicate it' })}
                </a>
                <span>{t('settings.presetBuiltinReadonlyTipSuffix', { defaultValue: '.' })}</span>
              </div>
            </div>
          )}

          {/* Name & Avatar */}
          <div className='flex-shrink-0'>
            <Typography.Text bold>
              <span className='text-red-500'>*</span>{' '}
              {t('settings.presetNameAvatar', { defaultValue: 'Name & Avatar' })}
            </Typography.Text>
            <div className='mt-10px flex items-center gap-12px'>
              {activePreset?.source === 'builtin' ? (
                <Avatar shape='square' size={40} className='bg-bg-1 rounded-4px'>
                  {editAvatarImage ? (
                    <img src={editAvatarImage} alt='' width={24} height={24} style={{ objectFit: 'contain' }} />
                  ) : editAvatar ? (
                    <span className='text-24px'>{editAvatar}</span>
                  ) : (
                    <Robot theme='outline' size={20} />
                  )}
                </Avatar>
              ) : (
                <EmojiPicker value={editAvatar} onChange={(emoji) => setEditAvatar(emoji)} placement='br'>
                  <div className='cursor-pointer'>
                    <Avatar shape='square' size={40} className='bg-bg-1 rounded-4px hover:bg-fill-2 transition-colors'>
                      {editAvatarImage ? (
                        <img src={editAvatarImage} alt='' width={24} height={24} style={{ objectFit: 'contain' }} />
                      ) : editAvatar ? (
                        <span className='text-24px'>{editAvatar}</span>
                      ) : (
                        <Robot theme='outline' size={20} />
                      )}
                    </Avatar>
                  </div>
                </EmojiPicker>
              )}
              <Input
                value={editName}
                onChange={(value) => setEditName(value)}
                disabled={readOnly}
                placeholder={t('settings.presetNamePlaceholder', { defaultValue: 'Enter a name for this preset' })}
                data-testid='input-preset-name'
                className='flex-1 rounded-4px bg-bg-1'
              />
            </div>
          </div>

          {/* Description */}
          <div className='flex-shrink-0'>
            <Typography.Text bold>
              {t('settings.presetDescription', { defaultValue: 'Preset Description' })}
            </Typography.Text>
            <Input
              className='mt-10px rounded-4px bg-bg-1'
              value={editDescription}
              onChange={(value) => setEditDescription(value)}
              disabled={readOnly}
              data-testid='input-preset-desc'
              placeholder={t('settings.presetDescriptionPlaceholder', {
                defaultValue: 'What can this preset help with?',
              })}
            />
          </div>

          <div className='flex-shrink-0'>
            <Typography.Text bold>
              {t('settings.presetRoutingDescription', { defaultValue: 'Agent-facing description' })}
            </Typography.Text>
            <Input.TextArea
              className='mt-10px rounded-4px bg-bg-1'
              value={editRoutingDescription}
              onChange={setEditRoutingDescription}
              disabled={readOnly}
              autoSize={{ minRows: 2, maxRows: 4 }}
              placeholder={t('settings.presetRoutingDescriptionPlaceholder', {
                defaultValue: 'Describe when an Agent or collaboration task should reuse this preset.',
              })}
            />
          </div>

          {/* Ordered Agent preferences */}
          <div className='flex-shrink-0'>
            <Typography.Text bold>{t('settings.presetPreferredAgents', { defaultValue: 'Preferred Agents' })}</Typography.Text>
            <NomiSelect
              mode='multiple'
              className='mt-10px w-full rounded-4px'
              value={editAgents}
              onChange={(value) => setEditAgents((value as string[]).map(parseAgentId))}
              disabled={readOnly}
              allowClear
              showSearch
              data-testid='select-preset-agent'
            >
              {agentOptions.map((opt) => (
                <NomiSelect.Option key={opt.id} value={opt.id}>
                  <span className='flex items-center gap-6px'>
                    {opt.name}
                    {opt.isExtension && (
                      <Tag size='small' bordered={false} className='!bg-primary-1 !text-primary-6'>
                        ext
                      </Tag>
                    )}
                  </span>
                </NomiSelect.Option>
              ))}
            </NomiSelect>
            <div className='mt-6px text-12px text-t-secondary'>
              {t('settings.presetPreferenceOrderHint', {
                defaultValue: 'The first available option is used. Drag-and-drop ordering will follow the selected order.',
              })}
            </div>
          </div>

          {/* Provider-qualified model preferences. */}
          <div className='flex-shrink-0'>
            <Typography.Text bold>
              {t('settings.presetPreferredModels', { defaultValue: '偏好模型（可选）' })}
            </Typography.Text>
            <NomiSelect
              mode='multiple'
              className='mt-10px w-full'
              value={selectedModelValues}
              onChange={(value) =>
                setEditModels(
                  (value as string[]).map((item) => {
                    const [providerId, ...modelParts] = item.split('::');
                    return {
                      provider_id: providerId === ANY_PROVIDER_TOKEN ? undefined : parseProviderId(providerId),
                      model: modelParts.join('::'),
                      required: false,
                    };
                  })
                )
              }
              disabled={readOnly}
              allowClear
              showSearch
              placeholder={t('settings.presetPreferredModelsPlaceholder', {
                defaultValue: '该角色参与协作任务时优先选用的模型',
              })}
              notFoundContent={
                <div className='text-center text-t-secondary text-12px py-8px'>
                  {t('settings.noAvailableModels', { defaultValue: 'No available models' })}
                </div>
              }
              data-testid='select-preset-preferred-models'
            >
              {modelOptions.map((option) => (
                <NomiSelect.Option key={option.value} value={option.value}>
                  {option.label}
                </NomiSelect.Option>
              ))}
            </NomiSelect>
          </div>

          {/* Summary */}
          <div className='flex flex-wrap items-center gap-8px p-10px rd-10px bg-fill-1'>
            <span className='text-12px text-t-secondary'>
              {t('settings.presetPreferredAgents', { defaultValue: 'Preferred Agents' })}:
            </span>
            <Tag size='small' bordered={false} className='!bg-primary-1 !text-primary-6'>
              {editAgents.length || t('common.none', { defaultValue: 'None' })}
            </Tag>
            <span className='text-12px text-t-secondary ml-6px'>
              {t('settings.presetSkills', { defaultValue: 'Skills' })}:
            </span>
            <Tag size='small' color={totalActiveSkillsCount > 0 ? 'green' : 'gray'}>
              {totalActiveSkillsCount > 0 ? `${totalActiveSkillsCount}/${totalSkillsCount}` : totalSkillsCount}
            </Tag>
          </div>

          <div className='flex-shrink-0 p-12px rd-10px border border-solid border-border-2 bg-bg-1'>
            <Typography.Text bold>{t('settings.presetApplication', { defaultValue: 'Application' })}</Typography.Text>
            <Checkbox.Group
              className='preset-scope-selection-checkbox mt-10px flex flex-wrap gap-x-16px gap-y-8px'
              value={editTargets}
              onChange={(value) => setEditTargets(value as PresetTarget[])}
              disabled={readOnly}
              options={targetOptions}
            />
            <div className='mt-12px flex flex-wrap gap-x-20px gap-y-8px'>
              <Checkbox
                checked={fallbackAllowed}
                disabled={readOnly}
                className='preset-scope-selection-checkbox'
                onChange={setFallbackAllowed}
              >
                {t('settings.presetAllowFallback', { defaultValue: 'Allow fallback when a preference is unavailable' })}
              </Checkbox>
              <Checkbox
                checked={autoSelectable}
                disabled={readOnly}
                className='preset-scope-selection-checkbox'
                onChange={setAutoSelectable}
              >
                {t('settings.presetAutoSelectable', { defaultValue: '允许协作任务自动选择' })}
              </Checkbox>
            </div>
          </div>

          <div className='flex-shrink-0 p-12px rd-10px border border-solid border-border-2 bg-bg-1'>
            <div className='flex items-center justify-between gap-12px'>
              <div>
                <Typography.Text bold>{t('settings.presetKnowledge', { defaultValue: 'Knowledge scope' })}</Typography.Text>
                <div className='text-12px text-t-secondary mt-2px'>
                  {t('settings.presetKnowledgeHint', { defaultValue: 'Bind knowledge bases to each launch without changing workspace defaults.' })}
                </div>
              </div>
              <Checkbox
                checked={knowledgePolicy.enabled}
                disabled={readOnly}
                className='preset-scope-selection-checkbox'
                onChange={(enabled) => setKnowledgePolicy({ ...knowledgePolicy, enabled })}
              >
                {t('common.enabled', { defaultValue: 'Enabled' })}
              </Checkbox>
            </div>
            {knowledgePolicy.enabled && (
              <div className='mt-12px flex flex-col gap-10px'>
                <NomiSelect
                  mode='multiple'
                  value={knowledgeBaseIds}
                  onChange={(value) => setKnowledgeBaseIds((value as unknown[]).map(parseKnowledgeBaseId))}
                  disabled={readOnly}
                  allowClear
                  showSearch
                  placeholder={t('settings.presetKnowledgeBasesPlaceholder', { defaultValue: 'Choose knowledge bases' })}
                >
                  {knowledgeBases.map((base) => (
                    <NomiSelect.Option key={base.knowledge_base_id} value={base.knowledge_base_id}>{base.name}</NomiSelect.Option>
                  ))}
                </NomiSelect>
                <div className='grid grid-cols-1 md:grid-cols-3 gap-10px'>
                  <Select
                    value={knowledgePolicy.mode}
                    disabled={readOnly}
                    onChange={(mode) => setKnowledgePolicy({ ...knowledgePolicy, mode: String(mode) })}
                  >
                    <Select.Option value='inherit'>{t('settings.presetKnowledgeModeInherit', { defaultValue: 'Inherit defaults' })}</Select.Option>
                    <Select.Option value='staged'>{t('settings.presetKnowledgeModeStaged', { defaultValue: 'Selected bases (staged)' })}</Select.Option>
                    <Select.Option value='direct'>{t('settings.presetKnowledgeModeDirect', { defaultValue: 'Selected bases (direct)' })}</Select.Option>
                  </Select>
                  <Checkbox checked={knowledgePolicy.grounded} disabled={readOnly} onChange={(grounded) => setKnowledgePolicy({ ...knowledgePolicy, grounded })}>
                    {t('settings.presetKnowledgeGrounded', { defaultValue: 'Require grounding' })}
                  </Checkbox>
                  <Checkbox checked={knowledgePolicy.writeback} disabled={readOnly} onChange={(writeback) => setKnowledgePolicy({ ...knowledgePolicy, writeback })}>
                    {t('settings.presetKnowledgeWriteback', { defaultValue: 'Allow write-back' })}
                  </Checkbox>
                </div>
                {knowledgePolicy.writeback && (
                  <div className='grid grid-cols-1 md:grid-cols-[minmax(0,1fr)_minmax(180px,0.45fr)] gap-10px items-center'>
                    <div className='text-12px text-t-secondary'>
                      {t('knowledge.mount.eagernessLabel', { defaultValue: 'Write-back eagerness' })}
                    </div>
                    <Select
                      value={knowledgePolicy.eagerness ?? 'conservative'}
                      disabled={readOnly}
                      onChange={(eagerness) =>
                        setKnowledgePolicy({
                          ...knowledgePolicy,
                          eagerness: eagerness as 'conservative' | 'aggressive',
                        })
                      }
                    >
                      <Select.Option value='conservative'>
                        {t('knowledge.control.eagernessConservative', { defaultValue: 'Conservative' })}
                      </Select.Option>
                      <Select.Option value='aggressive'>
                        {t('knowledge.control.eagernessAggressive', { defaultValue: 'Aggressive' })}
                      </Select.Option>
                    </Select>
                  </div>
                )}
              </div>
            )}
          </div>

          {/* Tags — audience / scenario chip pickers. Read-only for builtin /
              extension presets (their tags are immutable at the source). */}
          <div className='flex-shrink-0 flex flex-col gap-14px'>
            <Typography.Text bold>{t('settings.presetTags', { defaultValue: 'Tags' })}</Typography.Text>
            <PresetTagPicker
              ref={audiencePickerRef}
              dimension='audience'
              label={t('settings.presetTagPickAudience', { defaultValue: 'Audience tags' })}
              tags={audienceTags}
              value={editAudienceTags}
              onChange={(values) => setEditAudienceTags(values as PresetTagId[])}
              onCreateTag={onCreateTag}
              localeKey={localeKey}
              readOnly={readOnly}
              reference='id'
              showAddHint
            />
            <PresetTagPicker
              ref={scenarioPickerRef}
              dimension='scenario'
              label={t('settings.presetTagPickScenario', { defaultValue: 'Scenario tags' })}
              tags={scenarioTags}
              value={editScenarioTags}
              onChange={(values) => setEditScenarioTags(values as PresetTagId[])}
              onCreateTag={onCreateTag}
              localeKey={localeKey}
              readOnly={readOnly}
              reference='id'
              showAddHint
            />
          </div>

          {/* Agent instructions */}
          <div className='flex-shrink-0'>
            <div className='flex items-center justify-between'>
              <Typography.Text bold className='flex-shrink-0'>
                {t('settings.presetInstructions', { defaultValue: 'Agent instructions' })}
              </Typography.Text>
              <Button
                type='text'
                size='mini'
                data-testid='btn-expand-rules'
                onClick={() => setRulesExpanded((prev) => !prev)}
              >
                {rulesExpanded
                  ? t('common.collapse', { defaultValue: 'Collapse' })
                  : t('common.expand', { defaultValue: 'Expand' })}
              </Button>
            </div>
            <div
              className='mt-10px border border-border-2 overflow-hidden rounded-4px'
              style={{ height: rulesContainerHeight }}
            >
              {isRuleEditable && (
                <div className='flex items-center h-36px bg-fill-2 border-b border-border-2 flex-shrink-0'>
                  <div
                    className={`flex items-center h-full px-16px cursor-pointer transition-all text-13px font-medium ${promptViewMode === 'edit' ? 'text-primary border-b-2 border-primary bg-bg-1' : 'text-t-secondary hover:text-t-primary'}`}
                    onClick={() => setPromptViewMode('edit')}
                  >
                    {t('settings.promptEdit', { defaultValue: 'Edit' })}
                  </div>
                  <div
                    className={`flex items-center h-full px-16px cursor-pointer transition-all text-13px font-medium ${promptViewMode === 'preview' ? 'text-primary border-b-2 border-primary bg-bg-1' : 'text-t-secondary hover:text-t-primary'}`}
                    onClick={() => setPromptViewMode('preview')}
                  >
                    {t('settings.promptPreview', { defaultValue: 'Preview' })}
                  </div>
                </div>
              )}
              <div
                className='bg-fill-2'
                style={{
                  height: isRuleEditable ? 'calc(100% - 36px)' : '100%',
                  overflow: 'auto',
                }}
              >
                {promptViewMode === 'edit' && isRuleEditable ? (
                  <div ref={textareaWrapperRef} className='h-full'>
                    <Input.TextArea
                      value={editContext}
                      onChange={(value) => setEditContext(value)}
                      placeholder={t('settings.presetInstructionsPlaceholder', {
                        defaultValue: 'Describe the role, behavior, process and output expectations in Markdown...',
                      })}
                      autoSize={false}
                      className='border-none rounded-none bg-transparent h-full resize-none'
                    />
                  </div>
                ) : (
                  <div className='p-16px text-14px leading-7'>
                    {editContext ? (
                      <MarkdownView hiddenCodeCopyButton>{editContext}</MarkdownView>
                    ) : (
                      <div className='text-t-secondary text-center py-32px'>
                        {t('settings.promptPreviewEmpty', { defaultValue: 'No content to preview' })}
                      </div>
                    )}
                  </div>
                )}
              </div>
            </div>
          </div>

          {/* Skills section */}
          {showSkills && (
            <div className='flex-shrink-0 mt-16px' data-testid='skills-section'>
              <div className='flex items-center justify-between mb-12px'>
                <Typography.Text bold>{t('settings.presetSkills', { defaultValue: 'Skills' })}</Typography.Text>
                {/* Builtin readonly presets don't expose an Add Skills entry
                    point — users must duplicate first. The skill checkbox list
                    below renders disabled for the same reason. */}
                {isSkillsEditable && (
                  <div className='flex items-center gap-8px'>
                    <Button
                      size='small'
                      type='outline'
                      icon={<Plus size={14} />}
                      onClick={() => navigate('/skills')}
                      className='rounded-[100px]'
                      data-testid='btn-add-skills'
                    >
                      {t('settings.addSkills', { defaultValue: 'Add Skills' })}
                    </Button>
                    <Button
                      size='small'
                      type='outline'
                      icon={<Plus size={14} />}
                      onClick={() => setAgentImportVisible(true)}
                      className='rounded-[100px]'
                      data-testid='btn-import-agent-skills-to-preset'
                    >
                      {t('settings.agentSkillImport.shortAction', { defaultValue: 'Import from Agent' })}
                    </Button>
                  </div>
                )}
              </div>

              <Collapse defaultActiveKey={['custom-skills']} data-testid='skills-collapse'>
                {/* Custom Skills (Pending + Imported) */}
                <Collapse.Item
                  header={
                    <span className='text-13px font-medium'>
                      {t('settings.customSkills', { defaultValue: 'Imported Skills (Library)' })}
                    </span>
                  }
                  name='custom-skills'
                  className='mb-8px'
                  extra={
                    <div className='flex items-center gap-8px'>
                      <span
                        className='inline-block w-8px h-8px rd-50%'
                        style={{ background: customStatusDotColor }}
                        aria-hidden='true'
                      />
                      <span className='text-12px text-t-secondary'>
                        {customActiveCount > 0
                          ? `${customActiveCount}/${pendingSkills.length + customSkillItems.length}`
                          : pendingSkills.length + customSkillItems.length}
                      </span>
                    </div>
                  }
                >
                  <div className='space-y-4px'>
                    {/* Pending skills (not yet imported) */}
                    {pendingSkills.map((skill) => (
                      <div
                        key={`pending-${skill.name}`}
                        className='flex items-start gap-8px p-8px hover:bg-fill-1 rounded-4px group'
                      >
                        <Checkbox
                          checked={selectedSkills.includes(skill.name)}
                          disabled={!isSkillsEditable}
                          className='preset-skill-selection-checkbox mt-2px cursor-pointer'
                          onChange={() => {
                            if (selectedSkills.includes(skill.name)) {
                              setSelectedSkills(selectedSkills.filter((s) => s !== skill.name));
                            } else {
                              setSelectedSkills([...selectedSkills, skill.name]);
                            }
                          }}
                        />
                        <div className='flex-1 min-w-0'>
                          <div className='flex items-center gap-6px'>
                            <div className='text-13px font-medium text-t-primary'>{skill.name}</div>
                            <span className='bg-[rgba(var(--primary-6),0.08)] text-primary-6 border border-[rgba(var(--primary-6),0.2)] text-10px px-4px py-1px rd-4px font-medium uppercase'>
                              {t('settings.pending', { defaultValue: 'Pending' })}
                            </span>
                          </div>
                          {skill.description && (
                            <div className='text-12px text-t-secondary mt-2px line-clamp-2'>{skill.description}</div>
                          )}
                        </div>
                        <button
                          className='opacity-0 group-hover:opacity-100 transition-opacity p-4px hover:bg-fill-2 rounded-4px'
                          onClick={(e) => {
                            e.stopPropagation();
                            setDeletePendingSkillName(skill.name);
                          }}
                          title={t('settings.removeFromPreset', { defaultValue: 'Remove from preset' })}
                        >
                          <Delete size={16} fill='var(--color-text-3)' />
                        </button>
                      </div>
                    ))}
                    {/* All imported custom skills */}
                    {customSkillItems.map((skill) => (
                      <div
                        key={`custom-${skill.name}`}
                        className='flex items-start gap-8px p-8px hover:bg-fill-1 rounded-4px group'
                      >
                        <Checkbox
                          checked={selectedSkills.includes(skill.name)}
                          disabled={!isSkillsEditable}
                          className='preset-skill-selection-checkbox mt-2px cursor-pointer'
                          onChange={() => {
                            if (selectedSkills.includes(skill.name)) {
                              setSelectedSkills(selectedSkills.filter((s) => s !== skill.name));
                            } else {
                              setSelectedSkills([...selectedSkills, skill.name]);
                            }
                          }}
                        />
                        <LocalizedSkillContent
                          skill={skill}
                          localeKey={localeKey}
                          badge={
                            <span className='bg-[rgba(242,156,27,0.08)] text-[rgb(242,156,27)] border border-[rgba(242,156,27,0.2)] text-10px px-4px py-1px rd-4px font-medium uppercase'>
                              {t('settings.skillsHub.custom', { defaultValue: 'Custom' })}
                            </span>
                          }
                        />
                        <button
                          className='opacity-0 group-hover:opacity-100 transition-opacity p-4px hover:bg-fill-2 rounded-4px'
                          onClick={(e) => {
                            e.stopPropagation();
                            setDeleteCustomSkillName(skill.name);
                          }}
                          title={t('settings.removeFromPreset', { defaultValue: 'Remove from preset' })}
                        >
                          <Delete size={16} fill='var(--color-text-3)' />
                        </button>
                      </div>
                    ))}
                    {pendingSkills.length === 0 && customSkillItems.length === 0 && (
                      <div className='text-center text-t-secondary text-12px py-16px'>
                        {t('settings.noCustomSkills', { defaultValue: 'No custom skills added' })}
                      </div>
                    )}
                  </div>
                </Collapse.Item>

                {/* Builtin Skills */}
                <Collapse.Item
                  header={
                    <span className='text-13px font-medium'>
                      {t('settings.builtinSkills', { defaultValue: 'Builtin Skills' })}
                    </span>
                  }
                  name='builtin-skills'
                  extra={
                    <div className='flex items-center gap-8px'>
                      <span
                        className='inline-block w-8px h-8px rd-50%'
                        style={{ background: builtinStatusDotColor }}
                        aria-hidden='true'
                      />
                      <span className='text-12px text-t-secondary'>
                        {builtinActiveCount > 0
                          ? `${builtinActiveCount}/${builtinSkillItems.length}`
                          : builtinSkillItems.length}
                      </span>
                    </div>
                  }
                >
                  {builtinSkillItems.length > 0 ? (
                    <div className='space-y-4px'>
                      {builtinSkillItems.map((skill) => (
                        <div key={skill.name} className='flex items-start gap-8px p-8px hover:bg-fill-1 rounded-4px'>
                          <Checkbox
                            checked={selectedSkills.includes(skill.name)}
                            className='preset-skill-selection-checkbox mt-2px cursor-pointer'
                            onChange={() => {
                              if (selectedSkills.includes(skill.name)) {
                                setSelectedSkills(selectedSkills.filter((s) => s !== skill.name));
                              } else {
                                setSelectedSkills([...selectedSkills, skill.name]);
                              }
                            }}
                          />
                          <LocalizedSkillContent skill={skill} localeKey={localeKey} />
                        </div>
                      ))}
                    </div>
                  ) : (
                    <div className='text-center text-t-secondary text-12px py-16px'>
                      {t('settings.noBuiltinSkills', { defaultValue: 'No builtin skills available' })}
                    </div>
                  )}
                </Collapse.Item>

                {/* Extension Skills */}
                {extensionSkillItems.length > 0 && (
                  <Collapse.Item
                    header={
                      <span className='text-13px font-medium'>
                        {t('settings.extensionSkills', { defaultValue: 'Extension Skills' })}
                      </span>
                    }
                    name='extension-skills'
                    extra={
                      <div className='flex items-center gap-8px'>
                        <span
                          className='inline-block w-8px h-8px rd-50%'
                          style={{ background: extensionStatusDotColor }}
                          aria-hidden='true'
                        />
                        <span className='text-12px text-t-secondary'>
                          {extensionActiveCount > 0
                            ? `${extensionActiveCount}/${extensionSkillItems.length}`
                            : extensionSkillItems.length}
                        </span>
                      </div>
                    }
                  >
                    <div className='space-y-4px'>
                      {extensionSkillItems.map((skill) => (
                        <div key={skill.name} className='flex items-start gap-8px p-8px hover:bg-fill-1 rounded-4px'>
                          <Checkbox
                            checked={selectedSkills.includes(skill.name)}
                            className='preset-skill-selection-checkbox mt-2px cursor-pointer'
                            onChange={() => {
                              if (selectedSkills.includes(skill.name)) {
                                setSelectedSkills(selectedSkills.filter((s) => s !== skill.name));
                              } else {
                                setSelectedSkills([...selectedSkills, skill.name]);
                              }
                            }}
                          />
                          <LocalizedSkillContent
                            skill={skill}
                            localeKey={localeKey}
                            badge={
                              <span className='bg-[rgba(var(--primary-6),0.08)] text-primary-6 border border-[rgba(var(--primary-6),0.2)] text-10px px-4px py-1px rd-4px font-medium uppercase'>
                                {t('settings.extensionSkillsBadge', { defaultValue: 'Extension' })}
                              </span>
                            }
                          />
                        </div>
                      ))}
                    </div>
                  </Collapse.Item>
                )}

                {/* Auto-injected Builtin Skills */}
                {builtinAutoSkills.length > 0 && (
                  <Collapse.Item
                    header={
                      <span className='text-13px font-medium'>
                        {t('settings.autoInjectedSkills', { defaultValue: 'Auto-injected Skills' })}
                      </span>
                    }
                    name='auto-injected-skills'
                    extra={
                      <div className='flex items-center gap-8px'>
                        <span
                          className='inline-block w-8px h-8px rd-50%'
                          style={{ background: autoInjectedStatusDotColor }}
                          aria-hidden='true'
                        />
                        <span className='text-12px text-t-secondary'>
                          {`${autoInjectedActiveCount}/${builtinAutoSkills.length}`}
                        </span>
                      </div>
                    }
                  >
                    <div className='space-y-4px'>
                      {builtinAutoSkills.map((skill) => (
                        <div key={skill.name} className='flex items-start gap-8px p-8px hover:bg-fill-1 rounded-4px'>
                          <Checkbox
                            checked={!disabledBuiltinSkills.includes(skill.name)}
                            disabled={!isSkillsEditable}
                            className='preset-skill-selection-checkbox mt-2px cursor-pointer'
                            onChange={() => {
                              if (disabledBuiltinSkills.includes(skill.name)) {
                                setDisabledBuiltinSkills(disabledBuiltinSkills.filter((s) => s !== skill.name));
                              } else {
                                setDisabledBuiltinSkills([...disabledBuiltinSkills, skill.name]);
                              }
                            }}
                          />
                          <LocalizedSkillContent
                            skill={skill}
                            localeKey={localeKey}
                            badge={
                              <span className='bg-[rgba(var(--success-6),0.08)] text-[rgb(var(--success-6))] border border-[rgba(var(--success-6),0.2)] text-10px px-4px py-1px rd-4px font-medium uppercase'>
                                {t('settings.autoInjectedSkillsBadge', { defaultValue: 'Auto' })}
                              </span>
                            }
                          />
                        </div>
                      ))}
                    </div>
                  </Collapse.Item>
                )}
              </Collapse>
            </div>
          )}
        </div>
        <AgentSkillImportDrawer
          visible={agentImportVisible}
          onClose={() => setAgentImportVisible(false)}
          existingSkillNames={availableSkills.map((skill) => skill.name)}
          importSkills={onImportAgentSkills}
          mode='preset'
        />
      </div>
    </Drawer>
  );
};

export default PresetEditDrawer;
