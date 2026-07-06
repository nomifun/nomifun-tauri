/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

/**
 * KnowledgeDetailPage — Tab-shell redesign (Phase D).
 *
 * Structure:
 *   Header: back + kind icon + name + kind badge + tags + actions + meta row
 *   Tabs:   docs | inbox(n) | use | set
 *
 * Each tab body is a placeholder for D2-D5 tasks.
 * Existing document/inbox logic is preserved inline under the "docs"/"inbox" tabs.
 */

import classNames from 'classnames';
import React, { useCallback, useEffect, useMemo, useState } from 'react';
import { useNavigate, useParams, useSearchParams } from 'react-router-dom';
import { useTranslation } from 'react-i18next';
import type { TFunction } from 'i18next';
import {
  Badge,
  Button,
  Checkbox,
  Dropdown,
  Empty,
  Input,
  Menu,
  Message,
  Modal,
  Result,
  Spin,
  Tabs,
  Tag,
  Tree,
} from '@arco-design/web-react';
import {
  ApiApp,
  Delete,
  Earth,
  EditTwo,
  FileText,
  FolderOpen,
  FolderPlus,
  Left,
  LinkCloud,
  LinkOne,
  MagicHat,
  More,
  Plus,
  Refresh,
  Search,
  SettingOne,
  SettingTwo,
  Upload,
} from '@icon-park/react';
import type { IKnowledgeBase, IKnowledgeTag, IKnowledgeTreeEntry } from '@/common/adapter/ipcBridge';
import Markdown from '@renderer/components/Markdown';
import { useLayoutContext } from '@renderer/hooks/context/LayoutContext';
import { ipcBridge } from '@/common';
import {
  formatSize,
  getBaseSource,
  isAutogenNoProviderError,
  knowledgeErrorText,
  notifySourceFetchResult,
  useKnowledgeBase,
  useKnowledgeInbox,
} from '../useKnowledge';
import { useKnowledgeTags } from '../useKnowledgeTags';
import KnowledgeModelSelector, { useKnowledgeAutogenModel } from '../KnowledgeModelSelector';
import InboxReviewPanel from '../InboxReviewPanel';
import KnowledgeConnectorDrawer from '../KnowledgeConnectorDrawer';
import KnowledgeConsumersSection from '../KnowledgeConsumersSection';
import TagPicker from '../CreateStudio/TagPicker';
import { FEISHU_KNOWLEDGE_CREATION_ENABLED } from '../CreateStudio/sourceTypes';
import {
  buildKnowledgeSearchTree,
  isKnowledgePathWithin,
  knowledgeFolderPathChain,
  mergeKnowledgeTreeChildren,
  parentDirOfKnowledgePath,
  preserveKnowledgeTreeChildren,
  replaceKnowledgePathPrefix,
} from './treeModel';

// ─── Tab keys (maps to ?tab= query values) ─────────────────────────────────────

type TabKey = 'docs' | 'inbox' | 'use' | 'set';
const ALL_TABS: TabKey[] = ['docs', 'inbox', 'use', 'set'];

// ─── Kind config (mirrors KnowledgeCard — intentionally duplicated to avoid
//     circular deps; will be extracted to shared module in future cleanup) ────────

type KindConfig = {
  label: string;
  bgClass: string;
  textClass: string;
  borderClass: string;
  iconBg: string;
  iconBorder: string;
  iconColor: string;
};

function getKindConfig(kind: IKnowledgeBase['kind'], t: TFunction): KindConfig {
  switch (kind) {
    case 'local':
      return {
        label: t('knowledge.card.kindLocal', { defaultValue: '本地文件夹' }),
        bgClass: 'bg-[rgba(var(--primary-6),0.1)]',
        textClass: 'text-[rgb(var(--primary-5))]',
        borderClass: 'border-[rgba(var(--primary-6),0.3)]',
        iconBg: 'rgba(var(--primary-6),0.1)',
        iconBorder: 'rgba(var(--primary-6),0.3)',
        iconColor: 'rgb(var(--primary-5))',
      };
    case 'web':
      return {
        label: t('knowledge.card.kindWeb', { defaultValue: '网页' }),
        bgClass: 'bg-[rgba(var(--success-6),0.1)]',
        textClass: 'text-[rgb(var(--success-5))]',
        borderClass: 'border-[rgba(var(--success-6),0.3)]',
        iconBg: 'rgba(var(--success-6),0.1)',
        iconBorder: 'rgba(var(--success-6),0.3)',
        iconColor: 'rgb(var(--success-5))',
      };
    case 'feishu':
      return {
        label: t('knowledge.card.kindFeishu', { defaultValue: '飞书' }),
        bgClass: 'bg-[rgba(var(--warning-6),0.12)]',
        textClass: 'text-[rgb(var(--warning-5))]',
        borderClass: 'border-[rgba(var(--warning-6),0.3)]',
        iconBg: 'rgba(var(--warning-6),0.12)',
        iconBorder: 'rgba(var(--warning-6),0.3)',
        iconColor: 'rgb(var(--warning-5))',
      };
    case 'blank':
    default:
      return {
        label: t('knowledge.card.kindBlank', { defaultValue: '空白' }),
        bgClass: 'bg-fill-2',
        textClass: 'text-[var(--color-text-2)]',
        borderClass: 'border-[var(--color-border-2)]',
        iconBg: 'var(--color-fill-2)',
        iconBorder: 'var(--color-border-2)',
        iconColor: 'var(--color-text-2)',
      };
  }
}

/** Kind icon in a rounded square (52px for detail header, bigger than card). */
function DetailKindIcon({ kind, config }: { kind: IKnowledgeBase['kind']; config: KindConfig }) {
  const iconProps = { theme: 'outline' as const, size: 22, strokeWidth: 3 };
  return (
    <div
      className='w-52px h-52px rounded-14px flex-none grid place-items-center border border-solid'
      style={{ background: config.iconBg, borderColor: config.iconBorder, color: config.iconColor }}
    >
      {kind === 'local' && <FolderOpen {...iconProps} />}
      {kind === 'web' && <Earth {...iconProps} />}
      {kind === 'feishu' && <SettingOne {...iconProps} />}
      {kind === 'blank' && <EditTwo {...iconProps} />}
    </div>
  );
}

function collectKnowledgeDirKeys(nodes: IKnowledgeTreeEntry[]): string[] {
  const keys: string[] = [];
  const visit = (items: IKnowledgeTreeEntry[]) => {
    for (const item of items) {
      if (!item.is_dir) continue;
      keys.push(item.rel_path);
      if (item.children?.length) visit(item.children);
    }
  };
  visit(nodes);
  return keys;
}

// ─── Settings Tab (D5) ────────────────────────────────────────────────────────

interface SettingsTabProps {
  base: IKnowledgeBase;
  allTags: IKnowledgeTag[];
  createTag: (label: string) => Promise<IKnowledgeTag>;
  onRefresh: () => void;
  onConnectorOpen: () => void;
}

