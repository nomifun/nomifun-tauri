/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import React, { useCallback, useMemo, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Message } from '@arco-design/web-react';
import SendBox from '@renderer/components/chat/SendBox';
import type { SlashCommandItem } from '@/common/chat/slash/types';
import { useAddEventListener } from '@/renderer/utils/emitter';
import type { FileOrFolderItem } from '@/renderer/utils/file/fileTypes';
import type { XtermViewHandle } from './XtermView';
import './terminalSendBox.css';

/**
 * Enhanced composer for a terminal session. Reuses the conversation SendBox for
 * multiline editing, paste, and quick commands, but routes submit to the PTY
 * input endpoint instead of the LLM.
 *
 * Submitting EXECUTES in the terminal: we send a carriage return (`\r`) — the
 * exact byte a real Enter keypress produces — not a line feed (`\n`). Raw-mode
 * TUI agents (claude, vim, REPLs) and the shell line discipline treat `\r` as
 * "run this", whereas `\n` (Ctrl-J) often just inserts a newline and does not
 * submit. Internal newlines in a multi-line draft are likewise sent as `\r` so
 * each line executes in turn.
 *
 * NOTE: SendBox is a *controlled* component — its `value`/`onChange` default to
 * '' and a no-op. We MUST own the input state here and pass it through, or the
 * textarea is frozen and the user cannot type.
 *
 * Per the design, no model/permission selector here — the running process is
 * already fixed at create time. The composer carries quick-commands + flexible
 * editing only.
 */

interface TerminalSendBoxProps {
  /** Clear the visible terminal (delegated to the xterm view). */
  onClearView?: () => void;
  /** Handle to the xterm view — used to detect bracketed paste mode on submit. */
  terminalApi?: React.MutableRefObject<XtermViewHandle | null>;
  disabled?: boolean;
}

const TerminalSendBox: React.FC<TerminalSendBoxProps> = ({ onClearView, terminalApi, disabled }) => {
  const { t } = useTranslation();
  const [input, setInput] = useState('');

  // Insert the path of each selected node into the command draft. A path with a
  // space is wrapped in double quotes so the shell treats it as one argument;
  // otherwise the raw path is inserted. Paths are space-separated and appended
  // after the current draft (with a leading space when the draft is non-empty
  // and doesn't already end in whitespace). An empty selection (the reset case
  // emitted on selection-clear) is a no-op — we never wipe the user's typing.
  const insertPaths = useCallback((items: Array<string | FileOrFolderItem>) => {
    if (!items.length) return;
    const tokens = items
      .map((item) => (typeof item === 'string' ? item : item.path))
      .filter((p): p is string => Boolean(p))
      .map((p) => (/\s/.test(p) ? `"${p}"` : p));
    if (!tokens.length) return;
    const insertion = tokens.join(' ');
    setInput((prev) => {
      if (!prev) return insertion;
      const sep = /\s$/.test(prev) ? '' : ' ';
      return `${prev}${sep}${insertion}`;
    });
  }, []);

  // The terminal workspace rail emits these on file selection / "add to command".
  // Both replace and append behave the same here: insert the path(s) into the
  // draft. We deliberately ignore `.clear`/`.workspace.refresh` — those drive the
  // rail body, not the composer.
  useAddEventListener('terminal.selected.file', insertPaths, [insertPaths]);
  useAddEventListener('terminal.selected.file.append', insertPaths, [insertPaths]);

  const quickCommands = useMemo<SlashCommandItem[]>(
    () => [
      {
        name: 'clear',
        description: t('terminal.quickCommand.clear'),
        kind: 'builtin',
        source: 'builtin',
        selectionBehavior: 'execute',
      },
      {
        name: 'interrupt',
        description: t('terminal.quickCommand.interrupt'),
        kind: 'builtin',
        source: 'builtin',
        selectionBehavior: 'execute',
      },
    ],
    [t]
  );

  const writeToPty = async (text: string): Promise<void> => {
    const api = terminalApi?.current;
    if (!api) throw new Error('Terminal is still initializing');
    await api.writeToPty(text);
  };

  const handleSend = async (message: string) => {
    // Drop only trailing whitespace; preserve internal structure.
    const text = message.replace(/\s+$/, '');
    if (!text) return;
    // Normalize internal newlines to CR (the byte a real Enter produces).
    const body = text.replace(/\r?\n/g, '\r');
    // Multi-line submit: if the running program has bracketed paste enabled,
    // wrap the whole block as ONE paste (ESC[200~ … ESC[201~) so it is delivered
    // as a single paste rather than executed line by line, then a trailing CR
    // submits it once. Sent as a single write so the wrapper and the CR cannot
    // arrive out of order. When bracketed paste is off we fall back to the plain
    // CR stream — the program cannot distinguish a paste in that mode anyway.
    try {
      if (body.includes('\r') && terminalApi?.current?.isBracketedPaste()) {
        await writeToPty(`\x1b[200~${body}\x1b[201~\r`);
      } else {
        await writeToPty(`${body}\r`);
      }
    } catch (error) {
      setInput(message);
      Message.error(t('terminal.inputFailed', { defaultValue: 'Failed to send terminal input.' }));
      throw error;
    }
  };

  const handleBuiltin = (name: string) => {
    if (name === 'clear') {
      onClearView?.();
      setInput('');
    } else if (name === 'interrupt') {
      // Send Ctrl-C (ETX) to the PTY.
      void writeToPty('\x03').catch(() => {
        Message.error(t('terminal.inputFailed', { defaultValue: 'Failed to send terminal input.' }));
      });
    }
  };

  // Surface the submit shortcut (Enter) next to the send button. Enter already
  // submits via SendBox's keydown handler; this is just the visual affordance.
  const enterHint = (
    <span
      className='mr-6px inline-flex items-center justify-center min-w-18px h-18px px-4px rd-4px text-11px leading-none text-t-tertiary bg-fill-2 border border-solid border-[var(--color-border-2)] select-none'
      title={t('terminal.submitHint')}
      aria-hidden='true'
    >
      ↩
    </span>
  );

  return (
    <SendBox
      className='terminal-sendbox-compact'
      value={input}
      onChange={setInput}
      onSend={handleSend}
      disabled={disabled}
      enableBtw={false}
      slash_commands={quickCommands}
      onSlashBuiltinCommand={handleBuiltin}
      sendButtonPrefix={enterHint}
      placeholder={t('terminal.composerPlaceholder')}
    />
  );
};

export default TerminalSendBox;
