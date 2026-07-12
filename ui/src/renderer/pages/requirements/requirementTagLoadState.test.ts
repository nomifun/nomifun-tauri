import { describe, expect, test } from 'bun:test';
import { initialRequirementTagLoadState, reduceRequirementTagLoadState } from './requirementTagLoadState';

describe('requirement tag load state', () => {
  test('starts in loading state before the mount request begins', () => {
    expect(initialRequirementTagLoadState).toEqual({
      tags: [],
      loading: true,
      error: null,
      activeRequestId: null,
    });
  });

  test('a newer start supersedes the active request without discarding data', () => {
    const current = {
      tags: [{ tag: 'release', done: 1, total: 2 }],
      loading: false,
      error: 'old error',
      activeRequestId: 1,
    };

    expect(reduceRequirementTagLoadState(current, { type: 'start', requestId: 2 })).toEqual({
      tags: current.tags,
      loading: true,
      error: 'old error',
      activeRequestId: 2,
    });
  });

  test('ignores success from a stale request', () => {
    const current = {
      tags: [{ tag: 'current', done: 1, total: 2 }],
      loading: true,
      error: null,
      activeRequestId: 2,
    };

    expect(
      reduceRequirementTagLoadState(current, {
        type: 'success',
        requestId: 1,
        tags: [{ tag: 'stale', done: 2, total: 2 }],
      })
    ).toBe(current);
  });

  test('ignores failure from a stale request', () => {
    const current = {
      tags: [{ tag: 'current', done: 1, total: 2 }],
      loading: true,
      error: null,
      activeRequestId: 2,
    };

    expect(
      reduceRequirementTagLoadState(current, { type: 'failure', requestId: 1, error: 'stale failure' })
    ).toBe(current);
  });

  test('ignores finish from a stale request', () => {
    const current = {
      tags: [{ tag: 'current', done: 1, total: 2 }],
      loading: true,
      error: null,
      activeRequestId: 2,
    };

    expect(reduceRequirementTagLoadState(current, { type: 'finish', requestId: 1 })).toBe(current);
  });

  test('stores tags from the latest request and clears the previous error', () => {
    const tags = [{ tag: 'release', done: 2, total: 2 }];
    const current = {
      ...initialRequirementTagLoadState,
      error: 'network',
      activeRequestId: 2,
    };

    expect(reduceRequirementTagLoadState(current, { type: 'success', requestId: 2, tags })).toEqual({
      tags,
      loading: true,
      error: null,
      activeRequestId: 2,
    });
  });

  test('records failure from the latest request while preserving last-good tags', () => {
    const tags = [{ tag: 'release', done: 1, total: 2 }];
    const current = { tags, loading: true, error: null, activeRequestId: 2 };

    expect(reduceRequirementTagLoadState(current, { type: 'failure', requestId: 2, error: 'offline' })).toEqual({
      tags,
      loading: true,
      error: 'offline',
      activeRequestId: 2,
    });
  });

  test('finishes the latest request without changing data or error', () => {
    const current = { tags: [], loading: true, error: 'offline', activeRequestId: 2 };
    expect(reduceRequirementTagLoadState(current, { type: 'finish', requestId: 2 })).toEqual({
      tags: [],
      loading: false,
      error: 'offline',
      activeRequestId: 2,
    });
  });
});
