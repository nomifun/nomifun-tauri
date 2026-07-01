/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import classNames from 'classnames';
import React from 'react';

/**
 * MessageListSkeleton — a chat-shaped shimmer placeholder shown while a
 * conversation's message history is loading (or before the content page has
 * resolved its conversation). Rendered instead of a bare spinner so the
 * loading state reads as "the conversation is opening", not "the app froze".
 *
 * Extracted from {@link MessageList} so the conversation-page loading gate
 * ({@link ChatConversationIndex}) and the new-conversation pending overlay can
 * reuse the exact same visual language.
 */
const MessageListSkeleton: React.FC = () => {
  const rows = [
    { align: 'left', bubbleWidth: '100%', lines: [72, 58, 64] },
    { align: 'right', bubbleWidth: '82%', lines: [54, 48] },
    { align: 'left', bubbleWidth: '100%', lines: [68, 76, 44] },
    { align: 'left', bubbleWidth: '100%', lines: [46, 52] },
    { align: 'right', bubbleWidth: '78%', lines: [60, 42, 36] },
    { align: 'left', bubbleWidth: '100%', lines: [74, 62] },
    { align: 'right', bubbleWidth: '84%', lines: [52, 66] },
    { align: 'left', bubbleWidth: '100%', lines: [64, 56, 40] },
    { align: 'right', bubbleWidth: '80%', lines: [58, 46] },
  ] as const;

  return (
    <div
      className='flex-1 h-full overflow-y-auto pb-10px box-border'
      data-testid='message-list-skeleton'
      style={{ minHeight: '100%' }}
    >
      <div className='min-h-full flex flex-col justify-between py-10px box-border'>
        {rows.map((row, index) => (
          <div
            key={index}
            className={classNames(
              'w-full min-w-0 flex items-start message-item px-8px m-t-10px max-w-full md:max-w-780px mx-auto',
              {
                'justify-start': row.align === 'left',
                'justify-end': row.align === 'right',
              }
            )}
          >
            <div
              className='flex-none min-w-0 rd-16px p-14px'
              style={{
                width: row.bubbleWidth,
                maxWidth: '100%',
                background: 'var(--color-fill-1)',
                border: '1px solid var(--color-border-2)',
              }}
            >
              <div className='flex flex-col gap-10px'>
                {row.lines.map((width, lineIndex) => (
                  <div
                    key={lineIndex}
                    className='h-12px rd-999px'
                    style={{
                      width: `${width}%`,
                      background:
                        'linear-gradient(90deg, var(--color-fill-2) 0%, var(--color-fill-3) 50%, var(--color-fill-2) 100%)',
                      backgroundSize: '200% 100%',
                      animation: 'message-list-skeleton-shimmer 1.4s ease-in-out infinite',
                    }}
                  />
                ))}
              </div>
            </div>
          </div>
        ))}
      </div>
      <style>{`
        @keyframes message-list-skeleton-shimmer {
          0% { background-position: 200% 0; }
          100% { background-position: -200% 0; }
        }
      `}</style>
    </div>
  );
};

export default MessageListSkeleton;
