import { describe, expect, test } from 'bun:test';
import { parseExecutionStepId } from '@/common/types/ids';
import { resolveExecutionCanvasFocusStepId, summarizeExecutionText } from './executionCanvasPresentation';

describe('execution canvas summaries', () => {
  test('removes markdown and tool markup while preserving meaningful prose', () => {
    expect(
      summarizeExecutionText('## 结果\n<tool_call> **完成**：使用 `[容器](https://example.com)` 输出。'),
    ).toBe('结果 完成：使用 容器 输出。');
  });

  test('bounds long output without exposing the full transcript on the canvas', () => {
    const summary = summarizeExecutionText('这是一个需要在详情中继续阅读的很长执行结果', 12);
    expect(summary).toBe('这是一个需要在详情中继…');
    expect(summary?.length).toBeLessThanOrEqual(12);
  });

  test('omits empty presentation content', () => {
    expect(summarizeExecutionText('  <tool_call>  ')).toBeUndefined();
  });

  test('falls back from a superseded hover target to the still-current projected step', () => {
    const current = parseExecutionStepId('0190f5fe-7c00-7a00-8000-000000000001');
    const superseded = parseExecutionStepId('0190f5fe-7c00-7a00-8000-000000000002');
    const alsoSuperseded = parseExecutionStepId('0190f5fe-7c00-7a00-8000-000000000003');
    expect(resolveExecutionCanvasFocusStepId(new Set([current]), superseded, current)).toBe(current);
    expect(resolveExecutionCanvasFocusStepId(new Set([current]), superseded, alsoSuperseded)).toBeNull();
  });
});
