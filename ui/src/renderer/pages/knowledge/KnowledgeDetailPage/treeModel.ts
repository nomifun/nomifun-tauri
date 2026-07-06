import type { IKnowledgeFileEntry, IKnowledgeTreeEntry } from '@/common/adapter/ipcBridge';

function fileName(relPath: string): string {
  return relPath.split('/').filter(Boolean).at(-1) ?? relPath;
}

function dirNode(name: string, relPath: string): IKnowledgeTreeEntry {
  return {
    name,
    rel_path: relPath,
    is_dir: true,
    is_file: false,
    modified_at: null,
    children: [],
  };
}

function fileNode(file: IKnowledgeFileEntry): IKnowledgeTreeEntry {
  return {
    name: fileName(file.rel_path),
    rel_path: file.rel_path,
    is_dir: false,
    is_file: true,
    size: file.size,
    modified_at: file.modified_at,
  };
}

function sortTreeNodes(nodes: IKnowledgeTreeEntry[]): IKnowledgeTreeEntry[] {
  return nodes
    .map((node) => (node.children ? { ...node, children: sortTreeNodes(node.children) } : node))
    .sort((a, b) => {
      if (a.is_dir !== b.is_dir) return a.is_dir ? -1 : 1;
      return a.name.localeCompare(b.name, undefined, { sensitivity: 'base' }) || a.name.localeCompare(b.name);
    });
}

export function buildKnowledgeSearchTree(
  files: IKnowledgeFileEntry[],
  query: string
): IKnowledgeTreeEntry[] {
  const q = query.trim().toLowerCase();
  if (!q) return [];

  const root: IKnowledgeTreeEntry[] = [];
  const dirs = new Map<string, IKnowledgeTreeEntry>();

  for (const file of files) {
    if (!file.rel_path.toLowerCase().includes(q)) continue;

    const segments = file.rel_path.split('/').filter(Boolean);
    let level = root;
    let currentPath = '';
    for (let i = 0; i < segments.length - 1; i += 1) {
      currentPath = currentPath ? `${currentPath}/${segments[i]}` : segments[i];
      let dir = dirs.get(currentPath);
      if (!dir) {
        dir = dirNode(segments[i], currentPath);
        dirs.set(currentPath, dir);
        level.push(dir);
      }
      dir.children ??= [];
      level = dir.children;
    }
    level.push(fileNode(file));
  }

  return sortTreeNodes(root);
}

export function mergeKnowledgeTreeChildren(
  nodes: IKnowledgeTreeEntry[],
  parentRelPath: string,
  children: IKnowledgeTreeEntry[]
): IKnowledgeTreeEntry[] {
  return nodes.map((node) => {
    if (node.rel_path === parentRelPath && node.is_dir) {
      return { ...node, children };
    }
    if (node.children?.length) {
      return { ...node, children: mergeKnowledgeTreeChildren(node.children, parentRelPath, children) };
    }
    return node;
  });
}

export function preserveKnowledgeTreeChildren(
  nextNodes: IKnowledgeTreeEntry[],
  previousNodes: IKnowledgeTreeEntry[]
): IKnowledgeTreeEntry[] {
  const previousByPath = new Map<string, IKnowledgeTreeEntry>();
  const collect = (nodes: IKnowledgeTreeEntry[]) => {
    for (const node of nodes) {
      if (node.is_dir) previousByPath.set(node.rel_path, node);
      if (node.children?.length) collect(node.children);
    }
  };
  collect(previousNodes);

  const preserve = (nodes: IKnowledgeTreeEntry[]): IKnowledgeTreeEntry[] =>
    nodes.map((node) => {
      if (!node.is_dir) return node;
      const previous = previousByPath.get(node.rel_path);
      if (node.children?.length) {
        return { ...node, children: preserve(node.children) };
      }
      if (previous?.children) {
        return { ...node, children: previous.children };
      }
      return node;
    });

  return preserve(nextNodes);
}

export function firstKnowledgeFilePath(nodes: IKnowledgeTreeEntry[]): string | null {
  for (const node of nodes) {
    if (node.is_file) return node.rel_path;
    if (node.children?.length) {
      const found = firstKnowledgeFilePath(node.children);
      if (found) return found;
    }
  }
  return null;
}

export function parentDirOfKnowledgePath(relPath: string | null): string {
  if (!relPath) return '';
  const parts = relPath.split('/').filter(Boolean);
  if (parts.length <= 1) return '';
  return parts.slice(0, -1).join('/');
}

export function knowledgeFolderPathChain(relPath: string): string[] {
  const parts = relPath.split('/').filter(Boolean);
  return parts.map((_, index) => parts.slice(0, index + 1).join('/'));
}

export function isKnowledgePathWithin(path: string | null, folderPath: string): boolean {
  if (!path || !folderPath) return false;
  return path === folderPath || path.startsWith(`${folderPath}/`);
}

export function replaceKnowledgePathPrefix(path: string | null, oldPrefix: string, newPrefix: string): string | null {
  if (!path) return path;
  if (path === oldPrefix) return newPrefix;
  if (path.startsWith(`${oldPrefix}/`)) return `${newPrefix}${path.slice(oldPrefix.length)}`;
  return path;
}
