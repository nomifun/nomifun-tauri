/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import type { ConversationId, PreviewSnapshotId } from '@/common/types/ids';

export type { PreviewSnapshotId };

export type PreviewContentType =
  | 'markdown'
  | 'diff'
  | 'code'
  | 'html'
  | 'pdf'
  | 'ppt'
  | 'word'
  | 'excel'
  | 'image'
  | 'url';

export interface PreviewHistoryTarget {
  contentType: PreviewContentType;
  file_path?: string;
  workspace?: string;
  file_name?: string;
  title?: string;
  language?: string;
  conversation_id?: ConversationId;
}

export interface PreviewSnapshotInfo {
  snapshot_id: PreviewSnapshotId;
  label: string;
  created_at: number;
  size: number;
  content_type: PreviewContentType;
  file_name?: string;
  file_path?: string;
}

export interface PreviewUrlResponse {
  url: string;
  capability?: string;
  error?: string;
}

export interface RemoteImageFetchRequest {
  url: string;
}
