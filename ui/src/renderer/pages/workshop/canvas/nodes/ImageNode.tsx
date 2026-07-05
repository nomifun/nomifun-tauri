/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import React, { useState } from 'react';
import type { NodeProps } from '@xyflow/react';
import { DeleteFour, DownloadOne, Erase, Info, Lock, Pic, PreviewOpen, SaveOne, Unlock } from '@icon-park/react';
import { useTranslation } from 'react-i18next';
import type { ImageEditorMode } from '../../editor';
import { useCanvasNode } from '../CanvasNodeContext';
import { isImageFile, pickFiles, useWorkshopMedia } from '../media';
import type { ImageFlowNode } from '../model';
import { KIND_META } from '../model';
import { HoverToolbar, NodeCard, NodeHandles, ResizeFrame, ToolButton, UploadPlaceholder } from './nodeShared';

const EDIT_MODES: { mode: ImageEditorMode; labelKey: string; fallback: string }[] = [
  { mode: 'crop', labelKey: 'workshopCanvas.node.image.edit.crop', fallback: '裁剪' },
  { mode: 'split', labelKey: 'workshopCanvas.node.image.edit.split', fallback: '宫格切分' },
  { mode: 'upscale', labelKey: 'workshopCanvas.node.image.edit.upscale', fallback: '放大' },
  { mode: 'mask', labelKey: 'workshopCanvas.node.image.edit.mask', fallback: '局部重绘' },
];

function ImageNodeImpl({ id, data, selected }: NodeProps<ImageFlowNode>) {
  const { t } = useTranslation();
  const api = useCanvasNode();
  const media = useWorkshopMedia(data.assetId);
  const [hover, setHover] = useState(false);
  const [editOpen, setEditOpen] = useState(false);

  const lockAspect = data.lockAspect !== false;
  const showTools = (hover || selected) && !!data.assetId;

  const pickReplacement = async (): Promise<void> => {
    const files = await pickFiles('image/*', false);
    const file = files.find(isImageFile);
    if (file) api.fillNodeFromFile(id, file);
  };

  return (
    <>
      <ResizeFrame visible={selected} minWidth={KIND_META.image.minWidth} minHeight={KIND_META.image.minHeight} keepAspectRatio={lockAspect} />
      <div
        className='h-full w-full'
        onMouseEnter={() => setHover(true)}
        onMouseLeave={() => {
          setHover(false);
          setEditOpen(false);
        }}
      >
        <HoverToolbar show={showTools}>
          <ToolButton label={t('workshopCanvas.node.image.preview', { defaultValue: '预览' })} onClick={() => api.previewImageNode(id)}>
            <PreviewOpen theme='outline' size={15} strokeWidth={3} />
          </ToolButton>
          <ToolButton label={t('workshopCanvas.node.image.download', { defaultValue: '下载' })} onClick={() => data.assetId && api.downloadAsset(data.assetId)}>
            <DownloadOne theme='outline' size={15} strokeWidth={3} />
          </ToolButton>
          <ToolButton label={t('workshopCanvas.node.image.saveToLibrary', { defaultValue: '存入资产库' })} onClick={() => data.assetId && api.saveAssetToLibrary(data.assetId)}>
            <SaveOne theme='outline' size={15} strokeWidth={3} />
          </ToolButton>
          <div className='relative'>
            <ToolButton label={t('workshopCanvas.node.image.edit.label', { defaultValue: '编辑' })} onClick={() => setEditOpen((v) => !v)}>
              <Erase theme='outline' size={15} strokeWidth={3} />
            </ToolButton>
            {editOpen && (
              <div
                className='absolute left-1/2 top-full z-20 mt-6px flex -translate-x-1/2 flex-col gap-1px rounded-9px border border-solid border-[var(--color-border-2)] bg-[var(--color-bg-2)] p-4px shadow-[0_8px_24px_rgba(0,0,0,0.2)]'
                onMouseLeave={() => setEditOpen(false)}
              >
                {EDIT_MODES.map((m) => (
                  <div
                    key={m.mode}
                    role='button'
                    tabIndex={0}
                    onClick={(e) => {
                      e.stopPropagation();
                      setEditOpen(false);
                      api.editImageNode(id, m.mode);
                    }}
                    onKeyDown={(e) => {
                      if (e.key === 'Enter' || e.key === ' ') {
                        e.preventDefault();
                        e.stopPropagation();
                        setEditOpen(false);
                        api.editImageNode(id, m.mode);
                      }
                    }}
                    className='whitespace-nowrap rounded-6px px-10px py-6px text-12px text-[var(--color-text-2)] cursor-pointer hover:bg-[var(--color-fill-2)] hover:text-[var(--color-text-1)]'
                  >
                    {t(m.labelKey, { defaultValue: m.fallback })}
                  </div>
                ))}
              </div>
            )}
          </div>
          <ToolButton
            label={
              lockAspect
                ? t('workshopCanvas.node.image.aspectFree', { defaultValue: '自由比例' })
                : t('workshopCanvas.node.image.aspectLock', { defaultValue: '锁定比例' })
            }
            onClick={() => api.updateNodeData(id, { lockAspect: !lockAspect })}
          >
            {lockAspect ? <Lock theme='outline' size={14} strokeWidth={3} /> : <Unlock theme='outline' size={14} strokeWidth={3} />}
          </ToolButton>
          <ToolButton label={t('workshopCanvas.node.delete', { defaultValue: '删除' })} danger onClick={() => api.removeNode(id)}>
            <DeleteFour theme='outline' size={15} strokeWidth={3} />
          </ToolButton>
        </HoverToolbar>

        <NodeCard selected={selected} onDoubleClick={() => data.assetId && api.previewImageNode(id)}>
          <NodeHandles />
          {!data.assetId ? (
            <UploadPlaceholder
              icon={<Pic theme='outline' size={22} strokeWidth={3} />}
              label={t('workshopCanvas.node.image.emptyTitle', { defaultValue: '上传图片' })}
              hint={t('workshopCanvas.node.image.emptyHint', { defaultValue: '点击选择，或将图片拖到此处' })}
              onClick={() => void pickReplacement()}
            />
          ) : media.status === 'ready' ? (
            <img
              src={media.url}
              alt={data.caption ?? ''}
              draggable={false}
              className='h-full w-full select-none object-contain'
              style={{ background: 'var(--color-fill-1)' }}
            />
          ) : media.status === 'error' ? (
            <div className='flex h-full w-full flex-col items-center justify-center gap-6px px-12px text-center text-[rgb(var(--danger-6))]'>
              <Info theme='outline' size={20} strokeWidth={3} />
              <span className='text-11px'>{t('workshopCanvas.node.image.loadFailed', { defaultValue: '图片加载失败' })}</span>
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

export default React.memo(ImageNodeImpl);
