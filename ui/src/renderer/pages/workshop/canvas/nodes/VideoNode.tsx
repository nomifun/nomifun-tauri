/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import React, { useEffect, useState } from 'react';
import type { NodeProps } from '@xyflow/react';
import { DeleteFour, DownloadOne, Info, SaveOne, VideoTwo } from '@icon-park/react';
import { useTranslation } from 'react-i18next';
import { workshopFileUrl } from '../../api';
import { useCanvasNode } from '../CanvasNodeContext';
import { isVideoFile, pickFiles } from '../media';
import type { VideoFlowNode } from '../model';
import { KIND_META } from '../model';
import { HoverToolbar, NodeCard, NodeHandles, ResizeFrame, ToolButton, UploadPlaceholder } from './nodeShared';

function VideoNodeImpl({ id, data, selected }: NodeProps<VideoFlowNode>) {
  const { t } = useTranslation();
  const api = useCanvasNode();
  const [hover, setHover] = useState(false);
  // The `/api/workshop/files/{id}` serve route is auth-exempt (see
  // `workshop_public_routes`), so a bare `<video src>` reaches it on both desktop
  // and WebUI — no blob loader needed, and this streams via HTTP range requests
  // (seek-friendly) instead of buffering the whole file into an object URL.
  const src = data.assetId ? workshopFileUrl(data.assetId) : null;
  const [status, setStatus] = useState<'loading' | 'ready' | 'error'>(src ? 'loading' : 'ready');

  useEffect(() => {
    setStatus(data.assetId ? 'loading' : 'ready');
  }, [data.assetId]);

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
          {!src ? (
            <UploadPlaceholder
              icon={<VideoTwo theme='outline' size={22} strokeWidth={3} />}
              label={t('workshopCanvas.node.video.emptyTitle', { defaultValue: '上传视频' })}
              hint={t('workshopCanvas.node.video.emptyHint', { defaultValue: '点击选择，或将视频拖到此处' })}
              onClick={() => void pickReplacement()}
            />
          ) : (
            <div className='relative h-full w-full'>
              <video
                key={src}
                src={src}
                controls
                playsInline
                onLoadedData={() => setStatus('ready')}
                onError={() => setStatus('error')}
                className='nodrag h-full w-full bg-black object-contain'
              />
              {status === 'loading' && (
                <div className='pointer-events-none absolute inset-0 flex items-center justify-center'>
                  <span className='h-18px w-18px animate-spin rounded-full border-2 border-solid border-white/30 border-t-white' />
                </div>
              )}
              {status === 'error' && (
                <div className='absolute inset-0 flex flex-col items-center justify-center gap-6px px-12px text-center text-[rgb(var(--danger-6))]' style={{ background: 'var(--color-bg-2)' }}>
                  <Info theme='outline' size={20} strokeWidth={3} />
                  <span className='text-11px'>{t('workshopCanvas.node.video.loadFailed', { defaultValue: '视频加载失败' })}</span>
                </div>
              )}
            </div>
          )}
        </NodeCard>
      </div>
    </>
  );
}

export default React.memo(VideoNodeImpl);
