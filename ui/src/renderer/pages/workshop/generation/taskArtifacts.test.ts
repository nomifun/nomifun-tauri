import { describe, expect, it } from 'vitest';
import { parseAssetId } from '@/common/types/ids';
import { succeededArtifactIds } from './taskArtifacts';

describe('succeededArtifactIds', () => {
  it('rejects succeeded tasks whose persisted result list is empty or absent', () => {
    expect(succeededArtifactIds({ status: 'succeeded', result_asset_ids: [] })).toBeNull();
    expect(succeededArtifactIds({ status: 'succeeded', result_asset_ids: null })).toBeNull();
    expect(succeededArtifactIds({ status: 'succeeded' })).toBeNull();
  });

  it('returns persisted artifact ids for a genuine success', () => {
    const id = parseAssetId('019b0000-0000-7000-8000-000000000001');
    expect(succeededArtifactIds({ status: 'succeeded', result_asset_ids: [id] })).toEqual([id]);
  });

  it('never treats a non-success terminal state as an artifact success', () => {
    const id = parseAssetId('019b0000-0000-7000-8000-000000000001');
    expect(succeededArtifactIds({ status: 'failed', result_asset_ids: [id] })).toBeNull();
  });
});
