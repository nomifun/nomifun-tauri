/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

/**
 * AssetDetailModal — the full-size preview + metadata sheet for one asset.
 *
 * Left: large image / playable video / scrollable full text. Right: metadata
 * (kind, collection, tags, dimensions, size, created) and, for generated
 * assets, the provenance block (prompt / model / provider / params) with a
 * copy-prompt shortcut. Footer: insert / edit / delete.
 */

import React from 'react';
import { useTranslation } from 'react-i18next';
import { Modal } from '@arco-design/web-react';
import { Copy, Delete, EditTwo, FileText, ImageFiles, LinkOne, VideoTwo } from '@icon-park/react';

import type { WorkshopAsset } from '../types';
import { useArcoMessage } from '@renderer/utils/ui/useArcoMessage';
import { useWorkshopObjectUrl } from './useWorkshopMedia';
import { formatBytes, formatDimensions } from './format';

export interface AssetDetailModalProps {
  asset: WorkshopAsset | null;
  onClose: () => void;
  onInsert: (asset: WorkshopAsset) => void;
  onEdit: (asset: WorkshopAsset) => void;
  onDelete: (asset: WorkshopAsset) => void;
}

// ─── Preview pane ───────────────────────────────────────────────────────────

const PreviewPane: React.FC<{ asset: WorkshopAsset }> = ({ asset }) => {
  const isBinary = asset.kind === 'image' || asset.kind === 'video';
  const { url, status } = useWorkshopObjectUrl(isBinary ? asset.id : null, { thumb: false, enabled: isBinary });

  if (asset.kind === 'text') {
    return (
      <div className='h-full max-h-[60vh] min-h-200px overflow-y-auto rounded-12px border border-solid border-[var(--color-border-2)] bg-[var(--color-fill-1)] p-16px'>
        <pre className='m-0 whitespace-pre-wrap break-words font-[inherit] text-13px leading-[1.65] text-[var(--color-text-1)]'>
          {asset.text_content ?? ''}
        </pre>
      </div>
    );
  }

  return (
    <div className='grid min-h-260px place-items-center overflow-hidden rounded-12px border border-solid border-[var(--color-border-2)] bg-[var(--color-fill-1)]'>
      {status === 'loading' && <span className='h-30px w-30px animate-pulse rounded-full bg-[var(--color-fill-3)]' />}
      {status === 'error' && (
        <span className='text-[var(--color-text-4)]'>
          {asset.kind === 'video' ? (
            <VideoTwo theme='outline' size={40} strokeWidth={3} />
          ) : (
            <ImageFiles theme='outline' size={40} strokeWidth={3} />
          )}
        </span>
      )}
      {url && asset.kind === 'image' && (
        <img src={url} alt={asset.title} className='max-h-[60vh] w-full object-contain' />
      )}
      {url && asset.kind === 'video' && (
        <video src={url} controls className='max-h-[60vh] w-full bg-black' />
      )}
    </div>
  );
};

// ─── Metadata rows ──────────────────────────────────────────────────────────

const MetaRow: React.FC<{ label: string; children: React.ReactNode }> = ({ label, children }) => (
  <div className='flex flex-col gap-3px'>
    <span className='text-11px font-500 uppercase tracking-wide text-[var(--color-text-4)]'>{label}</span>
    <div className='text-13px text-[var(--color-text-1)] break-words'>{children}</div>
  </div>
);

const KIND_ICON = {
  image: ImageFiles,
  video: VideoTwo,
  text: FileText,
} as const;

