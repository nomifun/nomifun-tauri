/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

/**
 * ImageEditorModal — the full-screen editor shell mounted imperatively by
 * {@link openImageEditor}. Hosts the mode tabs, the shared dark stage + tool,
 * and the cancel / apply footer. Each tool is keyed by mode so switching resets
 * its state and rebinds the imperative apply handle.
 */
import React, { useCallback, useEffect, useRef, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Button, ConfigProvider, Spin } from '@arco-design/web-react';
import enUS from '@arco-design/web-react/es/locale/en-US';
import zhCN from '@arco-design/web-react/es/locale/zh-CN';
import { Check, Close, CuttingOne, GridNine, Paint, ZoomIn } from '@icon-park/react';
import { useArcoMessage } from '@renderer/utils/ui/useArcoMessage';
import type { ImageEditorMode, ImageEditorRequest, ImageEditorResult } from './index';
import { useEditorImage } from './useEditorImage';
import type { ImageToolHandle } from './toolTypes';
import CropTool from './tools/CropTool';
import MaskTool from './tools/MaskTool';
import SplitTool from './tools/SplitTool';
import UpscaleTool from './tools/UpscaleTool';

const SHELL_VARS = {
  '--nfe-stage-bg': '#14161c',
  '--nfe-checker': 'repeating-conic-gradient(#1b1e26 0% 25%, #23262f 0% 50%)',
  '--nfe-toolbar-bg': 'rgba(18,20,26,0.78)',
  '--nfe-stage-text': 'rgba(255,255,255,0.85)',
  '--nfe-stage-text-dim': 'rgba(255,255,255,0.55)',
  '--nfe-stage-border': 'rgba(255,255,255,0.12)',
  '--nfe-stage-hover': 'rgba(255,255,255,0.12)',
  '--nfe-scrim': 'rgba(0,0,0,0.55)',
  '--nfe-seam': 'rgba(255,64,64,0.30)',
  '--nfe-panel-bg': 'var(--color-bg-2)',
  '--nfe-panel-border': 'var(--color-border-2)',
  '--nfe-inset-bg': 'var(--color-fill-2)',
  '--nfe-text-1': 'var(--color-text-1)',
  '--nfe-text-2': 'var(--color-text-2)',
  '--nfe-text-3': 'var(--color-text-3)',
  '--nfe-accent': 'rgb(var(--primary-6))',
  '--nfe-accent-soft': 'rgba(var(--primary-6),0.14)',
} as React.CSSProperties;

interface ModeTab {
  mode: ImageEditorMode;
  label: string;
  icon: React.ReactNode;
}

export interface ImageEditorModalProps {
  req: ImageEditorRequest;
  onClose: (result: ImageEditorResult | null) => void;
}

