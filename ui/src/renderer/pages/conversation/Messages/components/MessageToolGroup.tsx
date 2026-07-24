/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import { ipcBridge } from '@/common';
import { parseConfirmationCorrelationId, type IMessageToolGroup } from '@/common/chat/chatLib';
import { optionalDisplayText, toDisplayText } from '@/common/chat/displayText';
import { iconColors } from '@/renderer/styles/colors';
import { Alert, Button, Radio, Tag } from '@arco-design/web-react';
import { LoadingOne } from '@icon-park/react';
import React, { useMemo, useState } from 'react';
import { useTranslation } from 'react-i18next';
import FeedbackButton from '@/renderer/components/base/FeedbackButton';
import FileChangesPanel from '@/renderer/components/base/FileChangesPanel';
import { useDiffPreviewHandlers } from '@/renderer/hooks/file/useDiffPreviewHandlers';
import { useConversationContextSafe } from '@/renderer/hooks/context/ConversationContext';
import { parseDiff } from '@/renderer/utils/file/diffUtils';
import MessageFileChanges from '../MessageFileChanges';
import CollapsibleContent from '@renderer/components/chat/CollapsibleContent';
import LocalImageView from '@renderer/components/media/LocalImageView';
import MarkdownView from '@renderer/components/Markdown';
import { ToolConfirmationOutcome } from '@renderer/utils/common';
import { COLLAPSE_CONFIG, TEXT_CONFIG } from '../constants';
import { MESSAGE_BODY_FONT_SIZE, MESSAGE_BODY_LINE_HEIGHT } from '../typography';
import type { WriteFileResult } from '../types';
import {
  enforceToolGroupArtifactTrust,
  getSuccessfulLegacyImage,
  isSuccessfulWriteFileResult,
} from './toolGroupArtifactVisibility';

const CODE_STYLE = { marginTop: 4, marginBottom: 4 };

// Alert 组件样式常量 Alert component style constant
// 顶部对齐图标与内容，避免多行文本时图标垂直居中
const ALERT_CLASSES =
  '!items-start !rd-8px !px-8px [&_.arco-alert-icon]:flex [&_.arco-alert-icon]:items-start [&_.arco-alert-content-wrapper]:flex [&_.arco-alert-content-wrapper]:items-start [&_.arco-alert-content-wrapper]:w-full [&_.arco-alert-content]:flex-1';

// CollapsibleContent 高度常量 CollapsibleContent height constants
const RESULT_MAX_HEIGHT = COLLAPSE_CONFIG.MAX_HEIGHT;

interface IMessageToolGroupProps {
  message: IMessageToolGroup;
}

