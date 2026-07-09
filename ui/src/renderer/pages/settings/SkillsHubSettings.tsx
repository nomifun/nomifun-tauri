/**
 * SkillsHubSettings — The Skills Hub page. Every skill (built-in, custom,
 * extension, auto-injected) lives in ONE responsive card grid, filtered by a
 * shared two-row tag bar (Audience / Skill Scenario) over the assistant tag
 * vocabulary. Cards open a SkillTagModal to assign tags; the "Manage Tags" chip
 * opens the shared TagManagementModal for vocabulary CRUD.
 *
 * Visual language mirrors the assistant page: a soft fill-2 panel, an
 * AssistantTagFilterBar, and AssistantCard-style grid items (see SkillCard).
 * Theme variables only; `<div onClick>`/Arco controls (no <button>).
 *
 * `withWrapper` is preserved for the legacy CapabilitiesSettings embed.
 */
import { ipcBridge } from '@/common';
import { isBackendHttpError } from '@/common/adapter/httpBridge';
import { resolveLocaleKey } from '@/common/utils';
import { useLayoutContext } from '@/renderer/hooks/context/LayoutContext';
import { useArcoMessage } from '@/renderer/utils/ui/useArcoMessage';
// Shared tag UI + vocabulary — reused verbatim from the assistant page so the
// skill and assistant surfaces share one chip language and one tag vocabulary.
import { useAssistantTags } from '@/renderer/hooks/assistant';
import AssistantTagFilterBar from './AssistantSettings/AssistantTagFilterBar';
import TagManagementModal from './AssistantSettings/TagManagementModal';
import type { SkillInfo } from './AssistantSettings/types';
import AgentSkillImportDrawer from './skill/AgentSkillImportDrawer';
import type { ExternalAgentSkillSource } from './skill/agentSkillImportUtils';
import SkillCard from './skill/SkillCard';
import SkillTagModal from './skill/SkillTagModal';
import { filterSkillsByTags, type SkillTagFilterState } from './skill/skillFilter';
import { Button, Input, Modal } from '@arco-design/web-react';
import { CloseSmall, FileZip, FolderOpen, Info, Refresh, Search } from '@icon-park/react';
import React, { useCallback, useEffect, useMemo, useRef, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { useSearchParams } from 'react-router-dom';
import SettingsPageWrapper from './components/SettingsPageWrapper';

/**
 * 卡片网格按「内容容器实际宽度」自动定列(auto-fill),而非视口断点 —— 设置内容
 * 面板被一级 rail + 二级 ContentSider 占去宽度。镜像 AssistantListPanel 的常量。
 * Card grid auto-fits columns to the actual container width (not viewport
 * breakpoints); copied from AssistantListPanel so both surfaces sit on the same
 * 232px lower bound.
 */
const CARD_GRID_COLS = 'repeat(auto-fill, minmax(min(232px, 100%), 1fr))';

interface SkillsHubSettingsProps {
  /** When false, renders without SettingsPageWrapper — useful for embedding in a tab */
  withWrapper?: boolean;
}

const SkillsHubSettings: React.FC<SkillsHubSettingsProps> = ({ withWrapper = true }) => {
  const { t, i18n } = useTranslation();
  const localeKey = resolveLocaleKey(i18n.language);
  const layout = useLayoutContext();
  const isMobile = layout?.isMobile ?? false;
  const [message, messageContext] = useArcoMessage({ maxCount: 10 });

  const [searchParams, setSearchParams] = useSearchParams();
  const highlightName = searchParams.get('highlight');
  const [highlightedSkill, setHighlightedSkill] = useState<string | null>(null);
  const skillRefs = useRef<Record<string, HTMLDivElement | null>>({});

  const [loading, setLoading] = useState(false);
  const [availableSkills, setAvailableSkills] = useState<SkillInfo[]>([]);
  const [skillPaths, setSkillPaths] = useState<{ user_skills_dir: string; builtin_skills_dir: string } | null>(null);
  const [builtinAutoSkills, setBuiltinAutoSkills] = useState<Array<{ name: string; description: string }>>([]);

  const [search_query, setSearchQuery] = useState('');
  const [searchExpanded, setSearchExpanded] = useState(false);
  const [tagFilter, setTagFilter] = useState<SkillTagFilterState>({ audience: [], scenario: [] });
  const [agentImportVisible, setAgentImportVisible] = useState(false);

  // Shared assistant tag vocabulary.
  const tags = useAssistantTags();
  const [tagMgmtVisible, setTagMgmtVisible] = useState(false);
  const [tagModalSkill, setTagModalSkill] = useState<SkillInfo | null>(null);

  // Name set of built-in auto-inject skills → drives the "Auto" badge.
  const autoInjectedNames = useMemo(
    () => new Set(builtinAutoSkills.map((s) => s.name)),
    [builtinAutoSkills]
  );

  const fetchData = useCallback(async () => {
    setLoading(true);
    try {
      const [skills, paths, autoSkills] = await Promise.all([
        ipcBridge.fs.listAvailableSkills.invoke(),
        ipcBridge.fs.getSkillPaths.invoke(),
        ipcBridge.fs.listBuiltinAutoSkills.invoke(),
      ]);
      setAvailableSkills(skills as SkillInfo[]);
      setSkillPaths(paths);
      setBuiltinAutoSkills(autoSkills);
    } catch (error) {
      console.error('Failed to fetch skills:', error);
      message.error(t('settings.skillsHub.fetchError', { defaultValue: 'Failed to fetch skills' }));
    } finally {
      setLoading(false);
    }
  }, [t, message]);

  useEffect(() => {
    void fetchData();
  }, [fetchData]);

  const filteredSkills = useMemo(() => {
    return filterSkillsByTags(availableSkills, search_query, tagFilter, localeKey);
  }, [availableSkills, search_query, tagFilter, localeKey]);

  // Self-heal the tag filter against the current vocabulary: dropping a tag in
  // the management modal must not leave a stale key invisibly constraining a
  // facet. Mirrors AssistantListPanel's guard.
  useEffect(() => {
    const audKeys = new Set(tags.audienceTags.map((tag) => tag.key));
    const scnKeys = new Set(tags.scenarioTags.map((tag) => tag.key));
    setTagFilter((prev) => {
      const audience = prev.audience.filter((k) => audKeys.has(k));
      const scenario = prev.scenario.filter((k) => scnKeys.has(k));
      if (audience.length === prev.audience.length && scenario.length === prev.scenario.length) return prev;
      return { audience, scenario };
    });
  }, [tags.audienceTags, tags.scenarioTags]);

  // Scroll to and highlight a skill when navigated with ?highlight=skillName.
  useEffect(() => {
    if (!highlightName || loading) return;
    const el = skillRefs.current[highlightName];
    if (!el) return;
    requestAnimationFrame(() => {
      el.scrollIntoView({ behavior: 'smooth', block: 'center' });
      setHighlightedSkill(highlightName);
      const timer = setTimeout(() => setHighlightedSkill(null), 2000);
      const next = new URLSearchParams(searchParams);
      next.delete('highlight');
      setSearchParams(next, { replace: true });
      return () => clearTimeout(timer);
    });
  }, [highlightName, loading, filteredSkills, searchParams, setSearchParams]);

  const handleImport = async (skillPath: string) => {
    try {
      const result = await ipcBridge.fs.importSkillWithSymlink.invoke({ skill_path: skillPath });
      const importedNames = result.skill_names?.length
        ? result.skill_names
        : result.skill_name
          ? [result.skill_name]
          : [];
      const count = importedNames.length;
      const names = importedNames.join(', ');
      message.success(
        t('settings.skillsHub.importSuccessDetailed', {
          count,
          names,
          defaultValue: count > 1 ? `Imported ${count} skills: ${names}` : `Imported skill: ${names}`,
        })
      );
      setSearchQuery('');
      void fetchData();
    } catch (error) {
      console.error('Failed to import skill:', error);
      const detail = isBackendHttpError(error) ? error.backendMessage : '';
      message.error(
        detail
          ? t('settings.skillsHub.importErrorDetailed', { detail, defaultValue: `Error importing skill: ${detail}` })
          : t('settings.skillsHub.importError', { defaultValue: 'Error importing skill' })
      );
    }
  };

  const loadAgentSkillSources = useCallback(async (): Promise<ExternalAgentSkillSource[]> => {
    return (await ipcBridge.fs.detectAndCountExternalSkills.invoke()) as ExternalAgentSkillSource[];
  }, []);

  const handleAgentSkillsImported = useCallback(async () => {
    setSearchQuery('');
    await fetchData();
  }, [fetchData]);

  const handleDelete = async (skillName: string) => {
    try {
      await ipcBridge.fs.deleteSkill.invoke({ skill_name: skillName });
      message.success(t('settings.skillsHub.deleteSuccess', { defaultValue: 'Skill deleted' }));
      void fetchData();
    } catch (error) {
      console.error('Failed to delete skill:', error);
      const detail = isBackendHttpError(error) ? error.backendMessage : '';
      message.error(
        detail
          ? t('settings.skillsHub.deleteErrorDetailed', { detail, defaultValue: `Error deleting skill: ${detail}` })
          : t('settings.skillsHub.deleteError', { defaultValue: 'Error deleting skill' })
      );
    }
  };

  const confirmDelete = (skill: SkillInfo) => {
    Modal.confirm({
      title: t('settings.skillsHub.deleteConfirmTitle', { defaultValue: 'Delete Skill' }),
      content: t('settings.skillsHub.deleteConfirmContent', {
        name: skill.name,
        defaultValue: `Are you sure you want to delete "${skill.name}"?`,
      }),
      okButtonProps: { status: 'danger' },
      okText: t('common.delete', { defaultValue: 'Delete' }),
      onOk: () => void handleDelete(skill.name),
      wrapClassName: 'modal-delete-skill',
    });
  };

  // Tauri's open() cannot offer file + directory selection in one dialog, so
  // folder import and .zip import are split into two explicit actions. Both
  // feed handleImport; the backend's is_zip_path routes by extension.
  const handleImportFolder = async () => {
    try {
      const result = await ipcBridge.dialog.showOpen.invoke({ properties: ['openDirectory'] });
      if (result && result.length > 0) await handleImport(result[0]);
    } catch (error) {
      console.error('Failed to open folder dialog:', error);
    }
  };

  const handleImportZip = async () => {
    try {
      const result = await ipcBridge.dialog.showOpen.invoke({
        properties: ['openFile'],
        filters: [{ name: 'Skill zip archives', extensions: ['zip'] }],
      });
      if (result && result.length > 0) await handleImport(result[0]);
    } catch (error) {
      console.error('Failed to open zip dialog:', error);
    }
  };

  const isSearchVisible = searchExpanded || search_query.length > 0;

  const mainContent = (
    <div className='flex flex-col h-full w-full'>
      {messageContext}
      <div className='space-y-16px pb-24px'>
        <div className={`bg-fill-2 rounded-24px ${isMobile ? 'p-16px' : 'p-20px'}`}>
          {/* Header: title + actions */}
          <div className='flex flex-col gap-16px mb-20px'>
            <div className={`flex gap-12px ${isMobile ? 'flex-col' : 'items-start justify-between'}`}>
              <div className='min-w-0'>
                <h2 className='m-0 text-28px font-700 leading-[1.1] text-t-primary'>
                  {t('settings.skillsHub.gridTitle', { defaultValue: 'Skills' })}
                </h2>
                <p className='mt-8px mb-0 max-w-[680px] text-14px text-t-secondary leading-relaxed'>
                  {t('settings.skillsHub.gridDescription', {
                    defaultValue:
                      'Reusable skill packages your assistants can call on. Tag them so they surface under the right filters.',
                  })}
                </p>
              </div>
              <div className={`flex items-center gap-10px ${isMobile ? 'w-full flex-wrap' : 'flex-shrink-0'}`}>
                <Button
                  type='text'
                  size='small'
                  data-testid='btn-refresh-skills'
                  className='!rounded-10px !h-34px !w-34px !p-0 flex items-center justify-center !text-t-secondary hover:!bg-fill-1 hover:!text-t-primary'
                  icon={<Refresh size={16} fill='currentColor' className={loading ? 'animate-spin' : ''} />}
                  onClick={async () => {
                    await fetchData();
                    message.success(t('common.refreshSuccess', { defaultValue: 'Refreshed' }));
                  }}
                  title={t('common.refresh', { defaultValue: 'Refresh' })}
                />
                <Button
                  type={isSearchVisible ? 'secondary' : 'text'}
                  size='small'
                  data-testid='btn-search-toggle'
                  className='!rounded-10px !h-34px !w-34px !p-0 flex items-center justify-center !text-t-secondary hover:!bg-fill-1 hover:!text-t-primary'
                  icon={
                    isSearchVisible ? <CloseSmall size={16} fill='currentColor' /> : <Search size={16} fill='currentColor' />
                  }
                  onClick={() => {
                    if (isSearchVisible) {
                      setSearchExpanded(false);
                      setSearchQuery('');
                      return;
                    }
                    setSearchExpanded(true);
                  }}
                />
                <Button
                  size='small'
                  data-testid='btn-import-agent-skills'
                  className='!rounded-[100px] !h-34px !px-14px !text-t-primary'
                  icon={<FolderOpen size={14} fill='currentColor' />}
                  onClick={() => setAgentImportVisible(true)}
                >
                  {t('settings.agentSkillImport.shortAction', { defaultValue: 'Import from Agent' })}
                </Button>
                <Button
                  size='small'
                  data-testid='btn-manual-import'
                  className='!rounded-[100px] !h-34px !px-14px !text-t-primary'
                  icon={<FolderOpen size={14} fill='currentColor' />}
                  onClick={handleImportFolder}
                >
                  {t('settings.skillsHub.manualImport', { defaultValue: 'Import Skills' })}
                </Button>
                <Button
                  size='small'
                  data-testid='btn-import-zip'
                  className='!rounded-[100px] !h-34px !px-14px !text-t-primary'
                  icon={<FileZip size={14} fill='currentColor' />}
                  onClick={handleImportZip}
                >
                  {t('settings.skillsHub.importZip', { defaultValue: 'Import .zip' })}
                </Button>
              </div>
            </div>

            {isSearchVisible && (
              <Input
                allowClear
                autoFocus
                value={search_query}
                onChange={setSearchQuery}
                data-testid='input-search-skills'
                className='!bg-[var(--color-bg-2)]'
                placeholder={t('settings.skillsHub.searchPlaceholder', { defaultValue: 'Search skills...' })}
                prefix={<Search size={14} fill='currentColor' />}
              />
            )}

            {/* Shared tag filter bar — vocab from useAssistantTags */}
            <AssistantTagFilterBar
              audienceTags={tags.audienceTags}
              scenarioTags={tags.scenarioTags}
              value={tagFilter}
              onChange={setTagFilter}
              localeKey={localeKey}
              onManageTags={() => setTagMgmtVisible(true)}
            />
          </div>

          {/* Single card grid for all skills */}
          {filteredSkills.length > 0 ? (
            <div className='grid gap-12px' style={{ gridTemplateColumns: CARD_GRID_COLS }}>
              {filteredSkills.map((skill) => (
                <SkillCard
                  key={skill.name}
                  skill={skill}
                  tagByKey={tags.tagByKey}
                  localeKey={localeKey}
                  isAutoInjected={skill.source !== 'extension' && autoInjectedNames.has(skill.name)}
                  onEditTags={setTagModalSkill}
                  onDelete={confirmDelete}
                  highlighted={highlightedSkill === skill.name}
                  cardRef={(el) => {
                    skillRefs.current[skill.name] = el;
                  }}
                />
              ))}
            </div>
          ) : (
            <div className='text-center text-t-secondary py-40px'>
              {loading
                ? t('common.loading', { defaultValue: 'Please wait...' })
                : availableSkills.length === 0
                  ? t('settings.skillsHub.noSkills', { defaultValue: 'No skills found. Import some to get started.' })
                  : t('settings.skillsHub.noMatch', { defaultValue: 'No skills match the current filters.' })}
            </div>
          )}

          {/* Skill directory path */}
          {skillPaths && (
            <div className='mt-16px flex items-center gap-8px text-12px text-t-tertiary font-mono'>
              <FolderOpen size={14} className='shrink-0' />
              <span className='truncate' title={skillPaths.user_skills_dir} data-testid='skill-paths-display'>
                {skillPaths.user_skills_dir}
              </span>
            </div>
          )}
        </div>

        {/* Usage tip */}
        <div className='px-16px md:px-[24px] py-20px bg-base border border-b-base shadow-sm rd-16px flex items-start gap-12px text-t-secondary'>
          <Info size={18} className='text-primary-6 mt-2px shrink-0' />
          <div className='flex flex-col gap-4px'>
            <span className='font-bold text-t-primary text-14px'>
              {t('settings.skillsHub.tipTitle', { defaultValue: 'Usage Tip:' })}
            </span>
            <span className='text-13px leading-relaxed'>{t('settings.skillsHub.tipContent')}</span>
          </div>
        </div>
      </div>

      <SkillTagModal
        visible={tagModalSkill !== null}
        skill={tagModalSkill}
        onClose={() => setTagModalSkill(null)}
        audienceTags={tags.audienceTags}
        scenarioTags={tags.scenarioTags}
        onCreateTag={tags.createTag}
        localeKey={localeKey}
        onSaved={fetchData}
        message={message}
      />

      <TagManagementModal
        visible={tagMgmtVisible}
        onClose={() => setTagMgmtVisible(false)}
        audienceTags={tags.audienceTags}
        scenarioTags={tags.scenarioTags}
        localeKey={localeKey}
        onCreate={tags.createTag}
        onRename={tags.renameTag}
        onDelete={tags.deleteTag}
        message={message}
      />

      <AgentSkillImportDrawer
        visible={agentImportVisible}
        onClose={() => setAgentImportVisible(false)}
        existingSkillNames={availableSkills.map((skill) => skill.name)}
        onImported={handleAgentSkillsImported}
        loadSources={loadAgentSkillSources}
      />
    </div>
  );

  return withWrapper ? <SettingsPageWrapper>{mainContent}</SettingsPageWrapper> : mainContent;
};

export default SkillsHubSettings;
