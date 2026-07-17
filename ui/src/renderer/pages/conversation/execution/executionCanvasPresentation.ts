/**
 * Turn persisted task prose into a bounded canvas summary. Full output remains
 * available in the step inspector; the graph only needs a clean quick read.
 */
export function summarizeExecutionText(value: string | null | undefined, maxLength = 240): string | undefined {
  if (!value) return undefined;
  const normalized = value
    .replace(/<[^>]+>/g, ' ')
    .replace(/```[\w-]*/g, ' ')
    .replace(/`([^`]+)`/g, '$1')
    .replace(/[*~]{1,3}/g, '')
    .replace(/\[([^\]]+)]\([^)]+\)/g, '$1')
    .replace(/(^|\s)#{1,6}\s+/g, ' ')
    .replace(/(^|\s)[>*+-]\s+/g, ' ')
    .replace(/\|/g, ' · ')
    .replace(/\s+/g, ' ')
    .trim();
  if (!normalized) return undefined;

  const limit = Math.max(2, maxLength);
  if (normalized.length <= limit) return normalized;
  return `${normalized.slice(0, limit - 1).trimEnd()}…`;
}

/** Prefer hover focus, then the projected detail, but never focus a node that
 * disappeared when the planner published a new immutable revision. */
export function resolveExecutionCanvasFocusStepId<T extends string>(
  activeStepIds: ReadonlySet<T>,
  hoveredStepId: T | null | undefined,
  projectedStepId: T | null | undefined,
): T | null {
  if (hoveredStepId && activeStepIds.has(hoveredStepId)) return hoveredStepId;
  if (projectedStepId && activeStepIds.has(projectedStepId)) return projectedStepId;
  return null;
}