const useConfirmationButtons = (
  confirmationDetails: IMessageToolGroupProps['message']['content'][number]['confirmationDetails'],
  t: (key: string, options?: any) => string
) => {
  return useMemo(() => {
    if (!confirmationDetails) return {};
    let question: string;
    const options: Array<{ label: string; value: ToolConfirmationOutcome }> = [];
    switch (confirmationDetails.type) {
      case 'edit':
        {
          question = t('messages.confirmation.applyChange');
          options.push(
            {
              label: t('messages.confirmation.yesAllowOnce'),
              value: ToolConfirmationOutcome.ProceedOnce,
            },
            {
              label: t('messages.confirmation.yesAllowAlways'),
              value: ToolConfirmationOutcome.ProceedAlways,
            },
            { label: t('messages.confirmation.no'), value: ToolConfirmationOutcome.Cancel }
          );
        }
        break;
      case 'exec':
        {
          question = t('messages.confirmation.allowExecution');
          options.push(
            {
              label: t('messages.confirmation.yesAllowOnce'),
              value: ToolConfirmationOutcome.ProceedOnce,
            },
            {
              label: t('messages.confirmation.yesAllowAlways'),
              value: ToolConfirmationOutcome.ProceedAlways,
            },
            { label: t('messages.confirmation.no'), value: ToolConfirmationOutcome.Cancel }
          );
        }
        break;
      case 'info':
        {
          question = t('messages.confirmation.proceed');
          options.push(
            {
              label: t('messages.confirmation.yesAllowOnce'),
              value: ToolConfirmationOutcome.ProceedOnce,
            },
            {
              label: t('messages.confirmation.yesAllowAlways'),
              value: ToolConfirmationOutcome.ProceedAlways,
            },
            { label: t('messages.confirmation.no'), value: ToolConfirmationOutcome.Cancel }
          );
        }
        break;
      default: {
        const mcpProps = confirmationDetails;
        question = t('messages.confirmation.allowMCPTool', {
          toolName: toDisplayText(mcpProps.tool_name),
          serverName: toDisplayText(mcpProps.server_name),
        });
        options.push(
          {
            label: t('messages.confirmation.yesAllowOnce'),
            value: ToolConfirmationOutcome.ProceedOnce,
          },
          {
            label: t('messages.confirmation.yesAlwaysAllowTool', {
              toolName: toDisplayText(mcpProps.tool_name),
              serverName: toDisplayText(mcpProps.server_name),
            }),
            value: ToolConfirmationOutcome.ProceedAlwaysTool,
          },
          {
            label: t('messages.confirmation.yesAlwaysAllowServer', {
              serverName: toDisplayText(mcpProps.server_name),
            }),
            value: ToolConfirmationOutcome.ProceedAlwaysServer,
          },
          { label: t('messages.confirmation.no'), value: ToolConfirmationOutcome.Cancel }
        );
      }
    }
    return {
      question,
      options,
    };
  }, [confirmationDetails, t]);
};

const EditConfirmationDiff: React.FC<{ diff: string; file_name: string; title: string }> = ({
  diff,
  file_name,
  title,
}) => {
  const fileInfo = useMemo(() => parseDiff(diff, file_name), [diff, file_name]);
  const display_name = file_name.split(/[/\\]/).pop() || file_name;
  const { handleFileClick, handleDiffClick } = useDiffPreviewHandlers({
    diffText: diff,
    display_name,
    file_path: file_name,
    title,
  });

  return (
    <FileChangesPanel
      title={title}
      files={[fileInfo]}
      onFileClick={handleFileClick}
      onDiffClick={handleDiffClick}
      defaultExpanded={true}
    />
  );
};

const ConfirmationDetails: React.FC<{
  content: IMessageToolGroupProps['message']['content'][number];
  onConfirm: (outcome: ToolConfirmationOutcome) => void;
  readOnly?: boolean;
}> = ({ content, onConfirm, readOnly }) => {
  const { t } = useTranslation();
  const { confirmationDetails } = content;
  if (!confirmationDetails) return;
  const node = useMemo(() => {
    if (!confirmationDetails) return null;
    switch (confirmationDetails.type) {
      case 'edit':
        return null; // Rendered separately below with hooks support
      case 'exec': {
        const bashSnippet = `\`\`\`bash\n${toDisplayText(confirmationDetails.command)}\n\`\`\``;
        return (
          <div className='w-full max-w-100% min-w-0'>
            <MarkdownView codeStyle={CODE_STYLE} fontSize={MESSAGE_BODY_FONT_SIZE} lineHeight={MESSAGE_BODY_LINE_HEIGHT}>
              {bashSnippet}
            </MarkdownView>
          </div>
        );
      }
      case 'info':
        return <span className='text-t-primary'>{toDisplayText(confirmationDetails.prompt)}</span>;
      case 'mcp':
        return <span className='text-t-primary'>{toDisplayText(confirmationDetails.tool_display_name)}</span>;
    }
  }, [confirmationDetails]);

  const { question = '', options = [] } = useConfirmationButtons(confirmationDetails, t);

  const [selected, setSelected] = useState<ToolConfirmationOutcome | null>(null);

  const isConfirm = content.status === 'Confirming';

  return (
    <div>
      {confirmationDetails.type === 'edit' ? (
        <EditConfirmationDiff
          diff={toDisplayText(confirmationDetails?.file_diff)}
          file_name={toDisplayText(confirmationDetails.file_name)}
          title={isConfirm ? toDisplayText(confirmationDetails.title) : toDisplayText(content.description)}
        />
      ) : (
        node
      )}
      {!readOnly && content.status === 'Confirming' && (
        <>
          <div className='mt-10px text-t-primary'>{question}</div>
          <Radio.Group direction='vertical' size='mini' value={selected} onChange={setSelected}>
            {options.map((item) => {
              return (
                <Radio key={item.value} value={item.value}>
                  {item.label}
                </Radio>
              );
            })}
          </Radio.Group>
          <div className='flex justify-start pl-20px'>
            <Button
              type='primary'
              size='mini'
              disabled={!selected}
              onClick={() => {
                // The button is disabled while `selected` is null, so this guard
                // never blocks a real click — it only narrows the type for onConfirm.
                if (selected) onConfirm(selected);
              }}
            >
              {t('messages.confirm')}
            </Button>
          </div>
        </>
      )}
    </div>
  );
};