const SettingsTab: React.FC<SettingsTabProps> = ({ base, allTags, createTag, onRefresh, onConnectorOpen }) => {
  const { t } = useTranslation();
  const navigate = useNavigate();

  // ─── Editable fields (local state, save on button click) ──────────────────
  const [editName, setEditName] = useState(base.name);
  const [editDesc, setEditDesc] = useState(base.description);
  const [editTags, setEditTags] = useState<string[]>(base.tags);
  const [saving, setSaving] = useState(false);

  // Sync local state when base changes from parent refresh
  useEffect(() => {
    setEditName(base.name);
    setEditDesc(base.description);
    setEditTags(base.tags);
  }, [base.name, base.description, base.tags]);

  const isDirty = editName !== base.name || editDesc !== base.description || JSON.stringify(editTags) !== JSON.stringify(base.tags);

  const handleSaveInfo = async () => {
    if (!isDirty) return;
    setSaving(true);
    try {
      await ipcBridge.knowledge.updateBase.invoke({
        id: base.id,
        name: editName.trim() || base.name,
        description: editDesc,
        tags: editTags,
      });
      Message.success(t('knowledge.detail.settings.saveOk', { defaultValue: '保存成功' }));
      onRefresh();
    } catch (e) {
      Message.error(String(e));
    } finally {
      setSaving(false);
    }
  };

  // ─── Source actions (per kind) ────────────────────────────────────────────
  const [sourceLoading, setSourceLoading] = useState(false);

  const handleRefreshSource = async () => {
    if (sourceLoading) return;
    setSourceLoading(true);
    try {
      const summary = await ipcBridge.knowledge.refreshSource.invoke({ id: base.id });
      notifySourceFetchResult(t, summary, t('knowledge.source.refreshOk', { defaultValue: '刷新完成，获取 {{fetched}} 条', fetched: summary.fetched }));
      onRefresh();
    } catch (e) {
      Message.error(knowledgeErrorText(e));
    } finally {
      setSourceLoading(false);
    }
  };

  const handleSyncSource = async () => {
    if (sourceLoading) return;
    setSourceLoading(true);
    try {
      const summary = await ipcBridge.knowledge.syncSource.invoke({ id: base.id });
      notifySourceFetchResult(t, summary, t('knowledge.source.syncOk', { defaultValue: '同步完成，获取 {{fetched}} 条', fetched: summary.fetched }));
      onRefresh();
    } catch (e) {
      Message.error(knowledgeErrorText(e));
    } finally {
      setSourceLoading(false);
    }
  };

  // ─── Danger zone: export ──────────────────────────────────────────────────
  const [exporting, setExporting] = useState(false);

  const handleExport = async () => {
    if (exporting) return;
    const dirs = await ipcBridge.dialog.showOpen.invoke({ properties: ['openDirectory'] });
    if (!dirs || dirs.length === 0) return;
    const destDir = dirs[0];
    setExporting(true);
    try {
      const { dest_path } = await ipcBridge.knowledge.exportBase.invoke({
        id: base.id,
        dest_path: destDir,
      });
      Message.success(t('knowledge.detail.settings.exportOk', { defaultValue: '已导出至 {{path}}', path: dest_path }));
    } catch (e) {
      Message.error(String(e));
    } finally {
      setExporting(false);
    }
  };

  // ─── Danger zone: delete ──────────────────────────────────────────────────
  const [deleteModalVisible, setDeleteModalVisible] = useState(false);
  const [purge, setPurge] = useState(false);
  const [deleting, setDeleting] = useState(false);

  const handleDelete = async () => {
    setDeleting(true);
    try {
      await ipcBridge.knowledge.deleteBase.invoke({ id: base.id, purge });
      Message.success(t('knowledge.detail.settings.deleteOk', { defaultValue: '已删除' }));
      navigate('/knowledge');
    } catch (e) {
      Message.error(String(e));
    } finally {
      setDeleting(false);
      setDeleteModalVisible(false);
    }
  };

  return (
    <div className='flex flex-col gap-16px max-w-560px'>
      {/* ─── Basic info: name / description / tags ─── */}
      <div className='flex flex-col gap-16px'>
        {/* Name */}
        <div className='flex flex-col gap-7px'>
          <label className='block text-13px text-[var(--color-text-2)]'>
            {t('knowledge.detail.settings.labelName', { defaultValue: '名称' })}
          </label>
          <Input
            value={editName}
            onChange={setEditName}
            placeholder={t('knowledge.detail.settings.namePlaceholder', { defaultValue: '知识库名称' })}
          />
        </div>

        {/* Description */}
        <div className='flex flex-col gap-7px'>
          <label className='block text-13px text-[var(--color-text-2)]'>
            {t('knowledge.detail.settings.labelDesc', { defaultValue: '描述（注入会话提示词）' })}
          </label>
          <Input.TextArea
            value={editDesc}
            onChange={setEditDesc}
            autoSize={{ minRows: 3, maxRows: 8 }}
            placeholder={t('knowledge.detail.settings.descPlaceholder', { defaultValue: '简要描述知识库内容和用途' })}
          />
        </div>

        {/* Tags */}
        <div className='flex flex-col gap-7px'>
          <label className='block text-13px text-[var(--color-text-2)]'>
            {t('knowledge.detail.settings.labelTags', { defaultValue: '标签' })}
          </label>
          <TagPicker value={editTags} onChange={setEditTags} tags={allTags} createTag={createTag} />
        </div>

        {/* Save button */}
        <div>
          <Button type='primary' loading={saving} disabled={!isDirty} onClick={() => void handleSaveInfo()}>
            {t('knowledge.detail.settings.save', { defaultValue: '保存修改' })}
          </Button>
        </div>
      </div>

      {/* ─── Source section (varies by kind) ─── */}
      <div className='flex flex-col gap-7px'>
        <label className='block text-13px text-[var(--color-text-2)]'>
          {t('knowledge.detail.settings.labelSource', { defaultValue: '来源' })}
          {' · '}
          {base.kind === 'local' && t('knowledge.card.kindLocal', { defaultValue: '本地文件夹' })}
          {base.kind === 'web' && t('knowledge.card.kindWeb', { defaultValue: '网页' })}
          {base.kind === 'feishu' && t('knowledge.card.kindFeishu', { defaultValue: '飞书' })}
          {base.kind === 'blank' && t('knowledge.card.kindBlank', { defaultValue: '空白' })}
        </label>

        {base.kind === 'local' && (
          <div className='flex items-center gap-9px'>
            <Input value={base.root_path} readOnly className='flex-1' />
            <Button
              icon={<FolderOpen theme='outline' size='14' />}
              onClick={() => {
                void ipcBridge.shell.openFolderWith.invoke({ folder_path: base.root_path, tool: 'explorer' }).catch((e: unknown) => Message.error(String(e)));
              }}
            >
              {t('knowledge.detail.settings.openFolder', { defaultValue: '打开' })}
            </Button>
          </div>
        )}

        {base.kind === 'web' && (
          <div className='flex items-center gap-9px'>
            <span className='text-12px text-[var(--color-text-3)]'>
              {t('knowledge.detail.settings.webHint', { defaultValue: '网页来源 — 点击"刷新"重新抓取所有 URL。' })}
            </span>
            <Button
              icon={<Refresh theme='outline' size='14' />}
              loading={sourceLoading}
              onClick={() => void handleRefreshSource()}
            >
              {t('knowledge.detail.settings.refreshSource', { defaultValue: '刷新' })}
            </Button>
          </div>
        )}

        {base.kind === 'feishu' && (
          <div className='flex items-center gap-9px'>
            <Button
              icon={<Refresh theme='outline' size='14' />}
              loading={sourceLoading}
              onClick={() => void handleSyncSource()}
            >
              {t('knowledge.detail.settings.syncSource', { defaultValue: '同步' })}
            </Button>
            <Button
              icon={<ApiApp theme='outline' size='14' />}
              disabled={!FEISHU_KNOWLEDGE_CREATION_ENABLED}
              className={classNames(!FEISHU_KNOWLEDGE_CREATION_ENABLED && 'cursor-not-allowed opacity-50')}
              onClick={FEISHU_KNOWLEDGE_CREATION_ENABLED ? onConnectorOpen : undefined}
            >
              {t('knowledge.detail.settings.connector', { defaultValue: '连接器' })}
            </Button>
          </div>
        )}
      </div>

      {/* ─── Danger zone ─── */}
      <div className='box-border rd-12px border border-solid border-[rgba(var(--danger-6),0.3)] p-16px mt-8px'>
        <div className='text-13px font-700 text-[rgb(var(--danger-6))] mb-10px'>
          {t('knowledge.detail.settings.dangerTitle', { defaultValue: '危险操作' })}
        </div>
        {/* Export */}
        <div className='flex items-center justify-between gap-12px mb-9px'>
          <p className='m-0 text-12px text-[var(--color-text-3)]'>
            {t('knowledge.detail.settings.exportDesc', { defaultValue: '导出为 .zip 备份包' })}
          </p>
          <Button size='small' loading={exporting} onClick={() => void handleExport()}>
            {t('knowledge.detail.settings.exportBtn', { defaultValue: '导出' })}
          </Button>
        </div>
        {/* Delete */}
        <div className='flex items-center justify-between gap-12px'>
          <p className='m-0 text-12px text-[var(--color-text-3)]'>
            {t('knowledge.detail.settings.deleteDesc', { defaultValue: '删除此知识库' })}
            {!base.managed && (
              <span className='block text-11px mt-2px text-[var(--color-text-3)]'>
                {t('knowledge.detail.settings.deleteLocalHint', { defaultValue: '（本地引用目录不会被删除）' })}
              </span>
            )}
          </p>
          <Button
            size='small'
            status='danger'
            onClick={() => setDeleteModalVisible(true)}
          >
            {t('knowledge.detail.settings.deleteBtn', { defaultValue: '删除知识库' })}
          </Button>
        </div>
      </div>

      {/* Delete confirmation modal */}
      <Modal
        title={t('knowledge.detail.settings.deleteModalTitle', { defaultValue: '确认删除知识库' })}
        visible={deleteModalVisible}
        onCancel={() => setDeleteModalVisible(false)}
        onOk={() => void handleDelete()}
        confirmLoading={deleting}
        okButtonProps={{ status: 'danger' }}
        okText={t('knowledge.detail.settings.deleteConfirm', { defaultValue: '确认删除' })}
      >
        <p className='text-13px text-[var(--color-text-2)] mb-12px'>
          {t('knowledge.detail.settings.deleteWarning', {
            defaultValue: '删除后无法恢复。知识库的所有文档、待审内容、挂载关系将被清除。',
          })}
        </p>
        {base.managed && (
          <Checkbox checked={purge} onChange={setPurge}>
            <span className='text-12px text-[var(--color-text-3)]'>
              {t('knowledge.detail.settings.purgeOption', { defaultValue: '同时删除磁盘上的数据目录' })}
            </span>
          </Checkbox>
        )}
        {!base.managed && (
          <p className='text-12px text-[var(--color-text-3)] m-0 mt-8px'>
            {t('knowledge.detail.settings.deleteLocalNote', {
              defaultValue: '本知识库引用的外部目录（{{path}}）不会被删除，仅取消关联。',
              path: base.root_path,
            })}
          </p>
        )}
      </Modal>
    </div>
  );
};

