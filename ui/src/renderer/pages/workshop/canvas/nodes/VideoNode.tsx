/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import React, { useState } from 'react';
import type { NodeProps } from '@xyflow/react';
import { DeleteFour, DownloadOne, Info, SaveOne, VideoTwo } from '@icon-park/react';
import { useTranslation } from 'react-i18next';
import { useCanvasNode } from '../CanvasNodeContext';
import { isVideoFile, pickFiles, useWorkshopMedia } from '../media';
import type { VideoFlowNode } from '../model';
import { KIND_META } from '../model';
import { HoverToolbar, NodeCard, NodeHandles, ResizeFrame, ToolButton, UploadPlaceholder } from './nodeShared';

function VideoNodeImpl({ id, data, selected }: NodeProps<VideoFlowNode>) {
  const { t } = useTranslation();
  const api = useCanvasNode();
  const media = useWorkshopMedia(data.assetId);
  const [hover, setHover] = useState(false);

  const showTools = (hover || selected) && !!data.assetId;

  const pickReplacement = async (): Promise<void> => {
    const files = await pickFiles('video/*', false);
    const file = files.find(isVideoFile);
    if (file) api.fillNodeFromFile(id, file);
  };

  return (
    <>
      <ResizeFrame visible={selected} minWidth={KIND_META.video.minWidth} minHeight={KIND_META.video.minHeight} />
      <div className='h-full w-full' onMouseEnter={() => setHover(true)} onMouseLeave={() => setHover(false)}>
        <HoverToolbar show={showTools}>
          <ToolButton label={t('workshopCanvas.node.video.download', { defaultValue: '下载' })} onClick={() => data.assetId && api.downloadAsset(data.assetId)}>
            <DownloadOne theme='outline' size={15} strokeWidth={3} />
          </ToolButton>
          <ToolButton label={t('workshopCanvas.node.video.saveToLibrary', { defaultValue: '存入资产库' })} onClick={() => data.assetId && api.saveAssetToLibrary(data.assetId)}>
            <SaveOne theme='outline' size={15} strokeWidth={3} />
          </ToolButton>
          <ToolButton label={t('workshopCanvas.node.delete', { defaultValue: '删除' })} danger onClick={() => api.removeNode(id)}>
            <DeleteFour theme='outline' size={15} strokeWidth={3} />
          </ToolButton>
        </HoverToolbar>

        <NodeCard selected={selected}>
          <NodeHandles />
          {!data.assetId ? (
            <UploadPlaceholder
              icon={<VideoTwo theme='outline' size={22} strokeWidth={3} />}
              label={t('workshopCanvas.node.video.emptyTitle', { defaultValue: '上传视频' })}
              hint={t('workshopCanvas.node.video.emptyHint', { defaultValue: '点击选择，或将视频拖到此处' })}
              onClick={() => void pickReplacement()}
            />
          ) : media.status === 'ready' ? (
            <video
              src={media.url}
              controls
              playsInline
              className='nodrag h-full w-full bg-black object-contain'
            />
          ) : media.status === 'error' ? (
            <div className='flex h-full w-full flex-col items-center justify-center gap-6px px-12px text-center text-[rgb(var(--danger-6))]'>
              <Info theme='outline' size={20} strokeWidth={3} />
              <span className='text-11px'>{t('workshopCanvas.node.video.loadFailed', { defaultValue: '视频加载失败' })}</span>
            </div>
          ) : (
            <div className='flex h-full w-full items-center justify-center'>
              <span className='h-18px w-18px animate-spin rounded-full border-2 border-solid border-[var(--color-fill-3)] border-t-[rgb(var(--primary-6))]' />
            </div>
          )}
        </NodeCard>
      </div>
    </>
  );
}

export default React.memo(VideoNodeImpl);