// Legacy tool-group image display. LocalImageView owns source-generation
// cancellation and stale/error clearing for local, remote, data and blob URLs.
const ImageDisplay: React.FC<{
  imgUrl: string;
  relativePath?: string;
}> = ({ imgUrl, relativePath }) => (
  <div className='my-8px' style={{ maxWidth: '197px' }}>
    <LocalImageView
      src={imgUrl}
      alt={relativePath || 'Generated image'}
      className='block max-w-full max-h-320px object-contain rd-8px'
    />
  </div>
);

const ToolResultDisplay: React.FC<{
  content: IMessageToolGroupProps['message']['content'][number];
}> = ({ content }) => {
  const { result_display, name } = content;
  const toolName = toDisplayText(name);

  // 图片生成特殊处理 Special handling for image generation
  const successfulImage = getSuccessfulLegacyImage(content);
  if (toolName === 'ImageGeneration' && successfulImage) {
    return (
      <LocalImageView
        src={successfulImage.imgUrl}
        alt={successfulImage.relativePath || successfulImage.imgUrl}
        className='max-w-100% max-h-100%'
      />
    );
  }

  // 将结果转换为字符串 Convert result to string
  const display = toDisplayText(result_display);

  // 使用 CollapsibleContent 包装长内容
  // Wrap long content with CollapsibleContent
  return (
    <CollapsibleContent maxHeight={RESULT_MAX_HEIGHT} defaultCollapsed={true} useMask={false}>
      <pre
        className='text-t-primary whitespace-pre-wrap break-words m-0'
        style={{ fontSize: `${TEXT_CONFIG.FONT_SIZE}px`, lineHeight: TEXT_CONFIG.LINE_HEIGHT }}
      >
        {display}
      </pre>
    </CollapsibleContent>
  );
};