const ImageEditorModal: React.FC<ImageEditorModalProps> = ({ req, onClose }) => {
  const { t, i18n } = useTranslation();
  const rootRef = useRef<HTMLDivElement>(null);
  const [message, messageHolder] = useArcoMessage();
  const [mode, setMode] = useState<ImageEditorMode>(req.mode);
  const [canApply, setCanApply] = useState(req.mode !== 'mask');
  const [busy, setBusy] = useState(false);
  const toolRef = useRef<ImageToolHandle>(null);

  const imageState = useEditorImage(req.src, req.naturalWidth, req.naturalHeight);

  // Esc cancels the whole editor.
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key === 'Escape') {
        e.stopPropagation();
        onClose(null);
      }
    };
    window.addEventListener('keydown', onKey, true);
    return () => window.removeEventListener('keydown', onKey, true);
  }, [onClose]);

  const switchMode = useCallback((next: ImageEditorMode) => {
    setMode(next);
    setCanApply(next !== 'mask');
  }, []);

  const handleApply = useCallback(async () => {
    const handle = toolRef.current;
    if (!handle || busy || !canApply) return;
    setBusy(true);
    try {
      const result = await handle.apply();
      if (result) {
        onClose(result);
        return; // unmount imminent — skip the busy reset below
      }
    } catch (err) {
      message.error(
        `${t('workshopEditor.errors.exportFailed', { defaultValue: '处理失败' })}: ${err instanceof Error ? err.message : String(err)}`
      );
    }
    setBusy(false);
  }, [busy, canApply, onClose, message, t]);

  const tabs: ModeTab[] = [
    { mode: 'crop', label: t('workshopEditor.tabs.crop', { defaultValue: '裁剪' }), icon: <CuttingOne theme='outline' size={16} /> },
    { mode: 'mask', label: t('workshopEditor.tabs.mask', { defaultValue: '遮罩' }), icon: <Paint theme='outline' size={16} /> },
    { mode: 'split', label: t('workshopEditor.tabs.split', { defaultValue: '切分' }), icon: <GridNine theme='outline' size={16} /> },
    { mode: 'upscale', label: t('workshopEditor.tabs.upscale', { defaultValue: '放大' }), icon: <ZoomIn theme='outline' size={16} /> },
  ];

  const renderTool = () => {
    if (imageState.status !== 'ready') return null;
    const image = imageState.image;
    const props = { image, onCanApplyChange: setCanApply };
    switch (mode) {
      case 'crop':
        return <CropTool key='crop' ref={toolRef} {...props} />;
      case 'mask':
        return <MaskTool key='mask' ref={toolRef} {...props} />;
      case 'split':
        return <SplitTool key='split' ref={toolRef} {...props} />;
      case 'upscale':
        return <UpscaleTool key='upscale' ref={toolRef} {...props} />;
      default:
        return null;
    }
  };

  return (
    <ConfigProvider
      locale={i18n.language === 'zh-CN' ? zhCN : enUS}
      getPopupContainer={() => rootRef.current ?? document.body}
    >
      <div ref={rootRef} className='fixed inset-0 flex flex-col' style={{ ...SHELL_VARS, zIndex: 1200, background: 'var(--color-bg-1)' }}>
        {messageHolder}

        {/* Header: title · mode tabs · close */}
        <header
          className='flex h-56px shrink-0 items-center gap-16px px-16px'
          style={{ borderBottom: '1px solid var(--nfe-panel-border)', background: 'var(--nfe-panel-bg)' }}
        >
          <span className='text-15px font-700' style={{ color: 'var(--nfe-text-1)' }}>
            {t('workshopEditor.title', { defaultValue: '图片编辑器' })}
          </span>
          <div className='mx-2px h-20px w-1px' style={{ background: 'var(--nfe-panel-border)' }} />
          <nav className='flex items-center gap-4px'>
            {tabs.map((tab) => {
              const active = tab.mode === mode;
              return (
                <div
                  key={tab.mode}
                  role='button'
                  tabIndex={0}
                  onClick={() => switchMode(tab.mode)}
                  onKeyDown={(e) => {
                    if (e.key === 'Enter' || e.key === ' ') {
                      e.preventDefault();
                      switchMode(tab.mode);
                    }
                  }}
                  className='flex h-34px cursor-pointer items-center gap-7px rounded-9px px-13px text-13px transition-all'
                  style={{
                    background: active ? 'var(--nfe-accent-soft)' : 'transparent',
                    color: active ? 'var(--nfe-accent)' : 'var(--nfe-text-2)',
                    fontWeight: active ? 600 : 400,
                  }}
                >
                  {tab.icon}
                  {tab.label}
                </div>
              );
            })}
          </nav>
          <div className='flex-1' />
          <div
            role='button'
            tabIndex={0}
            title={t('workshopEditor.common.close', { defaultValue: '关闭' })}
            onClick={() => onClose(null)}
            onKeyDown={(e) => {
              if (e.key === 'Enter' || e.key === ' ') {
                e.preventDefault();
                onClose(null);
              }
            }}
            className='grid h-32px w-32px cursor-pointer place-items-center rounded-8px transition-colors'
            style={{ color: 'var(--nfe-text-2)' }}
          >
            <Close theme='outline' size={18} />
          </div>
        </header>

        {/* Body */}
        <div className='relative flex min-h-0 flex-1'>
          {imageState.status === 'loading' && <CenterState><Spin /></CenterState>}
          {imageState.status === 'error' && (
            <CenterState>
              <span style={{ color: 'var(--nfe-text-3)' }}>
                {t('workshopEditor.errors.loadFailed', { defaultValue: '图片加载失败' })}
              </span>
            </CenterState>
          )}
          {imageState.status === 'ready' && renderTool()}

          {busy && (
            <div className='absolute inset-0 z-10 flex flex-col items-center justify-center gap-12px' style={{ background: 'rgba(10,11,15,0.55)', backdropFilter: 'blur(2px)' }}>
              <Spin size={28} />
              <span className='text-13px' style={{ color: 'var(--nfe-stage-text)' }}>
                {t('workshopEditor.common.applying', { defaultValue: '处理中…' })}
              </span>
            </div>
          )}
        </div>

        {/* Footer */}
        <footer
          className='flex h-60px shrink-0 items-center justify-end gap-10px px-16px'
          style={{ borderTop: '1px solid var(--nfe-panel-border)', background: 'var(--nfe-panel-bg)' }}
        >
          <Button onClick={() => onClose(null)} disabled={busy}>
            {t('workshopEditor.common.cancel', { defaultValue: '取消' })}
          </Button>
          <Button type='primary' loading={busy} disabled={!canApply || imageState.status !== 'ready'} onClick={() => void handleApply()}>
            <span className='inline-flex items-center gap-6px'>
              <Check theme='outline' size={15} />
              {t('workshopEditor.common.apply', { defaultValue: '应用' })}
            </span>
          </Button>
        </footer>
      </div>
    </ConfigProvider>
  );
};

const CenterState: React.FC<React.PropsWithChildren> = ({ children }) => (
  <div className='flex h-full w-full items-center justify-center' style={{ background: 'var(--nfe-stage-bg)' }}>
    {children}
  </div>
);

export default ImageEditorModal;
