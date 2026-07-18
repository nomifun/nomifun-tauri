import { readFileSync } from 'node:fs';
import { describe, expect, test } from 'bun:test';

const source = readFileSync(new URL('./index.tsx', import.meta.url), 'utf8');
const platformSendBoxes = [
  '../../../pages/conversation/platforms/remote/RemoteSendBox.tsx',
  '../../../pages/conversation/platforms/openclaw/OpenClawSendBox.tsx',
  '../../../pages/conversation/platforms/nanobot/NanobotSendBox.tsx',
  '../../../pages/conversation/platforms/acp/AcpSendBox.tsx',
  '../../../pages/conversation/platforms/nomi/NomiSendBox.tsx',
].map((path) => ({ path, source: readFileSync(new URL(path, import.meta.url), 'utf8') }));

describe('SendBox stop interaction', () => {
  test('deduplicates stop clicks and blocks send/steer until stop settles', () => {
    expect(source.includes('const [isStopping, setIsStopping] = useState(false)')).toBe(true);
    expect(source.includes('if (!onStop || isStoppingRef.current) return;')).toBe(true);
    expect(source.includes('isStoppingRef.current = true;')).toBe(true);
    expect(source.includes('isStoppingRef.current = false;')).toBe(true);
    expect(source.includes('setIsStopping(true);')).toBe(true);
    expect(source.includes('setIsStopping(false);')).toBe(true);
    expect(source.includes('if (isUploading || isStopping) return;')).toBe(true);
    expect(source.includes('if (!onSteer || isUploading || isStopping) return;')).toBe(true);
    expect(source.includes("data-testid='sendbox-stop-btn'")).toBe(true);
    expect(source.includes('disabled={isStopping}')).toBe(true);
    expect(source.includes('loading={isStopping}')).toBe(true);
  });

  test('remounts shared stop state when switching conversations', () => {
    for (const platform of platformSendBoxes) {
      expect(/<SendBox\r?\n\s+key=\{conversation_id\}/.test(platform.source)).toBe(true);
    }
  });

  test('checks authoritative start and completion generations before applying a stop result', () => {
    for (const platform of platformSendBoxes) {
      const stopResultIndex = platform.source.indexOf(
        'const result = await stopConversationAndConfirmRelease(conversation_id);'
      );
      const statusIndex = platform.source.indexOf(
        'const stopAttemptStatus = getStopAttemptStatus(stopAttempt);',
        stopResultIndex
      );
      const confirmIndex = platform.source.indexOf('confirmStopped();', statusIndex);
      const queueResetIndex = platform.source.indexOf("resetActiveExecution('external-reset');", statusIndex);

      expect(stopResultIndex >= 0).toBe(true);
      expect(statusIndex > stopResultIndex).toBe(true);
      expect(confirmIndex > statusIndex).toBe(true);
      expect(queueResetIndex > statusIndex).toBe(true);
      expect(platform.source.includes('getTurnCompletionGeneration')).toBe(true);
      expect(platform.source.includes('shouldReleaseStopInteraction(stopAttemptStatus)')).toBe(true);
    }
  });

  test('manual stop pauses the remaining queue before releasing the active execution gate', () => {
    for (const platform of platformSendBoxes) {
      const stop = platform.source.indexOf('const stopAttempt = beginStopAttempt();');
      const pause = platform.source.indexOf('pause();', stop);
      const reset = platform.source.indexOf("resetActiveExecution('stop');", stop);

      expect(stop >= 0).toBe(true);
      expect(pause > stop).toBe(true);
      expect(reset > pause).toBe(true);
    }
  });

  test('queue execution guards platform-local resolve and reject side effects', () => {
    for (const platform of platformSendBoxes) {
      const execute = platform.source.indexOf('const executeCommand = useCallback');
      const invoke = platform.source.indexOf('.sendMessage.invoke({', execute);
      const resolveFence = platform.source.indexOf(
        'if (execution && !execution.isCurrent()) return;',
        invoke
      );
      const reject = platform.source.indexOf('catch (error', resolveFence);
      const rejectFence = platform.source.indexOf(
        'if (execution && !execution.isCurrent()) return;',
        reject
      );

      expect(execute >= 0).toBe(true);
      expect(invoke > execute).toBe(true);
      expect(resolveFence > invoke).toBe(true);
      expect(reject > resolveFence).toBe(true);
      expect(rejectFence > reject).toBe(true);
    }
  });
});
