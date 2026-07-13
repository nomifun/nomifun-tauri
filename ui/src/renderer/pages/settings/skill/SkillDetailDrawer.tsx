/**
 * SkillDetailDrawer — read-only inspection for the canonical SKILL.md.
 * Metadata stays visible above the document, while Preview / Source modes let
 * people understand the skill or inspect its exact file without leaving Nomi.
 */
import { ipcBridge } from '@/common';
import type { PresetTag } from '@/common/types/agent/presetTypes';
import MarkdownView from '@/renderer/components/Markdown';
import { useLayoutContext } from '@/renderer/hooks/context/LayoutContext';
import type { SkillInfo } from '@/renderer/pages/settings/PresetSettings/types';
import { Button, Drawer, Spin } from '@arco-design/web-react';
import { Code, FileText, FolderOpen, Lightning, Refresh, SettingOne } from '@icon-park/react';
import React, { useCallback, useEffect, useMemo, useRef, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { resolveSkillDisplay } from './skillDisplay';
import { readSkillContent, stripSkillFrontmatter } from './skillDetail';

type SkillDetailDrawerProps = {
  visible: boolean;
  skill: SkillInfo | null;
  tagByKey: Map<string, PresetTag>;
  localeKey: string;
  isAutoInjected: boolean;
  onClose: () => void;
  onEditTags: (skill: SkillInfo) => void;
};

type ViewMode = 'preview' | 'source';

const SkillDetailDrawer: React.FC<SkillDetailDrawerProps> = ({
  visible,
  skill,
  tagByKey,
  localeKey,
  isAutoInjected,
  onClose,
  onEditTags,
}) => {
  const { t } = useTranslation();
  const isMobile = useLayoutContext()?.isMobile ?? false;
  const [content, setContent] = useState('');
  const [loading, setLoading] = useState(false);
  const [loadFailed, setLoadFailed] = useState(false);
  const [viewMode, setViewMode] = useState<ViewMode>('preview');
  const requestIdRef = useRef(0);

  const loadContent = useCallback(async () => {
    if (!visible || !skill) return;
    const requestId = ++requestIdRef.current;
    setLoading(true);
    setLoadFailed(false);
    try {
      const nextContent = await readSkillContent(skill, {
        readBuiltinSkill: (relativeLocation) =>
          ipcBridge.fs.readBuiltinSkill.invoke({ file_name: relativeLocation }),
        readFile: (location) => ipcBridge.fs.readFile.invoke({ path: location }),
      });
      if (requestId !== requestIdRef.current) return;
      setContent(nextContent);
    } catch (error) {
      if (requestId !== requestIdRef.current) return;
      console.error('Failed to read skill content:', error);
      setContent('');
      setLoadFailed(true);
    } finally {
      if (requestId === requestIdRef.current) setLoading(false);
    }
  }, [skill, visible]);

  useEffect(() => {
    if (!visible || !skill) return;
    setContent('');
    setViewMode('preview');
    void loadContent();
  }, [loadContent, skill, visible]);

  const display = skill ? resolveSkillDisplay(skill, localeKey) : null;
  const previewContent = useMemo(() => stripSkillFrontmatter(content).trim(), [content]);
  const resolveTags = (keys: string[] | undefined) =>
    (keys ?? []).map((key) => tagByKey.get(key)).filter((tag): tag is PresetTag => Boolean(tag));
  const audienceTags = resolveTags(skill?.audience_tags);
  const scenarioTags = resolveTags(skill?.scenario_tags);

  const sourceLabel = isAutoInjected
    ? t('settings.skillsHub.sourceAuto', { defaultValue: 'Auto' })
    : skill?.source === 'custom'
      ? t('settings.skillsHub.custom', { defaultValue: 'Custom' })
      : skill?.source === 'extension'
        ? t('settings.skillsHub.sourceExtension', { defaultValue: 'Extension' })
        : t('settings.skillsHub.builtin', { defaultValue: 'Built-in' });
  const sourceBadgeClass = isAutoInjected
    ? 'bg-[rgba(var(--success-6),0.1)] text-[rgb(var(--success-6))]'
    : skill?.source === 'custom'
      ? 'bg-[rgba(var(--orange-6),0.1)] text-[rgb(var(--orange-6))]'
      : skill?.source === 'extension'
        ? 'bg-fill-2 text-t-secondary'
        : 'bg-primary-1 text-primary-6';

  const renderTagGroup = (label: string, values: PresetTag[]) => (
    <div className='flex min-w-0 items-start gap-10px'>
      <span className='w-72px flex-shrink-0 pt-2px text-11px font-600 uppercase tracking-[0.08em] text-t-tertiary'>
        {label}
      </span>
      <div className='flex min-w-0 flex-1 flex-wrap gap-6px'>
        {values.length > 0 ? (
          values.map((tag) => (
            <span
              key={tag.key}
              className='inline-flex rounded-[12px] border border-solid border-[var(--color-border-2)] bg-fill-2 px-8px py-1px text-11px leading-16px text-t-secondary'
            >
              {tag.label_i18n?.[localeKey] || tag.label}
            </span>
          ))
        ) : (
          <span className='pt-1px text-12px text-t-tertiary'>
            {t('settings.skillsHub.detailNoTags', { defaultValue: 'No tags' })}
          </span>
        )}
      </div>
    </div>
  );

  return (
    <Drawer
      visible={visible}
      onCancel={onClose}
      placement='right'
      width={isMobile ? '100%' : 760}
      zIndex={1250}
      autoFocus={false}
      getPopupContainer={() => document.body}
      title={t('settings.skillsHub.detailTitle', { defaultValue: 'Skill Details' })}
      className='skill-detail-drawer'
      data-testid='skill-detail-drawer'
      headerStyle={{ background: 'var(--color-bg-1)' }}
      bodyStyle={{ background: 'var(--color-bg-1)', padding: 0 }}
      footer={
        <div className='flex w-full items-center justify-between gap-12px'>
          <Button onClick={onClose} className='min-w-100px rounded-[100px] bg-fill-2'>
            {t('common.close', { defaultValue: 'Close' })}
          </Button>
          {skill && (
            <Button
              type='primary'
              icon={<SettingOne size={14} strokeWidth={3} />}
              onClick={() => onEditTags(skill)}
              className='rounded-[100px]'
              data-testid='btn-detail-edit-tags'
            >
              {t('settings.skillsHub.editTags', { defaultValue: 'Edit Tags' })}
            </Button>
          )}
        </div>
      }
    >
      {skill && display && (
        <div className='flex h-full min-h-0 flex-col' data-testid='skill-detail-content'>
          <div className='flex-shrink-0 border-b border-solid border-[var(--color-border-1)] px-24px py-20px'>
            <div className='flex items-start gap-14px'>
              <div
                className={[
                  'flex h-44px w-44px flex-shrink-0 items-center justify-center rounded-12px text-17px font-700 uppercase shadow-sm',
                  isAutoInjected
                    ? 'bg-[rgba(var(--success-6),0.1)] text-[rgb(var(--success-6))]'
                    : 'bg-primary-1 text-primary-6',
                ].join(' ')}
              >
                {isAutoInjected ? <Lightning theme='filled' size={20} /> : skill.name.charAt(0)}
              </div>
              <div className='min-w-0 flex-1'>
                <div className='flex flex-wrap items-center gap-8px'>
                  <h2 className='m-0 break-words text-20px font-700 leading-28px text-t-primary'>{display.name}</h2>
                  <span className={`rounded-[10px] px-8px py-2px text-11px font-600 ${sourceBadgeClass}`}>
                    {sourceLabel}
                  </span>
                </div>
                <p className='mb-0 mt-6px text-13px leading-20px text-t-secondary'>
                  {display.description || t('settings.skillsHub.noDescription', { defaultValue: 'No description provided.' })}
                </p>
              </div>
            </div>

            <div className='mt-18px flex flex-col gap-10px rounded-12px bg-fill-2 p-12px'>
              {renderTagGroup(t('settings.presetTagAudience', { defaultValue: 'Audience' }), audienceTags)}
              {renderTagGroup(t('settings.presetTagScenario', { defaultValue: 'Skill Scenario' }), scenarioTags)}
              <div className='flex min-w-0 items-start gap-10px pt-10px'>
                <span className='w-72px flex-shrink-0 pt-2px text-11px font-600 uppercase tracking-[0.08em] text-t-tertiary'>
                  {t('settings.skillsHub.detailLocation', { defaultValue: 'Location' })}
                </span>
                <div className='flex min-w-0 flex-1 items-center gap-6px'>
                  <FolderOpen size={13} fill='currentColor' className='flex-shrink-0 text-t-tertiary' />
                  <code className='min-w-0 break-all text-11px leading-17px text-t-secondary' title={skill.location}>
                    {skill.location}
                  </code>
                </div>
              </div>
            </div>
          </div>

          <div className='flex min-h-0 flex-1 flex-col px-24px py-18px'>
            <div className='mb-12px flex flex-shrink-0 items-center justify-between gap-12px'>
              <div>
                <div className='text-13px font-700 text-t-primary'>
                  {t('settings.skillsHub.detailInstructions', { defaultValue: 'Instructions' })}
                </div>
                <div className='mt-2px text-11px text-t-tertiary'>SKILL.md</div>
              </div>
              <div className='flex rounded-[100px] bg-fill-2 p-3px' role='tablist'>
                {([
                  ['preview', t('settings.skillsHub.detailPreview', { defaultValue: 'Preview' }), FileText],
                  ['source', t('settings.skillsHub.detailSource', { defaultValue: 'Source' }), Code],
                ] as const).map(([mode, label, Icon]) => (
                  <div
                    key={mode}
                    role='tab'
                    tabIndex={0}
                    aria-selected={viewMode === mode}
                    onClick={() => setViewMode(mode)}
                    onKeyDown={(event) => {
                      if (event.key === 'Enter' || event.key === ' ') {
                        event.preventDefault();
                        setViewMode(mode);
                      }
                    }}
                    className={[
                      'flex cursor-pointer items-center gap-5px rounded-[100px] px-10px py-5px text-11px font-600 transition-colors',
                      viewMode === mode ? 'bg-base text-t-primary shadow-sm' : 'text-t-tertiary hover:text-t-secondary',
                    ].join(' ')}
                  >
                    <Icon size={12} />
                    {label}
                  </div>
                ))}
              </div>
            </div>

            <div className='min-h-0 flex-1 overflow-auto rounded-14px border border-solid border-[var(--color-border-1)] bg-base p-18px'>
              {loading ? (
                <div className='flex h-full min-h-180px items-center justify-center' data-testid='skill-detail-loading'>
                  <Spin size={24} />
                </div>
              ) : loadFailed ? (
                <div
                  className='flex h-full min-h-180px flex-col items-center justify-center gap-10px text-center'
                  data-testid='skill-detail-error'
                >
                  <div className='text-13px font-600 text-t-primary'>
                    {t('settings.skillsHub.detailLoadFailed', { defaultValue: 'Could not load this skill file.' })}
                  </div>
                  <div className='max-w-360px text-12px leading-18px text-t-tertiary'>
                    {t('settings.skillsHub.detailLoadFailedHint', {
                      defaultValue: 'The file may have moved or is no longer readable. Refresh the skill list and try again.',
                    })}
                  </div>
                  <Button size='small' icon={<Refresh size={13} />} onClick={() => void loadContent()}>
                    {t('common.retry', { defaultValue: 'Retry' })}
                  </Button>
                </div>
              ) : viewMode === 'source' ? (
                <pre
                  className='m-0 min-w-max whitespace-pre font-mono text-12px leading-19px text-t-secondary'
                  data-testid='skill-detail-source'
                >
                  {content}
                </pre>
              ) : previewContent ? (
                <MarkdownView hiddenCodeCopyButton className='text-13px' fontSize='13px' lineHeight='1.65'>
                  {previewContent}
                </MarkdownView>
              ) : (
                <div className='flex min-h-180px items-center justify-center text-12px text-t-tertiary'>
                  {t('settings.skillsHub.detailEmpty', { defaultValue: 'This skill has no instructions yet.' })}
                </div>
              )}
            </div>
          </div>
        </div>
      )}
    </Drawer>
  );
};

export default SkillDetailDrawer;
