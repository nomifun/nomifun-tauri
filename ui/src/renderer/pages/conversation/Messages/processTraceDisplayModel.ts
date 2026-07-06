/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import type { ToolReceiptDetailRow } from './components/toolGroupSummaryModel';

export const isFileReceiptRow = (row: ToolReceiptDetailRow): boolean =>
  (row.action === 'read_files' || row.action === 'edit_files') && Boolean(row.target);

export const shouldShowFileListDetail = (rows: ToolReceiptDetailRow[]): boolean =>
  rows.filter(isFileReceiptRow).length > 1;

export const shouldShowToolRowDetail = (
  row: ToolReceiptDetailRow,
  options: { fileRowCount?: number } = {}
): boolean => {
  if (row.action === 'run_commands') return true;

  if (isFileReceiptRow(row)) {
    const hasErrorDetail = (row.state === 'failed' || row.state === 'canceled') && Boolean(row.output || row.truncated);
    return (options.fileRowCount ?? 1) > 1 || hasErrorDetail;
  }

  return Boolean(row.input || row.output || row.truncated);
};
