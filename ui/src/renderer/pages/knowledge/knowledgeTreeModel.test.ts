import { describe, expect, test } from 'bun:test';
import type { IKnowledgeFileEntry, IKnowledgeTreeEntry } from '@/common/adapter/ipcBridge';
import {
  buildKnowledgeSearchTree,
  isKnowledgePathWithin,
  knowledgeFolderPathChain,
  mergeKnowledgeTreeChildren,
  preserveKnowledgeTreeChildren,
  replaceKnowledgePathPrefix,
} from './KnowledgeDetailPage/treeModel';

const file = (rel_path: string): IKnowledgeFileEntry => ({
  rel_path,
  size: rel_path.length,
  modified_at: null,
});

const node = (name: string, rel_path: string, is_dir: boolean): IKnowledgeTreeEntry => ({
  name,
  rel_path,
  is_dir,
  is_file: !is_dir,
  modified_at: null,
  ...(is_dir ? {} : { size: rel_path.length }),
});

describe('knowledge detail tree model', () => {
  test('builds a search tree that keeps ancestor folders for matched documents', () => {
    const tree = buildKnowledgeSearchTree(
      [
        file('README.md'),
        file('raw/python3-type-conversion.md'),
        file('raw/string.md'),
        file('tutorials/overview.md'),
      ],
      'python3'
    );

    expect(tree).toEqual([
      {
        name: 'raw',
        rel_path: 'raw',
        is_dir: true,
        is_file: false,
        modified_at: null,
        children: [
          {
            name: 'python3-type-conversion.md',
            rel_path: 'raw/python3-type-conversion.md',
            is_dir: false,
            is_file: true,
            size: 'raw/python3-type-conversion.md'.length,
            modified_at: null,
          },
        ],
      },
    ]);
  });

  test('merges lazy-loaded children into the matching directory only', () => {
    const root = [node('raw', 'raw', true), node('README.md', 'README.md', false)];
    const merged = mergeKnowledgeTreeChildren(root, 'raw', [
      node('python3-type-conversion.md', 'raw/python3-type-conversion.md', false),
    ]);

    expect(merged[0].children?.map((child) => child.rel_path)).toEqual(['raw/python3-type-conversion.md']);
    expect(merged[1].children).toBeUndefined();
  });

  test('preserves already loaded folder children when the root tree refreshes', () => {
    const previous = [
      {
        ...node('raw', 'raw', true),
        children: [node('python3-type-conversion.md', 'raw/python3-type-conversion.md', false)],
      },
    ];
    const refreshedRoot = [node('raw', 'raw', true), node('README.md', 'README.md', false)];

    const preserved = preserveKnowledgeTreeChildren(refreshedRoot, previous);

    expect(preserved[0].children?.map((child) => child.rel_path)).toEqual(['raw/python3-type-conversion.md']);
    expect(preserved[1].children).toBeUndefined();
  });

  test('builds a folder path chain for branch refresh and expansion', () => {
    expect(knowledgeFolderPathChain('raw/tutorials/deep')).toEqual(['raw', 'raw/tutorials', 'raw/tutorials/deep']);
    expect(knowledgeFolderPathChain('/raw//tutorials/')).toEqual(['raw', 'raw/tutorials']);
    expect(knowledgeFolderPathChain('')).toEqual([]);
  });

  test('detects and rewrites paths inside a renamed folder', () => {
    expect(isKnowledgePathWithin('raw/tutorials/topic.md', 'raw/tutorials')).toBe(true);
    expect(isKnowledgePathWithin('raw/tutorials', 'raw/tutorials')).toBe(true);
    expect(isKnowledgePathWithin('raw/tutorials-old/topic.md', 'raw/tutorials')).toBe(false);
    expect(replaceKnowledgePathPrefix('raw/tutorials/topic.md', 'raw/tutorials', 'wiki/tutorials')).toBe('wiki/tutorials/topic.md');
    expect(replaceKnowledgePathPrefix('raw/tutorials', 'raw/tutorials', 'wiki/tutorials')).toBe('wiki/tutorials');
    expect(replaceKnowledgePathPrefix(null, 'raw/tutorials', 'wiki/tutorials')).toBeNull();
  });
});
