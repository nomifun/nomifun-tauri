/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import type { ToolReceiptDetailRow } from './components/toolGroupSummaryModel';

type ThinkingReceiptContent = {
  content?: string | null;
  subject?: string | null;
  duration?: number | null;
  status?: string | null;
};

type ThinkingReceiptCopy = {
  completedFallback?: string;
  runningFallback: string;
  waitingFallback: string;
  previewMaxLength?: number;
};

const compactText = (value: unknown): string => {
  if (typeof value !== 'string') return '';
  return value.replace(/\s+/g, ' ').trim();
};

const truncatePreview = (value: string, maxLength: number): string => {
  if (value.length <= maxLength) return value;
  return `${value.slice(0, maxLength).trimEnd()}...`;
};

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

export const shouldShowThinkingReceiptDetail = (content: ThinkingReceiptContent): boolean =>
  Boolean(compactText(content.content));

export const buildThinkingReceiptDisplay = (
  content: ThinkingReceiptContent,
  copy: ThinkingReceiptCopy
): { label: string; detail: string } => {
  const detail = compactText(content.content);
  const subject = compactText(content.subject);

  if (subject) {
    return {
      label: subject,
      detail,
    };
  }

  if (detail) {
    return {
      label: truncatePreview(detail, copy.previewMaxLength ?? 84),
      detail,
    };
  }

  return {
    label:
      content.status === 'done'
        ? compactText(copy.completedFallback) || compactText(copy.runningFallback)
        : compactText(copy.waitingFallback) || compactText(copy.runningFallback),
    detail: '',
  };
};
