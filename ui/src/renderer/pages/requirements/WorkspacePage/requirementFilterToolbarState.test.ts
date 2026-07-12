import { describe, expect, test } from 'bun:test';

import {
  isRequirementSearchExpanded,
  shouldCollapseRequirementSearch,
} from './requirementFilterToolbarState';

describe('requirement filter toolbar search state', () => {
  test('stays collapsed when inactive and empty', () => {
    expect(isRequirementSearchExpanded(false, '')).toBe(false);
  });

  test('expands when activated or when a query is present', () => {
    expect(isRequirementSearchExpanded(true, '')).toBe(true);
    expect(isRequirementSearchExpanded(false, 'agent')).toBe(true);
  });

  test('only collapses on blur or Escape when the query is empty', () => {
    expect(shouldCollapseRequirementSearch('')).toBe(true);
    expect(shouldCollapseRequirementSearch('   ')).toBe(true);
    expect(shouldCollapseRequirementSearch('agent')).toBe(false);
  });
});