const MessageToolGroup: React.FC<IMessageToolGroupProps> = ({ message }) => {
  const { t } = useTranslation();
  const readOnly = useConversationContextSafe()?.readOnly === true;
  const toolContent = useMemo(
    () =>
      Array.isArray(message.content)
        ? message.content.map(enforceToolGroupArtifactTrust)
        : [],
    [message.content]
  );

  // 收集所有 WriteFile 结果用于汇总显示 / Collect all WriteFile results for summary display
  const writeFileResults = useMemo(() => {
    return toolContent
      .filter(
        (item) => isSuccessfulWriteFileResult(item)
      )
      .map((item) => {
        const result = item.result_display as WriteFileResult;
        return {
          file_diff: toDisplayText(result.file_diff),
          file_name: toDisplayText(result.file_name),
        };
      });
  }, [toolContent]);

  // 找到第一个 WriteFile 的索引 / Find the index of first WriteFile
  const firstWriteFileIndex = useMemo(() => {
    return toolContent.findIndex(
      (item) => isSuccessfulWriteFileResult(item)
    );
  }, [toolContent]);

  return (
    <div>
      {toolContent.map((content, index) => {
        const { status, call_id, name, description, result_display, confirmationDetails } = content;
        const statusText = toDisplayText(status);
        const callIdText = toDisplayText(call_id, `tool-${index}`);
        const nameText = toDisplayText(name, 'Tool');
        const descriptionText = optionalDisplayText(description);
        const isLoading = statusText !== 'Success' && statusText !== 'Error' && statusText !== 'Canceled';
        // status === "Confirming" &&
        if (confirmationDetails) {
          return (
            <ConfirmationDetails
              key={callIdText}
              content={content}
              readOnly={readOnly}
              onConfirm={(outcome) => {
                if (readOnly) return;
                ipcBridge.conversation.confirmMessage
                  .invoke({
                    confirm_key: outcome,
                    msg_id: message.msg_id ?? parseConfirmationCorrelationId(callIdText),
                    call_id: callIdText,
                    conversation_id: message.conversation_id,
                  })
                  .then(() => {
                    // confirmation sent successfully
                  })
                  .catch((error) => {
                    console.error('Failed to confirm message:', error);
                  });
              }}
            ></ConfirmationDetails>
          );
        }

        // WriteFile 特殊处理：使用 MessageFileChanges 汇总显示 / WriteFile special handling: use MessageFileChanges for summary display
        if (statusText === 'Success' && nameText === 'WriteFile' && typeof result_display !== 'string') {
          if (result_display && typeof result_display === 'object' && 'file_diff' in result_display) {
            // 只在第一个 WriteFile 位置显示汇总组件 / Only show summary component at first WriteFile position
            if (index === firstWriteFileIndex && writeFileResults.length > 0) {
              return (
                <div className='w-full min-w-0' key={callIdText}>
                  <MessageFileChanges writeFileChanges={writeFileResults} />
                </div>
              );
            }
            // 跳过其他 WriteFile / Skip other WriteFile
            return null;
          }
        }

        // ImageGeneration 特殊处理：单独展示图片，不用 Alert 包裹 Special handling for ImageGeneration: display image separately without Alert wrapper
        const successfulImage = getSuccessfulLegacyImage(content);
        if (successfulImage) {
          return (
            <ImageDisplay
              key={callIdText}
              imgUrl={successfulImage.imgUrl}
              relativePath={successfulImage.relativePath}
            />
          );
        }

        // 通用工具调用展示 Generic tool call display
        // 将可展开的长内容放在 Alert 下方，保持 Alert 仅展示头部信息
        return (
          <div key={callIdText}>
            <Alert
              className={ALERT_CLASSES}
              type={
                statusText === 'Error'
                  ? 'error'
                  : statusText === 'Success'
                    ? 'success'
                    : statusText === 'Canceled'
                      ? 'warning'
                      : 'info'
              }
              icon={
                isLoading && (
                  <LoadingOne theme='outline' size='12' fill={iconColors.primary} className='loading lh-[1] flex' />
                )
              }
              content={
                <div>
                  <Tag className={'mr-4px'}>
                    {nameText}
                    {statusText === 'Canceled' ? `(${t('messages.canceledExecution')})` : ''}
                  </Tag>
                </div>
              }
            />

            {(descriptionText || result_display || statusText === 'Error') && (
              <div className='mt-8px'>
                {descriptionText && (
                  <div
                    className={`text-12px text-t-secondary mb-2 ${statusText === 'Error' ? 'whitespace-pre-wrap break-words' : 'truncate'}`}
                  >
                    {descriptionText}
                  </div>
                )}
                {result_display && (
                  <div>
                    {/* 在 Alert 外展示完整结果 Display full result outside Alert */}
                    {/* ToolResultDisplay 内部已包含 CollapsibleContent，避免嵌套 */}
                    {/* ToolResultDisplay already contains CollapsibleContent internally, avoid nesting */}
                    <ToolResultDisplay content={content} />
                  </div>
                )}
                {statusText === 'Error' && (
                  <div className='mt-4px flex justify-end'>
                    <FeedbackButton module='conversation-session' />
                  </div>
                )}
              </div>
            )}
          </div>
        );
      })}
    </div>
  );
};

export default MessageToolGroup;
