import { describe, expect, test } from 'bun:test';

import { shouldResetTurnProcessReceiptExpansion } from './TurnProcessReceipt';

describe('TurnProcessReceipt expansion state', () => {
  test('does not reset the same receipt when only defaultExpanded changes', () => {
    expect(
      shouldResetTurnProcessReceiptExpansion(
        { receiptId: 'receipt-tool', canExpand: true },
        { receiptId: 'receipt-tool', canExpand: true }
      )
    ).toBe(false);
  });

  test('resets when a different receipt replaces the current one', () => {
    expect(
      shouldResetTurnProcessReceiptExpansion(
        { receiptId: 'receipt-tool', canExpand: true },
        { receiptId: 'receipt-permission', canExpand: true }
      )
    ).toBe(true);
  });

  test('resets when detail availability changes', () => {
    expect(
      shouldResetTurnProcessReceiptExpansion(
        { receiptId: 'receipt-tool', canExpand: false },
        { receiptId: 'receipt-tool', canExpand: true }
      )
    ).toBe(true);
  });
});
