import { ipcBridge } from '@/common';
import type { TChatConversation } from '@/common/config/storage';
import { refreshConversationCache } from '@/renderer/pages/conversation/utils/conversationCache';
import { isDesktopShell } from '@/renderer/utils/platform';
import { useArcoMessage } from '@/renderer/utils/ui/useArcoMessage';
import { FolderFocus } from '@icon-park/react';
import { Button, Tooltip } from '@arco-design/web-react';
import React, { useState } from 'react';
import { useTranslation } from 'react-i18next';

interface WorkspaceBindButtonProps {
  /**
   * Conversation whose `extra.workspace` will be redirected to the picked
   * directory. Required to issue the PATCH; when absent the button hides.
   */
  conversation_id?: number;
}

/**
 * Workspace Bind Button — shown in the workspace panel header for **temporary**
 * sessions (`conversation.extra.is_temporary_workspace === true`). It lets the
 * user redirect the session's workspace to a real folder on disk, so the agent
 * works directly inside that directory.
 *
 * Picking a directory relies on the native open dialog, so the entry only makes
 * sense inside the desktop shell — it hides in WebUI/browser mode. Visual
 * language mirrors {@link WorkspaceOpenButton} (text button, small size, folder
 * icon family) so the two occupy the same slot interchangeably.
 *
 * On success the conversation cache is refreshed; the backend re-derives
 * `is_temporary_workspace` as false on the next read (the workspace now lives
 * outside `data_dir`), which flips the header over to `WorkspaceOpenButton` and
 * replaces the "Temporary Space" label with the real path — all existing logic.
 */
const WorkspaceBindButton: React.FC<WorkspaceBindButtonProps> = ({ conversation_id }) => {
  const { t } = useTranslation();
  const [message, messageHolder] = useArcoMessage();
  const [binding, setBinding] = useState(false);

  // Hooks above run unconditionally; the desktop-shell / id guard comes after so
  // the entry stays hidden in WebUI (native dialog unavailable) and when we have
  // no conversation to patch.
  if (!isDesktopShell() || conversation_id == null) return null;

  const handleBind = async () => {
    if (binding) return;
    setBinding(true);
    try {
      const dirs = await ipcBridge.dialog.showOpen.invoke({ properties: ['openDirectory', 'createDirectory'] });
      const target = dirs?.[0]?.trim();
      if (!target) return;

      const ok = await ipcBridge.conversation.update.invoke({
        id: conversation_id,
        updates: { extra: { workspace: target } as TChatConversation['extra'] },
        merge_extra: true,
      });

      if (ok) {
        // Re-pull the conversation so `extra.workspace` /
        // `extra.is_temporary_workspace` propagate to every consumer
        // (WorkspaceRailBody, header, collapse preference).
        await refreshConversationCache(conversation_id);
        message.success(t('conversation.workspace.bindWorkspace.success'));
      } else {
        message.error(t('conversation.workspace.bindWorkspace.failed'));
      }
    } catch (error) {
      console.error('[WorkspaceBindButton] Failed to bind workspace directory:', error);
      message.error(t('conversation.workspace.bindWorkspace.failed'));
    } finally {
      setBinding(false);
    }
  };

  return (
    <div className='workspace-bind-button flex items-center'>
      {messageHolder}
      <Tooltip content={t('conversation.workspace.bindWorkspace.hint')}>
        <Button
          type='text'
          size='small'
          loading={binding}
          className='workspace-bind-button__btn flex items-center gap-4px px-8px'
          aria-label={t('conversation.workspace.bindWorkspace.label')}
          onClick={handleBind}
        >
          <FolderFocus size={16} />
        </Button>
      </Tooltip>
    </div>
  );
};

export default WorkspaceBindButton;