const AssetDetailModal: React.FC<AssetDetailModalProps> = ({ asset, onClose, onInsert, onEdit, onDelete }) => {
  const { t } = useTranslation();
  const [message, holder] = useArcoMessage();

  if (!asset) {
    return (
      <Modal visible={false} footer={null} onCancel={onClose}>
        {holder}
      </Modal>
    );
  }

  const KindIcon = KIND_ICON[asset.kind];
  const kindLabel = t(`workshopAssets.kind.${asset.kind}`, { defaultValue: asset.kind });
  const dimensions = formatDimensions(asset.width, asset.height);
  const origin = asset.origin;
  const createdAt = new Date(asset.created_at).toLocaleString();

  const copyPrompt = async () => {
    if (!origin?.prompt) return;
    try {
      await navigator.clipboard.writeText(origin.prompt);
      message.success(t('workshopAssets.detail.origin.copied', { defaultValue: '已复制提示词' }));
    } catch {
      /* clipboard unavailable — silently ignore */
    }
  };

  const footerActions: { key: string; icon: React.ReactNode; label: string; run: () => void; danger?: boolean }[] = [
    {
      key: 'insert',
      icon: <LinkOne theme='outline' size={14} strokeWidth={3} />,
      label: t('workshopAssets.detail.insert', { defaultValue: '插入画布' }),
      run: () => onInsert(asset),
    },
    {
      key: 'edit',
      icon: <EditTwo theme='outline' size={14} strokeWidth={3} />,
      label: t('workshopAssets.detail.edit', { defaultValue: '编辑' }),
      run: () => onEdit(asset),
    },
    {
      key: 'delete',
      icon: <Delete theme='outline' size={14} strokeWidth={3} />,
      label: t('workshopAssets.detail.delete', { defaultValue: '删除' }),
      run: () => onDelete(asset),
      danger: true,
    },
  ];

  return (
    <Modal
      title={
        <span className='flex items-center gap-8px'>
          <KindIcon theme='outline' size={18} strokeWidth={3} className='text-[rgb(var(--primary-6))]' />
          <span className='truncate'>{asset.title}</span>
        </span>
      }
      visible
      onCancel={onClose}
      footer={null}
      style={{ width: 'min(760px, 92vw)' }}
      autoFocus={false}
      unmountOnExit
    >
      {holder}
      <div className='grid grid-cols-1 gap-16px md:grid-cols-[1.35fr_1fr]'>
        {/* Preview */}
        <PreviewPane asset={asset} />

        {/* Metadata */}
        <div className='flex flex-col gap-14px'>
          <MetaRow label={t('workshopAssets.detail.kind', { defaultValue: '类型' })}>{kindLabel}</MetaRow>

          <MetaRow label={t('workshopAssets.detail.collection', { defaultValue: '集合' })}>
            {asset.collection || (
              <span className='text-[var(--color-text-3)]'>
                {t('workshopAssets.detail.ungrouped', { defaultValue: '未分组' })}
              </span>
            )}
          </MetaRow>

          <MetaRow label={t('workshopAssets.detail.tags', { defaultValue: '标签' })}>
            {asset.tags.length > 0 ? (
              <div className='flex flex-wrap gap-6px'>
                {asset.tags.map((tag) => (
                  <span
                    key={tag}
                    className='inline-flex items-center rounded-6px border border-solid border-[var(--color-border-2)] bg-[var(--color-fill-2)] px-8px py-2px text-11px text-[var(--color-text-2)]'
                  >
                    {tag}
                  </span>
                ))}
              </div>
            ) : (
              <span className='text-[var(--color-text-3)]'>
                {t('workshopAssets.detail.noTags', { defaultValue: '无标签' })}
              </span>
            )}
          </MetaRow>

          {dimensions && (
            <MetaRow label={t('workshopAssets.detail.dimensions', { defaultValue: '尺寸' })}>{dimensions}</MetaRow>
          )}

          {asset.kind !== 'text' && (
            <MetaRow label={t('workshopAssets.detail.size', { defaultValue: '大小' })}>
              {formatBytes(asset.bytes)}
            </MetaRow>
          )}

          <MetaRow label={t('workshopAssets.detail.createdAt', { defaultValue: '创建时间' })}>{createdAt}</MetaRow>

          {/* Provenance */}
          {origin && (origin.prompt || origin.model || origin.provider_id) && (
            <div className='flex flex-col gap-10px rounded-12px border border-solid border-[var(--color-border-2)] bg-[var(--color-fill-1)] p-12px'>
              <span className='text-12px font-600 text-[var(--color-text-2)]'>
                {t('workshopAssets.detail.origin.title', { defaultValue: '生成溯源' })}
              </span>
              {origin.model && (
                <MetaRow label={t('workshopAssets.detail.origin.model', { defaultValue: '模型' })}>
                  {origin.model}
                </MetaRow>
              )}
              {origin.provider_id && (
                <MetaRow label={t('workshopAssets.detail.origin.provider', { defaultValue: '平台' })}>
                  {origin.provider_id}
                </MetaRow>
              )}
              {origin.prompt && (
                <div className='flex flex-col gap-4px'>
                  <div className='flex items-center justify-between gap-8px'>
                    <span className='text-11px font-500 uppercase tracking-wide text-[var(--color-text-4)]'>
                      {t('workshopAssets.detail.origin.prompt', { defaultValue: '提示词' })}
                    </span>
                    <div
                      role='button'
                      tabIndex={0}
                      title={t('workshopAssets.detail.origin.copyPrompt', { defaultValue: '复制提示词' })}
                      onClick={() => void copyPrompt()}
                      onKeyDown={(e) => {
                        if (e.key === 'Enter' || e.key === ' ') {
                          e.preventDefault();
                          void copyPrompt();
                        }
                      }}
                      className='inline-flex items-center gap-4px rounded-6px px-6px py-2px text-11px text-[var(--color-text-3)] cursor-pointer hover:bg-[var(--color-fill-2)] hover:text-[var(--color-text-1)] transition-colors'
                    >
                      <Copy theme='outline' size={12} strokeWidth={3} />
                      {t('workshopAssets.detail.origin.copyPrompt', { defaultValue: '复制提示词' })}
                    </div>
                  </div>
                  <p className='m-0 max-h-140px overflow-y-auto whitespace-pre-wrap break-words text-12px leading-[1.55] text-[var(--color-text-2)]'>
                    {origin.prompt}
                  </p>
                </div>
              )}
            </div>
          )}
        </div>
      </div>

      {/* Footer actions */}
      <div className='mt-18px flex items-center justify-end gap-8px'>
        {footerActions.map((action) => (
          <div
            key={action.key}
            role='button'
            tabIndex={0}
            onClick={action.run}
            onKeyDown={(e) => {
              if (e.key === 'Enter' || e.key === ' ') {
                e.preventDefault();
                action.run();
              }
            }}
            className={[
              'inline-flex items-center gap-6px rounded-9px border border-solid px-14px py-7px text-13px font-500 cursor-pointer transition-colors',
              action.danger
                ? 'border-[rgba(var(--danger-6),0.35)] text-[rgb(var(--danger-6))] bg-transparent hover:bg-[rgba(var(--danger-6),0.08)]'
                : action.key === 'insert'
                  ? 'border-transparent bg-[rgb(var(--primary-6))] text-white hover:bg-[rgb(var(--primary-5))]'
                  : 'border-[var(--color-border-2)] text-[var(--color-text-2)] hover:bg-[var(--color-fill-2)] hover:text-[var(--color-text-1)]',
            ].join(' ')}
          >
            {action.icon}
            {action.label}
          </div>
        ))}
      </div>
    </Modal>
  );
};

export default AssetDetailModal;