// ─── Main Component ─────────────────────────────────────────────────────────────

const KnowledgeDetailPage: React.FC = () => {
  const { t } = useTranslation();
  const navigate = useNavigate();
  const { id } = useParams<{ id: string }>();
  const [searchParams, setSearchParams] = useSearchParams();
  const layout = useLayoutContext();
  const isMobile = layout?.isMobile ?? false;

  // ─── Data hooks ─────────────────────────────────────────────────────────────
  const { base, files, tree, loading, error, refresh } = useKnowledgeBase(id);
  const { items: inboxItems, loading: inboxLoading, refresh: refreshInbox } = useKnowledgeInbox(id);
  const { choice: modelChoice, setChoice: setModelChoice } = useKnowledgeAutogenModel();
  const { tags: allTags, createTag } = useKnowledgeTags();

  // ─── Tab routing via ?tab= ──────────────────────────────────────────────────
  const rawTabParam = searchParams.get('tab');
  const activeTab: TabKey = rawTabParam && ALL_TABS.includes(rawTabParam as TabKey) ? (rawTabParam as TabKey) : 'docs';

  const setTab = useCallback(
    (key: string) => {
      setSearchParams(
        (prev) => {
          prev.set('tab', key);
          return prev;
        },
        { replace: true }
      );
    },
    [setSearchParams]
  );

  // ─── Tag resolution ─────────────────────────────────────────────────────────
  const tagMap = useMemo(() => {
    const m: Record<string, IKnowledgeTag> = {};
    for (const tag of allTags) m[tag.key] = tag;
    return m;
  }, [allTags]);

  // ─── Document state (preserved from original — D2 will own this) ────────────
  const [selectedPath, setSelectedPath] = useState<string | null>(null);
  const [content, setContent] = useState<string>('');
  const [fileLoading, setFileLoading] = useState(false);
  const [editMode, setEditMode] = useState(false);
  const [draft, setDraft] = useState('');
  const [saving, setSaving] = useState(false);
  const [newFileVisible, setNewFileVisible] = useState(false);
  const [newFilePath, setNewFilePath] = useState('');
  const [newFolderVisible, setNewFolderVisible] = useState(false);
  const [newFolderPath, setNewFolderPath] = useState('');
  const [renameVisible, setRenameVisible] = useState(false);
  const [renameTarget, setRenameTarget] = useState<IKnowledgeTreeEntry | null>(null);
  const [renameName, setRenameName] = useState('');
  const [autogenLoading, setAutogenLoading] = useState(false);
  const [refreshingSource, setRefreshingSource] = useState(false);
  const [connectorVisible, setConnectorVisible] = useState(false);
  const [treeData, setTreeData] = useState<IKnowledgeTreeEntry[]>([]);
  const [expandedTreeKeys, setExpandedTreeKeys] = useState<string[]>([]);
  const [selectedFolderPath, setSelectedFolderPath] = useState('');
  const [selectedTreeKey, setSelectedTreeKey] = useState<string | null>(null);

  const handleConnectorOpen = useCallback(() => {
    if (!FEISHU_KNOWLEDGE_CREATION_ENABLED) return;
    setConnectorVisible(true);
  }, []);
  const [fileSearch, setFileSearch] = useState('');
  const isTreeSearch = fileSearch.trim().length > 0;

  const source = getBaseSource(base);

  useEffect(() => {
    setTreeData((prev) => preserveKnowledgeTreeChildren(tree, prev));
  }, [tree]);

  const handleInboxChanged = () => {
    void refresh();
    void refreshInbox();
  };

  // Auto-select first file
  useEffect(() => {
    if (!selectedPath && files.length > 0) {
      setSelectedPath(files[0].rel_path);
      setSelectedTreeKey(files[0].rel_path);
    }
    if (selectedPath && !files.some((f) => f.rel_path === selectedPath)) {
      const nextPath = files.length > 0 ? files[0].rel_path : null;
      setSelectedPath(nextPath);
      setSelectedTreeKey(nextPath);
    }
  }, [files, selectedPath]);

  // Reset per-base view state when switching knowledge bases — the route param
  // changes but React reuses this component instance, so the previous base's
  // document search query / edit mode would otherwise leak into the next base
  // (looking like "documents missing"). selectedPath is reconciled above.
  useEffect(() => {
    setFileSearch('');
    setEditMode(false);
    setExpandedTreeKeys([]);
    setSelectedFolderPath('');
    setSelectedTreeKey(null);
  }, [id]);

  // Load file content
  useEffect(() => {
    if (!id || !selectedPath) {
      setContent('');
      return;
    }
    let cancelled = false;
    setFileLoading(true);
    setEditMode(false);
    ipcBridge.knowledge.readFile
      .invoke({ id, path: selectedPath })
      .then((res) => {
        if (!cancelled) setContent(res.content);
      })
      .catch((e) => {
        if (!cancelled) Message.error(String(e));
      })
      .finally(() => {
        if (!cancelled) setFileLoading(false);
      });
    return () => {
      cancelled = true;
    };
  }, [id, selectedPath]);

  const startEdit = () => {
    setDraft(content);
    setEditMode(true);
  };

  const handleSave = async () => {
    if (!id || !selectedPath) return;
    setSaving(true);
    try {
      await ipcBridge.knowledge.writeFile.invoke({ id, path: selectedPath, content: draft });
      setContent(draft);
      setEditMode(false);
      Message.success(t('knowledge.actions.saveOk'));
      void refresh();
    } catch (e) {
      Message.error(String(e));
    } finally {
      setSaving(false);
    }
  };

  const handleLoadTreeChildren = useCallback(
    async (node: IKnowledgeTreeEntry) => {
      if (!id || node.is_file || isTreeSearch) return;
      const children = await ipcBridge.knowledge.listTree.invoke({ id, path: node.rel_path });
      setTreeData((prev) => mergeKnowledgeTreeChildren(prev, node.rel_path, children));
    },
    [id, isTreeSearch]
  );

  const reloadTreePath = useCallback(
    async (folderPath: string) => {
      if (!id) return;
      const rootChildren = await ipcBridge.knowledge.listTree.invoke({ id });
      setTreeData(rootChildren);

      const branchesToReload = knowledgeFolderPathChain(folderPath);
      for (const branchPath of branchesToReload) {
        const children = await ipcBridge.knowledge.listTree.invoke({ id, path: branchPath });
        setTreeData((prev) => mergeKnowledgeTreeChildren(prev, branchPath, children));
      }
      if (branchesToReload.length > 0) {
        setExpandedTreeKeys((prev) => [...new Set([...prev, ...branchesToReload])]);
      }
    },
    [id]
  );

  const openNewFileModal = (folderOverride?: string) => {
    const folder = folderOverride ?? (selectedFolderPath || parentDirOfKnowledgePath(selectedPath));
    setNewFilePath(folder ? `${folder}/` : '');
    setNewFileVisible(true);
  };

  const openNewFolderModal = (folderOverride?: string) => {
    const folder = folderOverride ?? (selectedFolderPath || parentDirOfKnowledgePath(selectedPath));
    setNewFolderPath(folder ? `${folder}/` : '');
    setNewFolderVisible(true);
  };

  const openRenameModal = (item: IKnowledgeTreeEntry) => {
    setRenameTarget(item);
    setRenameName(item.name);
    setRenameVisible(true);
  };

  const handleCreateFile = async () => {
    if (!id) return;
    let path = newFilePath.trim();
    if (!path) return;
    if (!path.toLowerCase().endsWith('.md')) path = `${path}.md`;
    const parent = parentDirOfKnowledgePath(path);
    const fileTitle = path.split('/').filter(Boolean).at(-1)?.replace(/\.md$/i, '') || path.replace(/\.md$/i, '');
    try {
      await ipcBridge.knowledge.writeFile.invoke({ id, path, content: `# ${fileTitle}\n` });
      setNewFileVisible(false);
      setNewFilePath('');
      await refresh();
      setFileSearch('');
      await reloadTreePath(parent);
      setSelectedPath(path);
      setSelectedTreeKey(path);
      Message.success(t('knowledge.actions.createOk'));
    } catch (e) {
      Message.error(String(e));
    }
  };

  const handleCreateFolder = async () => {
    if (!id) return;
    const path = newFolderPath.trim().replace(/\\/g, '/').replace(/^\/+|\/+$/g, '');
    if (!path) return;
    const parent = parentDirOfKnowledgePath(path);
    try {
      await ipcBridge.knowledge.createFolder.invoke({ id, path });
      setNewFolderVisible(false);
      setNewFolderPath('');
      setFileSearch('');
      await reloadTreePath(parent);
      setSelectedFolderPath(path);
      setSelectedTreeKey(path);
      Message.success(t('knowledge.actions.createFolderOk', { defaultValue: '文件夹已创建' }));
    } catch (e) {
      Message.error(String(e));
    }
  };

  const handleRenameTreeEntry = async () => {
    if (!id || !renameTarget) return;
    let newName = renameName.trim();
    if (!newName) return;
    if (renameTarget.is_file && !newName.toLowerCase().endsWith('.md')) newName = `${newName}.md`;
    const oldPath = renameTarget.rel_path;
    const parent = parentDirOfKnowledgePath(oldPath);
    try {
      const renamed = await ipcBridge.knowledge.renameTreeEntry.invoke({ id, path: oldPath, newName });
      setRenameVisible(false);
      setRenameTarget(null);
      setRenameName('');
      setFileSearch('');
      setExpandedTreeKeys((prev) =>
        prev.map((key) => replaceKnowledgePathPrefix(key, oldPath, renamed.rel_path) ?? key)
      );
      setSelectedPath((prev) => replaceKnowledgePathPrefix(prev, oldPath, renamed.rel_path));
      setSelectedFolderPath((prev) => replaceKnowledgePathPrefix(prev || null, oldPath, renamed.rel_path) || '');
      setSelectedTreeKey((prev) => replaceKnowledgePathPrefix(prev, oldPath, renamed.rel_path));
      await refresh();
      await reloadTreePath(parent);
      Message.success(t('knowledge.actions.renameOk', { defaultValue: '已重命名' }));
    } catch (e) {
      Message.error(String(e));
    }
  };

  const handleDeleteFile = async (path: string) => {
    if (!id) return;
    const parent = parentDirOfKnowledgePath(path);
    try {
      await ipcBridge.knowledge.deleteFile.invoke({ id, path });
      Message.success(t('knowledge.actions.deleteOk'));
      if (selectedPath === path) {
        setSelectedPath(null);
        setSelectedTreeKey(parent || null);
      }
      await refresh();
      await reloadTreePath(parent);
    } catch (e) {
      Message.error(String(e));
    }
  };

  const handleDeleteFolder = async (path: string) => {
    if (!id) return;
    const parent = parentDirOfKnowledgePath(path);
    try {
      await ipcBridge.knowledge.deleteFolder.invoke({ id, path });
      Message.success(t('knowledge.actions.deleteFolderOk', { defaultValue: '目录已删除' }));
      setFileSearch('');
      setExpandedTreeKeys((prev) => prev.filter((key) => !isKnowledgePathWithin(key, path)));
      if (isKnowledgePathWithin(selectedPath, path)) {
        setSelectedPath(null);
        setContent('');
        setDraft('');
        setEditMode(false);
      }
      if (isKnowledgePathWithin(selectedTreeKey, path)) {
        setSelectedTreeKey(parent || null);
      }
      if (isKnowledgePathWithin(selectedFolderPath || null, path)) {
        setSelectedFolderPath(parent);
      }
      await refresh();
      await reloadTreePath(parent);
    } catch (e) {
      Message.error(String(e));
    }
  };

  const confirmDeleteTreeEntry = (item: IKnowledgeTreeEntry) => {
    if (item.is_dir) {
      Modal.confirm({
        title: t('knowledge.tree.deleteFolderTitle', { defaultValue: '确认删除目录？' }),
        content: (
          <div className='text-13px leading-20px text-[var(--color-text-2)]'>
            <div>
              {t('knowledge.tree.deleteFolderWarning', {
                defaultValue: '删除目录“{{name}}”会一并清空其下所有文档和子目录，无法撤销。',
                name: item.name,
              })}
            </div>
            <div className='mt-6px break-all text-[var(--color-text-3)]'>{item.rel_path}</div>
          </div>
        ),
        okButtonProps: { status: 'danger' },
        okText: t('knowledge.actions.delete', { defaultValue: '删除' }),
        onOk: () => handleDeleteFolder(item.rel_path),
      });
      return;
    }

    Modal.confirm({
      title: t('knowledge.actions.deleteFileConfirm', { defaultValue: '确认删除该文档？' }),
      content: <div className='break-all text-[var(--color-text-3)]'>{item.rel_path}</div>,
      okButtonProps: { status: 'danger' },
      okText: t('knowledge.actions.delete', { defaultValue: '删除' }),
      onOk: () => handleDeleteFile(item.rel_path),
    });
  };

  const handleTreeNodeMenuClick = (key: string, item: IKnowledgeTreeEntry) => {
    if (key === 'new-file' && item.is_dir) {
      openNewFileModal(item.rel_path);
      return;
    }
    if (key === 'new-folder' && item.is_dir) {
      openNewFolderModal(item.rel_path);
      return;
    }
    if (key === 'rename') {
      openRenameModal(item);
      return;
    }
    if (key === 'delete') {
      confirmDeleteTreeEntry(item);
    }
  };

  const handleOpenFolder = async () => {
    if (!base) return;
    try {
      await ipcBridge.shell.openFolderWith.invoke({ folder_path: base.root_path, tool: 'explorer' });
    } catch (e) {
      Message.error(String(e));
    }
  };

  const handleAutogen = async () => {
    if (!id || autogenLoading) return;
    setAutogenLoading(true);
    try {
      const res = await ipcBridge.knowledge.autogenBase.invoke({ id, ...(modelChoice ?? {}) });
      Message.success(
        t(res.readme_written ? 'knowledge.actions.autogenOkReadme' : 'knowledge.actions.autogenOkNoReadme')
      );
      void refresh();
    } catch (e) {
      Message.error(isAutogenNoProviderError(e) ? t('knowledge.actions.autogenNoProvider') : knowledgeErrorText(e));
    } finally {
      setAutogenLoading(false);
    }
  };

  const handleRefreshSource = async () => {
    if (!id || refreshingSource) return;
    setRefreshingSource(true);
    try {
      const summary = await ipcBridge.knowledge.refreshSource.invoke({ id });
      notifySourceFetchResult(t, summary, t('knowledge.source.refreshOk', { fetched: summary.fetched }));
      void refresh();
    } catch (e) {
      Message.error(knowledgeErrorText(e));
    } finally {
      setRefreshingSource(false);
    }
  };

  // ─── Computed ───────────────────────────────────────────────────────────────
  const kindConfig = base ? getKindConfig(base.kind, t) : null;
  const pendingCount = base?.pending_inbox ?? inboxItems.length;

  const displayedTreeData = useMemo(
    () => (isTreeSearch ? buildKnowledgeSearchTree(files, fileSearch) : treeData),
    [files, fileSearch, isTreeSearch, treeData]
  );
  const visibleTreeExpandedKeys = useMemo(
    () => (isTreeSearch ? collectKnowledgeDirKeys(displayedTreeData) : expandedTreeKeys),
    [displayedTreeData, expandedTreeKeys, isTreeSearch]
  );

  // Build breadcrumb segments from selected path
  const breadcrumbSegments = useMemo(() => {
    if (!selectedPath) return [];
    return selectedPath.split('/');
  }, [selectedPath]);

  const relativeTime = useMemo(() => {
    if (!base?.updated_at) return '';
    // updated_at is already epoch-MILLIS (TimestampMs / now_ms() on the backend);
    // KnowledgeCard's formatRelativeTime treats it as ms directly. The stray
    // `* 1000` here pushed it ~1.7e15, making diffMin always < 1 → forever "刚刚".
    const diffMs = Date.now() - base.updated_at;
    const diffMin = Math.floor(diffMs / 60000);
    if (diffMin < 1) return t('knowledge.detail.justNow', { defaultValue: '刚刚' });
    if (diffMin < 60) return t('knowledge.detail.minutesAgo', { defaultValue: '{{n}} 分钟前', n: diffMin });
    const diffH = Math.floor(diffMin / 60);
    if (diffH < 24) return t('knowledge.detail.hoursAgo', { defaultValue: '{{n}} 小时前', n: diffH });
    const diffD = Math.floor(diffH / 24);
    return t('knowledge.detail.daysAgo', { defaultValue: '{{n}} 天前', n: diffD });
  }, [base?.updated_at, t]);

  // ─── Error state ────────────────────────────────────────────────────────────
  if (error) {
    return (
      <div className='size-full flex items-center justify-center'>
        <Result
          status='error'
          title={t('knowledge.loadError')}
          subTitle={error}
          extra={<Button onClick={() => navigate('/knowledge')}>{t('knowledge.backToList')}</Button>}
        />
      </div>
    );
  }

  // ─── Render ─────────────────────────────────────────────────────────────────
  return (
    <div
      className={classNames(
        'size-full box-border overflow-y-auto',
        isMobile ? 'px-16px py-14px' : 'px-12px py-24px md:px-40px md:py-32px'
      )}
    >
      <div className='mx-auto flex w-full max-w-1180px box-border flex-col gap-16px'>
        {/* ─── Back link ─────────────────────────────────────────────────────── */}
        <button
          type='button'
          className='knowledge-detail-back-link inline-flex h-24px items-center gap-6px border-0 bg-transparent p-0 font-[inherit] text-12px leading-none text-[var(--color-text-3)] appearance-none cursor-pointer transition-colors hover:text-[rgb(var(--primary-6))] focus-visible:outline-none focus-visible:text-[rgb(var(--primary-6))]'
          onClick={() => navigate('/knowledge')}
        >
          <span className='knowledge-detail-back-icon inline-flex h-14px w-14px items-center justify-center leading-none [&_svg]:block'>
            <Left theme='outline' size='14' />
          </span>
          <span className='leading-none'>{t('knowledge.detail.back', { defaultValue: '返回知识库' })}</span>
        </button>

        {/* ─── Header ────────────────────────────────────────────────────────── */}
        <div className='flex flex-wrap items-start justify-between gap-18px'>
          {/* Left: icon + title + badges + tags */}
          <div className='flex gap-14px items-center'>
            {base && kindConfig && <DetailKindIcon kind={base.kind} config={kindConfig} />}
            <div className='flex flex-col gap-6px'>
              <h1 className='m-0 text-21px font-700 text-[var(--color-text-1)] flex items-center gap-9px'>
                {base?.name ?? '...'}
                {/* Pen icon — edit entry point (actual editing in D5/Settings tab) */}
                <span
                  className='text-12px text-[var(--color-text-3)] cursor-pointer hover:text-[rgb(var(--primary-6))]'
                  onClick={() => setTab('set')}
                  title={t('knowledge.detail.editName', { defaultValue: '编辑名称' })}
                >
                  <EditTwo theme='outline' size='12' />
                </span>
              </h1>
              <div className='flex flex-wrap items-center gap-6px'>
                {/* Kind badge */}
                {kindConfig && (
                  <span
                    className={`inline-flex items-center rounded-6px px-8px py-2px text-10px font-600 border border-solid ${kindConfig.bgClass} ${kindConfig.textClass} ${kindConfig.borderClass}`}
                  >
                    {kindConfig.label}
                  </span>
                )}
                {/* User tags */}
                {base?.tags.map((tagKey) => {
                  const tag = tagMap[tagKey];
                  return (
                    <span
                      key={tagKey}
                      className='inline-flex items-center gap-5px text-11px text-[var(--color-text-2)] bg-[var(--color-fill-2)] border border-solid border-[var(--color-border-3)] rounded-6px px-8px py-2px'
                    >
                      {tag?.color && (
                        <i className='w-6px h-6px rounded-full inline-block' style={{ background: tag.color }} />
                      )}
                      {tag?.label ?? tagKey}
                    </span>
                  );
                })}
                {/* Add tag placeholder (leads to settings tab) */}
                <span
                  className='text-11px text-[var(--color-text-3)] cursor-pointer border border-dashed border-[var(--color-border-3)] rounded-6px px-8px py-2px hover:text-[rgb(var(--primary-6))] hover:border-[rgba(var(--primary-6),0.5)]'
                  onClick={() => setTab('set')}
                >
                  + {t('knowledge.detail.addTag', { defaultValue: '标签' })}
                </span>
              </div>
            </div>
          </div>

          {/* Right: action buttons */}
          <div className='flex items-center gap-8px flex-wrap'>
            <Button
              shape='round'
              icon={<Search theme='outline' size='14' />}
              onClick={() => Message.info(t('knowledge.detail.searchPlaceholder', { defaultValue: '检索功能开发中' }))}
            >
              {t('knowledge.detail.search', { defaultValue: '检索' })}
            </Button>
            <Button
              type='primary'
              shape='round'
              icon={<LinkOne theme='outline' size='14' />}
              onClick={() => setTab('use')}
            >
              {t('knowledge.detail.mountToSession', { defaultValue: '挂载到会话' })}
            </Button>
            <Dropdown
              droplist={
                <Menu>
                  <Menu.Item key='export' onClick={() => setTab('set')}>
                    {t('knowledge.detail.export', { defaultValue: '导出' })}
                  </Menu.Item>
                  <Menu.Item key='openFolder' onClick={() => void handleOpenFolder()}>
                    {t('knowledge.actions.openFolder', { defaultValue: '打开文件夹' })}
                  </Menu.Item>
                  <Menu.Item
                    key='connector'
                    disabled={!FEISHU_KNOWLEDGE_CREATION_ENABLED}
                    className={classNames(!FEISHU_KNOWLEDGE_CREATION_ENABLED && 'cursor-not-allowed opacity-50')}
                    onClick={() => {
                      if (FEISHU_KNOWLEDGE_CREATION_ENABLED) setConnectorVisible(true);
                    }}
                  >
                    <span className='inline-flex items-center gap-6px'>
                      <ApiApp theme='outline' size='14' />
                      {t('knowledge.detail.connector', { defaultValue: '连接器' })}
                    </span>
                  </Menu.Item>
                  <Menu.Item key='delete' className='!text-[rgb(var(--danger-6))]' onClick={() => setTab('set')}>
                    {t('knowledge.detail.delete', { defaultValue: '删除知识库' })}
                  </Menu.Item>
                </Menu>
              }
              position='br'
            >
              <Button shape='round' icon={<More theme='outline' size='14' />} />
            </Dropdown>
          </div>
        </div>

        {/* ─── Meta info row ─────────────────────────────────────────────────── */}
        {base && (
          <div className='flex flex-wrap gap-14px text-12px text-[var(--color-text-3)]'>
            <span>{t('knowledge.detail.fileCount', { defaultValue: '{{n}} 篇文档', n: base.file_count })}</span>
            <span>{formatSize(base.total_size)}</span>
            {/* mount count placeholder — D3 consumers section will provide real data */}
            <span>{t('knowledge.detail.rootPath', { defaultValue: '{{path}}', path: base.root_path })}</span>
            {relativeTime && (
              <span>{t('knowledge.detail.updatedAt', { defaultValue: '更新于 {{time}}', time: relativeTime })}</span>
            )}
          </div>
        )}

        {/* ─── Tabs ──────────────────────────────────────────────────────────── */}
        <Tabs activeTab={activeTab} onChange={(k) => setTab(k)} type='line'>
          {/* Tab: Documents */}
          <Tabs.TabPane key='docs' title={t('knowledge.detail.tabDocs', { defaultValue: '文档' })}>
            {/* ── Document tree + viewer (D2 redesign) ── */}
            <div
              className={classNames(
                'flex w-full gap-18px pt-16px',
                isMobile ? 'flex-col' : 'flex-row',
                'min-h-440px'
              )}
            >
              {/* ─── Left: File tree panel ─── */}
              <div
                className={classNames(
                  'box-border shrink-0 flex flex-col rd-14px border border-solid border-[var(--color-border-2)] bg-[var(--color-fill-1)] p-12px',
                  isMobile ? 'w-full' : 'w-264px'
                )}
              >
                {/* Document actions */}
                <div className='knowledge-doc-actions mb-8px grid grid-cols-3 gap-4px rounded-10px bg-[var(--color-fill-2)] p-3px'>
                  <button
                    type='button'
                    className='knowledge-doc-action inline-flex min-w-0 appearance-none items-center justify-center gap-4px rounded-8px border-none bg-transparent px-6px py-7px font-[inherit] text-11px font-500 text-[var(--color-text-2)] cursor-pointer transition-colors hover:bg-[var(--color-fill-3)] hover:text-[var(--color-text-1)] focus-visible:outline-none focus-visible:bg-[var(--color-fill-3)] focus-visible:text-[var(--color-text-1)]'
                    onClick={() => openNewFileModal()}
                    title={t('knowledge.detail.docs.newFile', { defaultValue: '新建文档' })}
                  >
                    <Plus theme='outline' size='12' className='shrink-0' />
                    <span className='truncate'>{t('knowledge.detail.docs.newFile', { defaultValue: '新建文档' })}</span>
                  </button>
                  <button
                    type='button'
                    className='knowledge-doc-action inline-flex min-w-0 appearance-none items-center justify-center gap-4px rounded-8px border-none bg-transparent px-6px py-7px font-[inherit] text-11px font-500 text-[var(--color-text-2)] cursor-pointer transition-colors hover:bg-[var(--color-fill-3)] hover:text-[var(--color-text-1)] focus-visible:outline-none focus-visible:bg-[var(--color-fill-3)] focus-visible:text-[var(--color-text-1)]'
                    onClick={() => openNewFolderModal()}
                    title={t('knowledge.detail.docs.newFolder', { defaultValue: '新建文件夹' })}
                  >
                    <FolderPlus theme='outline' size='12' className='shrink-0' />
                    <span className='truncate'>{t('knowledge.detail.docs.newFolder', { defaultValue: '文件夹' })}</span>
                  </button>
                  <button
                    type='button'
                    className='knowledge-doc-action inline-flex min-w-0 appearance-none items-center justify-center gap-4px rounded-8px border-none bg-transparent px-6px py-7px font-[inherit] text-11px font-500 text-[var(--color-text-2)] cursor-pointer transition-colors hover:bg-[var(--color-fill-3)] hover:text-[var(--color-text-1)] focus-visible:outline-none focus-visible:bg-[var(--color-fill-3)] focus-visible:text-[var(--color-text-1)]'
                    onClick={() => Message.info(t('knowledge.detail.docs.uploadTodo', { defaultValue: '上传功能开发中' }))}
                    title={t('knowledge.detail.docs.upload', { defaultValue: '上传' })}
                  >
                    <Upload theme='outline' size='12' className='shrink-0' />
                    <span className='truncate'>{t('knowledge.detail.docs.upload', { defaultValue: '上传' })}</span>
                  </button>
                </div>

                {/* Search box */}
                <div className='knowledge-doc-search flex items-center gap-7px rounded-8px bg-[var(--color-fill-2)] border border-solid border-[var(--color-border-3)] px-10px py-7px mb-8px'>
                  <Search theme='outline' size='13' className='text-[var(--color-text-3)] shrink-0' />
                  <input
                    className='border-none bg-transparent outline-none text-[var(--color-text-1)] text-12px w-full placeholder:text-[var(--color-text-3)]'
                    placeholder={t('knowledge.detail.docs.searchPlaceholder', { defaultValue: '搜索文档…' })}
                    value={fileSearch}
                    onChange={(e) => setFileSearch(e.target.value)}
                  />
                </div>

                {/* File tree */}
                <div className='flex-1 overflow-y-auto'>
                  <Spin loading={loading} className='w-full'>
                    {displayedTreeData.length === 0 ? (
                      <Empty
                        description={
                          fileSearch.trim()
                            ? t('knowledge.detail.docs.noSearchResults', { defaultValue: '无匹配文件' })
                            : t('knowledge.noFiles')
                        }
                        className='mt-32px'
                      />
                    ) : (
                      <Tree
                        className='knowledge-doc-tree text-13px [&_.arco-tree-node]:w-full [&_.arco-tree-node-title-wrapper]:flex [&_.arco-tree-node-title-wrapper]:w-full [&_.arco-tree-node-title-wrapper]:min-w-0 [&_.arco-tree-node-title-wrapper]:items-center [&_.arco-tree-node-title]:min-w-0 [&_.arco-tree-node-title]:flex-1 [&_.arco-tree-node-title]:!pr-0'
                        showLine
                        actionOnClick={['select', 'expand']}
                        selectedKeys={selectedTreeKey ? [selectedTreeKey] : []}
                        expandedKeys={visibleTreeExpandedKeys}
                        treeData={displayedTreeData}
                        fieldNames={{
                          children: 'children',
                          title: 'name',
                          key: 'rel_path',
                          isLeaf: 'is_file',
                        }}
                        onSelect={(_keys, extra) => {
                          const dataRef = (extra?.node as { props?: { dataRef?: IKnowledgeTreeEntry } } | undefined)
                            ?.props?.dataRef;
                          if (!dataRef) return;
                          setSelectedTreeKey(dataRef.rel_path);
                          if (dataRef.is_file) {
                            setSelectedPath(dataRef.rel_path);
                            setSelectedFolderPath(parentDirOfKnowledgePath(dataRef.rel_path));
                          } else {
                            setSelectedFolderPath(dataRef.rel_path);
                          }
                        }}
                        onExpand={(keys) => {
                          if (!isTreeSearch) setExpandedTreeKeys(keys.map(String));
                        }}
                        loadMore={(treeNode) => {
                          const dataRef = (treeNode.props as { dataRef?: IKnowledgeTreeEntry }).dataRef;
                          if (!dataRef || dataRef.is_file || isTreeSearch) return Promise.resolve();
                          return handleLoadTreeChildren(dataRef).catch((e: unknown) => {
                            Message.error(String(e));
                          });
                        }}
                        renderTitle={(node) => {
                          const item = node.dataRef as IKnowledgeTreeEntry;
                          return (
                            <div className='knowledge-tree-node-row group flex w-full min-w-0 items-center gap-6px pr-1px'>
                              <span className='knowledge-tree-node-main flex min-w-0 flex-1 items-center gap-5px'>
                                {item.is_dir ? (
                                  <FolderOpen theme='outline' size='13' className='shrink-0 text-[var(--color-text-3)]' />
                                ) : (
                                  <FileText theme='outline' size='13' className='shrink-0 text-[var(--color-text-3)]' />
                                )}
                                <span className='knowledge-tree-node-name block min-w-0 truncate' title={item.rel_path}>
                                  {node.title}
                                </span>
                              </span>
                              <span className='knowledge-tree-node-action ml-auto w-24px grid shrink-0 place-items-center opacity-0 transition-opacity duration-150 group-hover:opacity-100 focus-within:opacity-100'>
                                <Dropdown
                                  trigger='click'
                                  droplist={
                                    <Menu onClickMenuItem={(key) => handleTreeNodeMenuClick(String(key), item)}>
                                      {item.is_dir && (
                                        <>
                                          <Menu.Item key='new-file'>
                                            <span className='inline-flex items-center gap-6px'>
                                              <Plus theme='outline' size='13' />
                                              {t('knowledge.detail.docs.newFile', { defaultValue: '新建文档' })}
                                            </span>
                                          </Menu.Item>
                                          <Menu.Item key='new-folder'>
                                            <span className='inline-flex items-center gap-6px'>
                                              <FolderPlus theme='outline' size='13' />
                                              {t('knowledge.detail.docs.newFolder', { defaultValue: '新建文件夹' })}
                                            </span>
                                          </Menu.Item>
                                        </>
                                      )}
                                      <Menu.Item key='rename'>
                                        <span className='inline-flex items-center gap-6px'>
                                          <EditTwo theme='outline' size='13' />
                                          {t('knowledge.actions.rename', { defaultValue: '重命名' })}
                                        </span>
                                      </Menu.Item>
                                      <Menu.Item key='delete' className='!text-[rgb(var(--danger-6))]'>
                                        <span className='inline-flex items-center gap-6px'>
                                          <Delete theme='outline' size='13' />
                                          {t('knowledge.actions.delete', { defaultValue: '删除' })}
                                        </span>
                                      </Menu.Item>
                                    </Menu>
                                  }
                                >
                                  <button
                                    type='button'
                                    className='knowledge-tree-node-more grid h-22px w-22px shrink-0 place-items-center rounded-6px border-0 bg-transparent p-0 text-[var(--color-text-3)] cursor-pointer hover:bg-[var(--color-fill-2)] hover:text-[var(--color-text-1)] focus-visible:outline-none focus-visible:bg-[var(--color-fill-2)]'
                                    onMouseDown={(e) => e.stopPropagation()}
                                    onClick={(e) => e.stopPropagation()}
                                    title={t('common.more', { defaultValue: '更多' })}
                                    aria-label={t('common.more', { defaultValue: '更多' })}
                                  >
                                    <More theme='outline' size='13' />
                                  </button>
                                </Dropdown>
                              </span>
                            </div>
                          );
                        }}
                      />
                    )}
                  </Spin>
                </div>
              </div>

              {/* ─── Right: Viewer / editor panel ─── */}
              <div className='box-border min-w-0 flex-1 flex flex-col rd-14px border border-solid border-[var(--color-border-2)] bg-[var(--color-fill-1)] overflow-hidden'>
                {selectedPath == null ? (
                  <div className='flex-1 grid place-items-center'>
                    <Empty description={t('knowledge.selectFile')} />
                  </div>
                ) : (
                  <>
                    {/* Toolbar: breadcrumb + toggle + save */}
                    <div className='flex items-center justify-between gap-8px px-16px py-11px border-b border-solid border-[var(--color-border-2)]'>
                      {/* Breadcrumb */}
                      <div className='text-12px text-[var(--color-text-3)] truncate'>
                        {breadcrumbSegments.map((seg, idx) => (
                          <React.Fragment key={idx}>
                            {idx > 0 && <span className='mx-4px'>/</span>}
                            {idx === breadcrumbSegments.length - 1 ? (
                              <span className='font-500 text-[var(--color-text-2)]'>{seg}</span>
                            ) : (
                              <span>{seg}</span>
                            )}
                          </React.Fragment>
                        ))}
                      </div>
                      {/* Right side controls */}
                      <div className='flex items-center gap-10px shrink-0'>
                        {/* Preview / Edit segmented toggle */}
                        <div className='inline-flex bg-[var(--color-fill-2)] border border-solid border-[var(--color-border-3)] rd-8px p-2px'>
                          <button
                            className={classNames(
                              'border-none bg-transparent text-12px px-12px py-5px rd-6px cursor-pointer font-inherit',
                              !editMode
                                ? '!bg-primary-1 !text-primary-6 font-600'
                                : 'text-[var(--color-text-3)] hover:text-[var(--color-text-2)]'
                            )}
                            onClick={() => setEditMode(false)}
                          >
                            {t('knowledge.detail.docs.preview', { defaultValue: '预览' })}
                          </button>
                          <button
                            className={classNames(
                              'border-none bg-transparent text-12px px-12px py-5px rd-6px cursor-pointer font-inherit',
                              editMode
                                ? '!bg-primary-1 !text-primary-6 font-600'
                                : 'text-[var(--color-text-3)] hover:text-[var(--color-text-2)]'
                            )}
                            onClick={startEdit}
                          >
                            {t('knowledge.detail.docs.edit', { defaultValue: '编辑' })}
                          </button>
                        </div>
                        {/* Save button (visible when editing) */}
                        {editMode && (
                          <Button size='small' type='primary' loading={saving} onClick={() => void handleSave()}>
                            {t('knowledge.actions.save')}
                          </Button>
                        )}
                      </div>
                    </div>
                    {/* Content area */}
                    <div className='flex-1 overflow-y-auto p-20px'>
                      <Spin loading={fileLoading} className='w-full'>
                        {editMode ? (
                          <Input.TextArea
                            value={draft}
                            onChange={setDraft}
                            autoSize={{ minRows: 18, maxRows: 40 }}
                            className='font-mono text-13px'
                          />
                        ) : (
                          <Markdown>{content}</Markdown>
                        )}
                      </Spin>
                    </div>
                  </>
                )}
              </div>
            </div>
            {/* AI actions row (autogen / refresh source) */}
            <div className='flex flex-wrap items-center gap-8px mt-12px'>
              <Button
                shape='round'
                size='small'
                loading={autogenLoading}
                icon={<MagicHat theme='outline' size='14' />}
                onClick={() => void handleAutogen()}
              >
                {t('knowledge.actions.aiGenerateOverview')}
              </Button>
              <KnowledgeModelSelector size='small' choice={modelChoice} onChange={(c) => void setModelChoice(c)} />
              {source && (
                <Button
                  shape='round'
                  size='small'
                  icon={<Refresh theme='outline' size='12' />}
                  loading={refreshingSource}
                  onClick={() => void handleRefreshSource()}
                >
                  {t('knowledge.source.refresh')}
                </Button>
              )}
            </div>
          </Tabs.TabPane>

          {/* Tab: Inbox / Pending Review */}
          <Tabs.TabPane
            key='inbox'
            title={
              <span className='flex items-center gap-6px'>
                {t('knowledge.detail.tabInbox', { defaultValue: '待审' })}
                {pendingCount > 0 && <Badge count={pendingCount} />}
              </span>
            }
          >
            <div className='pt-16px'>
              {base && (inboxLoading || inboxItems.length > 0) ? (
                <InboxReviewPanel baseId={base.id} items={inboxItems} loading={inboxLoading} onChanged={handleInboxChanged} />
              ) : (
                <Empty description={t('knowledge.detail.inboxEmpty', { defaultValue: '暂无待审内容' })} />
              )}
            </div>
          </Tabs.TabPane>

          {/* Tab: Mount & Usage */}
          <Tabs.TabPane key='use' title={t('knowledge.detail.tabUse', { defaultValue: '挂载与使用' })}>
            <div className='flex flex-col gap-16px pt-16px'>
              {/* ── Three-step tutorial hero cards ── */}
              <div className={classNames('grid gap-12px', isMobile ? 'grid-cols-1' : 'grid-cols-3')}>
                {/* Step 1 */}
                <div className='box-border rd-12px border border-solid border-[var(--color-border-2)] bg-[var(--color-fill-1)] p-16px'>
                  <div className='w-26px h-26px rd-8px grid place-items-center mb-10px text-13px font-700 bg-[rgba(var(--primary-6),0.1)] text-[rgb(var(--primary-5))] border border-solid border-[rgba(var(--primary-6),0.4)]'>
                    1
                  </div>
                  <b className='block text-13px text-[var(--color-text-1)] mb-5px'>
                    {t('knowledge.detail.use.step1Title', { defaultValue: '挂载到一个会话' })}
                  </b>
                  <p className='m-0 text-12px leading-relaxed text-[var(--color-text-3)]'>
                    {t('knowledge.detail.use.step1Desc', {
                      defaultValue: '把知识库挂到会话 / 终端 / 数字伙伴上，它就成为该处模型的扩展知识。一个库可被多处复用。',
                    })}
                  </p>
                </div>
                {/* Step 2 */}
                <div className='box-border rd-12px border border-solid border-[var(--color-border-2)] bg-[var(--color-fill-1)] p-16px'>
                  <div className='w-26px h-26px rd-8px grid place-items-center mb-10px text-13px font-700 bg-[rgba(var(--primary-6),0.1)] text-[rgb(var(--primary-5))] border border-solid border-[rgba(var(--primary-6),0.4)]'>
                    2
                  </div>
                  <b className='block text-13px text-[var(--color-text-1)] mb-5px'>
                    {t('knowledge.detail.use.step2Title', { defaultValue: '模型自动检索' })}
                  </b>
                  <p className='m-0 text-12px leading-relaxed text-[var(--color-text-3)]'>
                    {t('knowledge.detail.use.step2Desc', {
                      defaultValue: '模型会在 .nomi/knowledge/ 下按需检索，命中的内容用于回答——原文不塞进上下文，省 token。',
                    })}
                  </p>
                </div>
                {/* Step 3 */}
                <div className='box-border rd-12px border border-solid border-[var(--color-border-2)] bg-[var(--color-fill-1)] p-16px'>
                  <div className='w-26px h-26px rd-8px grid place-items-center mb-10px text-13px font-700 bg-[rgba(var(--primary-6),0.1)] text-[rgb(var(--primary-5))] border border-solid border-[rgba(var(--primary-6),0.4)]'>
                    3
                  </div>
                  <b className='block text-13px text-[var(--color-text-1)] mb-5px'>
                    {t('knowledge.detail.use.step3Title', { defaultValue: '（可选）回血沉淀' })}
                  </b>
                  <p className='m-0 text-12px leading-relaxed text-[var(--color-text-3)]'>
                    {t('knowledge.detail.use.step3Desc', {
                      defaultValue: '开启回血后，会话里新学到的知识可暂存到「待审」由你确认，知识库越用越厚。',
                    })}
                  </p>
                </div>
              </div>

              {/* ── Consumers section: who is mounting this base ── */}
              <div className='box-border rd-12px border border-solid border-[var(--color-border-2)] bg-[var(--color-fill-1)] p-16px'>
                <div className='knowledge-mount-heading mb-12px flex flex-wrap items-start gap-x-10px gap-y-4px'>
                  <span className='shrink-0 text-13px font-700 leading-20px text-[var(--color-text-1)]'>
                    {t('knowledge.detail.use.mountedTitle', { defaultValue: '已挂载' })}
                  </span>
                  <div className='knowledge-mount-hint flex min-w-0 flex-1 items-start gap-6px pt-1px'>
                    <LinkOne theme='outline' size='13' className='text-[var(--color-text-4)] shrink-0 mt-2px' />
                    <span className='min-w-0 text-12px text-[var(--color-text-3)] leading-relaxed'>
                      {t('knowledge.detail.use.mountHint', {
                        defaultValue: '挂载操作在会话侧的「挂载知识库」控件中进行——打开任意会话 / 终端 / 数字伙伴，点击知识库按钮即可将本库挂载上去。',
                      })}
                    </span>
                  </div>
                </div>
                {base ? <KnowledgeConsumersSection baseId={base.id} /> : null}
              </div>

              {/* ── Writeback explanation (honest: per-binding, no fake global toggle) ── */}
              <div className='box-border rd-12px border border-solid border-[var(--color-border-2)] bg-[var(--color-fill-1)] p-16px'>
                <div className='text-13px font-700 text-[var(--color-text-1)] mb-10px'>
                  {t('knowledge.detail.use.writebackTitle', { defaultValue: '回血（让会话把新知识写回本库）' })}
                </div>
                <div className='text-12px text-[var(--color-text-2)] leading-relaxed space-y-6px'>
                  <p className='m-0'>
                    {t('knowledge.detail.use.writebackDesc', {
                      defaultValue: '回血模式在每个会话的「挂载知识库」控件里按工作区设置——不是全局统一开关。每个挂载可独立选择：',
                    })}
                  </p>
                  <ul className='m-0 pl-18px text-[var(--color-text-3)]'>
                    <li>
                      <span className='text-[var(--color-text-2)] font-500'>
                        {t('knowledge.detail.use.writebackOff', { defaultValue: '关闭' })}
                      </span>
                      {' — '}
                      {t('knowledge.detail.use.writebackOffDesc', { defaultValue: '纯只读，不回写' })}
                    </li>
                    <li>
                      <span className='text-[var(--color-text-2)] font-500'>
                        {t('knowledge.detail.use.writebackStaged', { defaultValue: '暂存审阅' })}
                      </span>
                      {' — '}
                      {t('knowledge.detail.use.writebackStagedDesc', { defaultValue: '新知识先进「待审」，你确认后才并入（推荐）' })}
                    </li>
                    <li>
                      <span className='text-[var(--color-text-2)] font-500'>
                        {t('knowledge.detail.use.writebackDirect', { defaultValue: '直接写入' })}
                      </span>
                      {' — '}
                      {t('knowledge.detail.use.writebackDirectDesc', { defaultValue: '模型直接改库，适合个人/数字伙伴' })}
                    </li>
                  </ul>
                </div>
              </div>

              {/* ── Terminal CLI registration entry ── */}
              <div className='box-border rd-12px border border-solid border-[var(--color-border-2)] bg-[var(--color-fill-1)] p-16px'>
                <div className='text-13px font-700 text-[var(--color-text-1)] mb-8px'>
                  {t('knowledge.detail.use.cliTitle', { defaultValue: '终端 CLI 接入' })}
                </div>
                <p className='m-0 text-12px text-[var(--color-text-3)] leading-relaxed mb-12px'>
                  {t('knowledge.detail.use.cliDesc', {
                    defaultValue: '给 claude / codex / gemini 一键注入只读的 knowledge_search 工具，让命令行里的 Agent 也能查这个库。请在终端页面使用「接入知识库」按钮完成注册。',
                  })}
                </p>
                <Button
                  size='small'
                  icon={<LinkCloud theme='outline' size='14' />}
                  onClick={() => navigate('/terminal')}
                >
                  {t('knowledge.detail.use.goTerminal', { defaultValue: '前往终端注册' })}
                </Button>
              </div>
            </div>
          </Tabs.TabPane>

          {/* Tab: Settings (D5) */}
          <Tabs.TabPane
            key='set'
            title={
              <span className='flex items-center gap-6px'>
                <SettingTwo theme='outline' size='13' />
                {t('knowledge.detail.tabSettings', { defaultValue: '设置' })}
              </span>
            }
          >
            <div className='pt-16px'>
              {base && (
                <SettingsTab
                  base={base}
                  allTags={allTags}
                  createTag={createTag}
                  onRefresh={refresh}
                  onConnectorOpen={handleConnectorOpen}
                />
              )}
            </div>
          </Tabs.TabPane>
        </Tabs>
      </div>

      {/* ─── Connector drawer (preserved) ──────────────────────────────────── */}
      {base ? (
        <KnowledgeConnectorDrawer
          visible={connectorVisible}
          onClose={() => setConnectorVisible(false)}
          base={base}
          onChanged={() => void refresh()}
        />
      ) : null}

      {/* ─── New file modal (preserved) ────────────────────────────────────── */}
      <Modal
        title={t('knowledge.newFile')}
        visible={newFileVisible}
        onOk={() => void handleCreateFile()}
        onCancel={() => setNewFileVisible(false)}
        autoFocus={false}
      >
        <Input
          placeholder={t('knowledge.newFilePlaceholder')}
          value={newFilePath}
          onChange={setNewFilePath}
          onPressEnter={() => void handleCreateFile()}
        />
      </Modal>

      <Modal
        title={t('knowledge.newFolder', { defaultValue: '新建文件夹' })}
        visible={newFolderVisible}
        onOk={() => void handleCreateFolder()}
        onCancel={() => setNewFolderVisible(false)}
        autoFocus={false}
      >
        <Input
          placeholder={t('knowledge.newFolderPlaceholder', { defaultValue: '输入文件夹名或相对路径，例如 raw 或 raw/tutorials' })}
          value={newFolderPath}
          onChange={setNewFolderPath}
          onPressEnter={() => void handleCreateFolder()}
        />
      </Modal>

      <Modal
        title={t('knowledge.renameTitle', { defaultValue: '重命名' })}
        visible={renameVisible}
        onOk={() => void handleRenameTreeEntry()}
        onCancel={() => {
          setRenameVisible(false);
          setRenameTarget(null);
          setRenameName('');
        }}
        autoFocus={false}
      >
        <Input
          placeholder={t('knowledge.renamePlaceholder', { defaultValue: '输入新的名称' })}
          value={renameName}
          onChange={setRenameName}
          onPressEnter={() => void handleRenameTreeEntry()}
        />
      </Modal>
    </div>
  );
};

export default KnowledgeDetailPage;
