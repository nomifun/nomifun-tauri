import { ipcBridge } from '@/common';
import type {
  PresetTag,
  PresetTagDimension,
  CreatePresetTagRequest,
} from '@/common/types/agent/presetTypes';
import { useCallback, useEffect, useMemo, useState } from 'react';

/** Loads the merged tag vocabulary and exposes CRUD + per-dimension views. */
export const usePresetTags = () => {
  const [tags, setTags] = useState<PresetTag[]>([]);
  const [loading, setLoading] = useState(false);

  const loadTags = useCallback(async () => {
    setLoading(true);
    try {
      setTags(await ipcBridge.presetTags.list.invoke());
    } catch (error) {
      console.error('Failed to load preset tags:', error);
      setTags([]);
    } finally {
      setLoading(false);
    }
  }, []);

  useEffect(() => {
    void loadTags();
  }, [loadTags]);

  const audienceTags = useMemo(
    () => tags.filter((t) => t.dimension === 'audience').sort((a, b) => a.sort_order - b.sort_order),
    [tags]
  );
  const scenarioTags = useMemo(
    () => tags.filter((t) => t.dimension === 'scenario').sort((a, b) => a.sort_order - b.sort_order),
    [tags]
  );

  /** UUIDv7 business ID → PresetTag, for preset wire references. */
  const tagById = useMemo(() => new Map(tags.map((t) => [t.preset_tag_id, t])), [tags]);
  /** Readable catalog key → PresetTag, retained for skill-tag side stores. */
  const tagByKey = useMemo(() => new Map(tags.map((t) => [t.key, t])), [tags]);

  const createTag = useCallback(
    async (req: CreatePresetTagRequest) => {
      const created = await ipcBridge.presetTags.create.invoke(req);
      await loadTags();
      return created;
    },
    [loadTags]
  );

  const renameTag = useCallback(
    async (preset_tag_id: PresetTag['preset_tag_id'], label: string) => {
      await ipcBridge.presetTags.update.invoke({ preset_tag_id, label });
      await loadTags();
    },
    [loadTags]
  );

  const deleteTag = useCallback(
    async (preset_tag_id: PresetTag['preset_tag_id']) => {
      await ipcBridge.presetTags.delete.invoke({ preset_tag_id });
      await loadTags();
    },
    [loadTags]
  );

  return {
    tags,
    audienceTags,
    scenarioTags,
    tagById,
    tagByKey,
    loading,
    loadTags,
    createTag,
    renameTag,
    deleteTag,
  };
};

export type TagDimension = PresetTagDimension;
