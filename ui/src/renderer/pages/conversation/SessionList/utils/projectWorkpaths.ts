/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import { DEFAULT_WORKPATH_KEY, workpathKey } from './workpathKey';

export const PROJECT_WORKPATHS_STORAGE_KEY = 'nomifun:session-list-project-workpaths';
const PROJECT_WORKPATHS_CHANGED_EVENT = 'nomifun:session-list-project-workpaths-changed';

const readProjectWorkpaths = (): string[] => {
  if (typeof window === 'undefined') return [];
  try {
    const parsed = JSON.parse(localStorage.getItem(PROJECT_WORKPATHS_STORAGE_KEY) ?? '[]') as unknown;
    return Array.isArray(parsed) ? parsed.filter((item): item is string => typeof item === 'string') : [];
  } catch {
    return [];
  }
};

export const getProjectWorkpaths = (): string[] => {
  const unique = new Set<string>();
  for (const path of readProjectWorkpaths()) {
    const key = workpathKey(path);
    if (key !== DEFAULT_WORKPATH_KEY) unique.add(key);
  }
  return [...unique];
};

export const addProjectWorkpath = (path: string): void => {
  const key = workpathKey(path);
  if (key === DEFAULT_WORKPATH_KEY || typeof window === 'undefined') return;

  try {
    const prev = getProjectWorkpaths();
    const next = [key, ...prev.filter((item) => item !== key)];
    localStorage.setItem(PROJECT_WORKPATHS_STORAGE_KEY, JSON.stringify(next));
    window.dispatchEvent(new Event(PROJECT_WORKPATHS_CHANGED_EVENT));
  } catch {
    // ignore storage errors (quota / privacy mode)
  }
};

export const removeProjectWorkpath = (path: string): void => {
  const key = workpathKey(path);
  if (key === DEFAULT_WORKPATH_KEY || typeof window === 'undefined') return;

  try {
    const next = getProjectWorkpaths().filter((item) => item !== key);
    localStorage.setItem(PROJECT_WORKPATHS_STORAGE_KEY, JSON.stringify(next));
    window.dispatchEvent(new Event(PROJECT_WORKPATHS_CHANGED_EVENT));
  } catch {
    // ignore storage errors (quota / privacy mode)
  }
};

export const subscribeProjectWorkpaths = (callback: () => void): (() => void) => {
  if (typeof window === 'undefined') return () => {};

  const handleStorage = (event: StorageEvent) => {
    if (event.key === PROJECT_WORKPATHS_STORAGE_KEY) callback();
  };

  window.addEventListener(PROJECT_WORKPATHS_CHANGED_EVENT, callback);
  window.addEventListener('storage', handleStorage);
  return () => {
    window.removeEventListener(PROJECT_WORKPATHS_CHANGED_EVENT, callback);
    window.removeEventListener('storage', handleStorage);
  };
};
