/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import { describe, expect, test } from 'bun:test';

import {
  addProjectWorkpath,
  getProjectWorkpaths,
  PROJECT_WORKPATHS_STORAGE_KEY,
  removeProjectWorkpath,
} from './projectWorkpaths';

const installStorage = () => {
  const originalLocalStorage = Object.getOwnPropertyDescriptor(globalThis, 'localStorage');
  const originalWindow = Object.getOwnPropertyDescriptor(globalThis, 'window');
  const store = new Map<string, string>();
  const localStorageMock = {
    getItem: (key: string) => store.get(key) ?? null,
    setItem: (key: string, value: string) => store.set(key, value),
    removeItem: (key: string) => store.delete(key),
  };
  const windowMock = {
    localStorage: localStorageMock,
    dispatchEvent: () => true,
    addEventListener: () => {},
    removeEventListener: () => {},
  };
  Object.defineProperty(globalThis, 'localStorage', {
    configurable: true,
    writable: true,
    value: localStorageMock,
  });
  Object.defineProperty(globalThis, 'window', {
    configurable: true,
    writable: true,
    value: windowMock,
  });

  return () => {
    if (originalLocalStorage) Object.defineProperty(globalThis, 'localStorage', originalLocalStorage);
    else Reflect.deleteProperty(globalThis, 'localStorage');
    if (originalWindow) Object.defineProperty(globalThis, 'window', originalWindow);
    else Reflect.deleteProperty(globalThis, 'window');
  };
};

describe('projectWorkpaths', () => {
  test('adds normalized project paths without capping the sidebar project registry', () => {
    const restore = installStorage();
    try {
      for (let i = 0; i < 7; i += 1) {
        addProjectWorkpath(`/Users/a/project-${i}/`);
      }

      expect(getProjectWorkpaths()).toHaveLength(7);
      expect(getProjectWorkpaths()[0]).toBe('/Users/a/project-6');
    } finally {
      restore();
    }
  });

  test('deduplicates by normalized project path', () => {
    const restore = installStorage();
    try {
      addProjectWorkpath('/Users/a/project/');
      addProjectWorkpath('/Users/a/project');

      expect(getProjectWorkpaths()).toEqual(['/Users/a/project']);
    } finally {
      restore();
    }
  });

  test('removes only the normalized project path', () => {
    const restore = installStorage();
    try {
      addProjectWorkpath('/Users/a/keep');
      addProjectWorkpath('/Users/a/remove/');

      removeProjectWorkpath('/Users/a/remove');

      expect(getProjectWorkpaths()).toEqual(['/Users/a/keep']);
    } finally {
      restore();
    }
  });

});
