import type { IProvider } from '@/common/config/storage';

const moveItem = <T>(items: T[], fromIndex: number, toIndex: number): T[] => {
  const next = items.slice();
  const [item] = next.splice(fromIndex, 1);
  next.splice(toIndex, 0, item);
  return next;
};

export const reorderById = <T extends { id: string }>(items: T[], activeId: string, overId: string): T[] => {
  const oldIndex = items.findIndex((item) => item.id === activeId);
  const newIndex = items.findIndex((item) => item.id === overId);
  if (oldIndex < 0 || newIndex < 0 || oldIndex === newIndex) return items;
  return moveItem(items, oldIndex, newIndex);
};

export const reorderStrings = (items: string[], activeId: string, overId: string): string[] => {
  const oldIndex = items.indexOf(activeId);
  const newIndex = items.indexOf(overId);
  if (oldIndex < 0 || newIndex < 0 || oldIndex === newIndex) return items;
  return moveItem(items, oldIndex, newIndex);
};

export const withDenseSortOrder = (providers: IProvider[]): IProvider[] =>
  providers.map((provider, index) => ({
    ...provider,
    sort_order: index,
  }));
