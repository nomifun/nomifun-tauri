/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

/**
 * AssetThumb — the visual preview inside a grid card: an image thumbnail, a
 * video poster (with a play glyph), or a graceful icon fallback while loading /
 * when no binary preview exists. Binaries always flow through
 * `useWorkshopObjectUrl` (never a bare `<img src>`).
 */

import React from 'react';
import { ImageFiles, PlayOne, VideoTwo } from '@icon-park/react';

import type { WorkshopAsset } from '../types';
import { useWorkshopObjectUrl } from './useWorkshopMedia';

interface AssetThumbProps {
  asset: WorkshopAsset;
  /** When set, only load once the card is on-screen. */
  enabled?: boolean;
}

const AssetThumb: React.FC<AssetThumbProps> = ({ asset, enabled = true }) => {
  // Prefer the server thumbnail when present; otherwise fall back to the full
  // binary for images (small enough) and to an icon poster for videos.
  const wantThumb = Boolean(asset.thumb_url);
  const canPreview = asset.kind === 'image' || (asset.kind === 'video' && wantThumb);
  const { url, status } = useWorkshopObjectUrl(canPreview ? asset.id : null, {
    thumb: wantThumb,
    enabled: enabled && canPreview,
  });

  const isVideo = asset.kind === 'video';

  return (
    <div className='relative h-full w-full overflow-hidden bg-[var(--color-fill-1)]'>
      {url ? (
        <img
          src={url}
          alt={asset.title}
          draggable={false}
          className='absolute inset-0 h-full w-full select-none object-cover'
        />
      ) : (
        <div className='absolute inset-0 grid place-items-center text-[var(--color-text-4)]'>
          {status === 'loading' ? (
            <span className='h-22px w-22px animate-pulse rounded-full bg-[var(--color-fill-3)]' />
          ) : isVideo ? (
            <VideoTwo theme='outline' size={26} strokeWidth={3} />
          ) : (
            <ImageFiles theme='outline' size={26} strokeWidth={3} />
          )}
        </div>
      )}

      {/* Video play affordance */}
      {isVideo && (
        <div className='pointer-events-none absolute inset-0 grid place-items-center'>
          <span className='grid h-32px w-32px place-items-center rounded-full bg-[rgba(0,0,0,0.42)] text-white backdrop-blur-sm'>
            <PlayOne theme='filled' size={16} />
          </span>
        </div>
      )}
    </div>
  );
};

export default AssetThumb;
