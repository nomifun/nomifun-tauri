export type ToolGroupDisplayStatus = 'Executing' | 'Success' | 'Error' | 'Canceled' | 'Pending' | 'Confirming';

/**
 * Normalize both the current backend wire vocabulary (`snake_case`) and the
 * legacy renderer vocabulary (PascalCase) into one display/runtime status.
 *
 * ToolCallStatus is serialized by Rust as `running | completed | error`.  Keep
 * the legacy spellings for persisted transcripts created by older builds.
 */
export const normalizeToolGroupStatus = (value: unknown): ToolGroupDisplayStatus => {
  switch (value) {
    case 'completed':
    case 'Success':
      return 'Success';
    case 'error':
    case 'failed':
    case 'Error':
      return 'Error';
    case 'canceled':
    case 'cancelled':
    case 'Canceled':
      return 'Canceled';
    case 'pending':
    case 'Pending':
      return 'Pending';
    case 'confirming':
    case 'Confirming':
      return 'Confirming';
    case 'running':
    case 'in_progress':
    case 'Executing':
    default:
      return 'Executing';
  }
};

export const isToolGroupStatusActive = (value: unknown): boolean => {
  const status = normalizeToolGroupStatus(value);
  return status === 'Executing' || status === 'Confirming' || status === 'Pending';
};
