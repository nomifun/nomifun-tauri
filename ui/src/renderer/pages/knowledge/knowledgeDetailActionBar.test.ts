import { describe, expect, test } from 'bun:test';
import { readFileSync } from 'node:fs';

const detailSource = readFileSync(new URL('./KnowledgeDetailPage/index.tsx', import.meta.url), 'utf8');

describe('Knowledge detail document action bar', () => {
  test('keeps the back link icon and label vertically centered as one row', () => {
    expect(detailSource.includes('knowledge-detail-back-link')).toBe(true);
    expect(detailSource.includes('knowledge-detail-back-icon')).toBe(true);
    expect(detailSource.includes('[&_svg]:block')).toBe(true);
    expect(detailSource.includes("<Left theme='outline' size='14' />\n          <span>")).toBe(false);
  });

  test('uses a soft borderless action bar for new and upload actions', () => {
    expect(detailSource.includes('knowledge-doc-actions')).toBe(true);
    expect(detailSource.includes('knowledge-doc-action')).toBe(true);
    expect(detailSource.includes('Bottom actions: new + upload */}\n                <div className=\'flex gap-7px mt-8px border-t')).toBe(false);
    expect(detailSource.includes('border-none bg-transparent')).toBe(true);
  });

  test('places document actions above document search and includes folder creation', () => {
    const actionsIndex = detailSource.indexOf('knowledge-doc-actions');
    const searchIndex = detailSource.indexOf('knowledge-doc-search');
    expect(actionsIndex).toBeGreaterThan(-1);
    expect(searchIndex).toBeGreaterThan(-1);
    expect(actionsIndex).toBeLessThan(searchIndex);
    expect(detailSource.includes('openNewFolderModal')).toBe(true);
    expect(detailSource.includes('FolderPlus')).toBe(true);
  });

  test('uses compact per-node menus instead of inline delete text in the document tree', () => {
    expect(detailSource.includes('knowledge-tree-node-row')).toBe(true);
    expect(detailSource.includes('knowledge-tree-node-name')).toBe(true);
    expect(detailSource.includes('knowledge-tree-node-action')).toBe(true);
    expect(detailSource.includes('knowledge-tree-node-more')).toBe(true);
    expect(detailSource.includes('handleTreeNodeMenuClick')).toBe(true);
    expect(detailSource.includes("key='new-file'")).toBe(true);
    expect(detailSource.includes("key='new-folder'")).toBe(true);
    expect(detailSource.includes("key='rename'")).toBe(true);
    expect(detailSource.includes("key='delete'")).toBe(true);
    expect(detailSource.includes('deleteFolderWarning')).toBe(true);
    expect(detailSource.includes("className='!hidden group-hover:!inline-flex shrink-0'")).toBe(false);
  });

  test('right-aligns tree row actions and reveals them only for the active row', () => {
    expect(detailSource.includes('knowledge-doc-tree')).toBe(true);
    expect(detailSource.includes('[&_.arco-tree-node-title-wrapper]:flex')).toBe(true);
    expect(detailSource.includes('[&_.arco-tree-node-title]:flex-1')).toBe(true);
    expect(detailSource.includes('knowledge-tree-node-row group flex w-full')).toBe(true);
    expect(detailSource.includes('knowledge-tree-node-action ml-auto w-24px')).toBe(true);
    expect(detailSource.includes('opacity-0')).toBe(true);
    expect(detailSource.includes('group-hover:opacity-100')).toBe(true);
    expect(detailSource.includes('focus-within:opacity-100')).toBe(true);
    expect(detailSource.includes("aria-label={t('common.more'")).toBe(true);
  });

  test('disables connector entries while Feishu knowledge creation is disabled', () => {
    expect(detailSource.includes('FEISHU_KNOWLEDGE_CREATION_ENABLED')).toBe(true);
    expect(detailSource.includes("disabled={!FEISHU_KNOWLEDGE_CREATION_ENABLED}")).toBe(true);
    expect(detailSource.includes("!FEISHU_KNOWLEDGE_CREATION_ENABLED && 'cursor-not-allowed opacity-50'")).toBe(true);
    expect(detailSource.includes("onClick={() => setConnectorVisible(true)}")).toBe(false);
    expect(detailSource.includes("onConnectorOpen={() => setConnectorVisible(true)}")).toBe(false);
  });
});
