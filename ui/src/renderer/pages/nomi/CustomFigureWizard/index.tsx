/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import React, { useCallback, useEffect, useRef, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Alert, Button, Input, Modal, Progress, Slider, Spin, Steps } from '@arco-design/web-react';
import { IconUpload } from '@arco-design/web-react/icon';
import classNames from 'classnames';
import { ipcBridge } from '@/common';
import type { IFigureMeta } from '@/common/adapter/ipcBridge';
import { uploadFileViaHttp } from '@renderer/services/FileService';
import { ensureMattingModel } from '@renderer/services/matting/modelCache';
import type { MatteMethod, MatteRequest, MatteResponse } from '@renderer/services/matting/matting.worker';
import FrameStep, { clampHeadBox, type HeadBox } from './FrameStep';

/**
 * CustomFigureWizard — DIY figure three-step modal: pick (file/drag/paste) →
 * matte (Worker ML-first cutout with heuristic redo) → frame (head-box + size
 * tier + name), finishing by uploading the cutout to the shared **figure
 * library** (`POST /api/companion/figures`). Decoupled from any companion: `onDone` hands
 * back the new figure so the caller can select it for a companion (or just keep it
 * in the library).
 */

type Step = 'pick' | 'matte' | 'frame';

interface MatteOutcome {
  blob: Blob;
  url: string;
  aspect: number;
  method: MatteMethod;
}

const ACCEPT = '.jpg,.jpeg,.png,.webp,.gif';
const ACCEPT_EXT = ['jpg', 'jpeg', 'png', 'webp', 'gif'];
const ACCEPT_MIME = ['image/jpeg', 'image/png', 'image/webp', 'image/gif'];
/** Decode clamp: sources larger than 4096×4096 are downscaled before matting. */
const MAX_SOURCE_EDGE = 4096;
/** "Use original" path mirrors the worker's output clamp. */
const MAX_OUTPUT_EDGE = 2048;
const DEFAULT_TOLERANCE = 11;
/** Below this cutout aspect (w/h) the figure is too narrow — warn, don't block. */
const NARROW_ASPECT = 0.3;

const isAcceptedImage = (f: File): boolean => {
  if (ACCEPT_MIME.includes(f.type)) return true;
  const ext = f.name.split('.').pop()?.toLowerCase() ?? '';
  return ACCEPT_EXT.includes(ext);
};

/** Decode a file into an ImageBitmap, downscaling so both edges are ≤ maxEdge. */
async function createBitmapClamped(file: File, maxEdge: number): Promise<ImageBitmap> {
  const probe = await createImageBitmap(file);
  if (probe.width <= maxEdge && probe.height <= maxEdge) return probe;
  const scale = maxEdge / Math.max(probe.width, probe.height);
  const resized = await createImageBitmap(probe, {
    resizeWidth: Math.max(1, Math.round(probe.width * scale)),
    resizeHeight: Math.max(1, Math.round(probe.height * scale)),
    resizeQuality: 'high',
  });
  probe.close();
  return resized;
}

/** "Use original" — encode the unmatted image (long edge ≤2048) as the figure. */
async function encodeOriginal(file: File): Promise<{ blob: Blob; aspect: number }> {
  const bitmap = await createBitmapClamped(file, MAX_OUTPUT_EDGE);
  const canvas = document.createElement('canvas');
  canvas.width = bitmap.width;
  canvas.height = bitmap.height;
  const ctx = canvas.getContext('2d');
  if (!ctx) throw new Error('canvas 2d context unavailable');
  ctx.drawImage(bitmap, 0, 0);
  const aspect = bitmap.width / bitmap.height;
  bitmap.close();
  // Safari can't encode webp — it silently falls back to png, which the
  // backend ingest accepts too (magic-sniffed, extension stays .webp).
  const blob = await new Promise<Blob | null>((resolve) => canvas.toBlob(resolve, 'image/webp', 0.9));
  if (!blob) throw new Error('image encode failed');
  return { blob, aspect };
}

const defaultHeadBoxFor = (aspect: number): HeadBox => clampHeadBox((1 - 0.3) / 2, 0, 0.3, 0.3 * aspect);

const round4 = (v: number): number => Math.round(v * 10000) / 10000;

export interface CustomFigureWizardProps {
  open: boolean;
  onClose: () => void;
  /** A new library figure was created (uploaded to /api/companion/figures). */
  onDone: (figure: IFigureMeta) => void;
}

const CustomFigureWizard: React.FC<CustomFigureWizardProps> = ({ open, onClose, onDone }) => {
  const { t } = useTranslation();

  const [step, setStep] = useState<Step>('pick');
  const [pickError, setPickError] = useState<string | null>(null);
  const [dragging, setDragging] = useState(false);
  const [progress, setProgress] = useState<{ phase: 'download' | 'infer' | 'process'; loaded?: number; total?: number } | null>(null);
  const [matteFailed, setMatteFailed] = useState(false);
  /** ML 抠图模型经后端代理也拉不到（离线/上游全挂）——已降级快速抠图，提示用户。 */
  const [modelUnavailable, setModelUnavailable] = useState(false);
  const [result, setResult] = useState<MatteOutcome | null>(null);
  const [tolerance, setTolerance] = useState(DEFAULT_TOLERANCE);
  const [headBox, setHeadBox] = useState<HeadBox>({ x: 0.35, y: 0, w: 0.3, h: 0.3 });
  // Default to the smallest tier: a desktop pet should start compact and not
  // hog the desktop; the user can size up to 中/大 in the picker if they want.
  const [sizeTier, setSizeTier] = useState<'s' | 'm' | 'l'>('s');
  const [name, setName] = useState('');
  const [frameError, setFrameError] = useState<string | null>(null);
  const [submitting, setSubmitting] = useState(false);

  const fileRef = useRef<File | null>(null);
  const inputRef = useRef<HTMLInputElement | null>(null);
  const workerRef = useRef<Worker | null>(null);
  /** Monotonic request id — stale worker responses and aborted decodes are ignored. */
  const reqIdRef = useRef(0);

  /** Swap the matte outcome, revoking the previous preview object URL. */
  const swapResult = useCallback((next: MatteOutcome | null) => {
    setResult((prev) => {
      if (prev && prev.url !== next?.url) URL.revokeObjectURL(prev.url);
      return next;
    });
  }, []);

  const failBackToPick = useCallback(
    (detail: unknown) => {
      setStep('pick');
      setProgress(null);
      setPickError(`${t('nomi.customFigure.errFailed')}${detail ? `: ${String(detail)}` : ''}`);
    },
    [t]
  );

  const handleWorkerMessage = useCallback(
    (msg: MatteResponse) => {
      if (msg.id !== reqIdRef.current) return; // stale run
      if (msg.type === 'progress') {
        setProgress({ phase: msg.phase, loaded: msg.loaded, total: msg.total });
        return;
      }
      if (msg.type === 'done') {
        setProgress(null);
        const url = URL.createObjectURL(msg.blob);
        swapResult({ blob: msg.blob, url, aspect: msg.aspect, method: msg.method });
        // The matting worker suggests a square box ({x,y,w}); seed h as the
        // square height (w·aspect) — the user can then stretch it to any rectangle.
        setHeadBox(clampHeadBox(msg.headBox.x, msg.headBox.y, msg.headBox.w, msg.headBox.w * msg.aspect));
        // Transparent sources skip the matte review entirely.
        if (msg.method === 'passthrough') setStep('frame');
        return;
      }
      // error
      if (msg.message === 'MATTE_FAILED') {
        setProgress(null);
        setMatteFailed(true); // stay on the matte step: re-matte or pick another image
      } else {
        failBackToPick(msg.message);
      }
    },
    [failBackToPick, swapResult]
  );
  const handleWorkerMessageRef = useRef(handleWorkerMessage);
  handleWorkerMessageRef.current = handleWorkerMessage;

  const ensureWorker = useCallback((): Worker => {
    if (!workerRef.current) {
      const worker = new Worker(new URL('../../../services/matting/matting.worker.ts', import.meta.url), {
        type: 'module',
      });
      worker.onmessage = (ev: MessageEvent<MatteResponse>) => handleWorkerMessageRef.current(ev.data);
      worker.onerror = (ev) => {
        workerRef.current?.terminate();
        workerRef.current = null;
        failBackToPick(ev.message || 'worker error');
      };
      workerRef.current = worker;
    }
    return workerRef.current;
  }, [failBackToPick]);

  const runMatte = useCallback(
    async (file: File, mode: 'auto' | 'heuristic', tol: number) => {
      setStep('matte');
      setMatteFailed(false);
      swapResult(null);
      setProgress({ phase: 'process' });
      const id = ++reqIdRef.current;
      try {
        // ML 模式：先确保模型在 Cache Storage（主线程经后端代理下载，带进度、无硬
        // 超时）。拿不到则降级快速抠图——worker 只读缓存，绝不在 worker 里下载。
        let effectiveMode = mode;
        if (mode === 'auto') {
          try {
            await ensureMattingModel((loaded, total) => {
              if (id === reqIdRef.current) setProgress({ phase: 'download', loaded, total });
            });
          } catch {
            effectiveMode = 'heuristic';
            setModelUnavailable(true);
          }
          if (id !== reqIdRef.current) return;
          setProgress({ phase: 'process' });
        }
        const bitmap = await createBitmapClamped(file, MAX_SOURCE_EDGE);
        if (id !== reqIdRef.current) {
          bitmap.close();
          return;
        }
        const req: MatteRequest = { id, type: 'matte', bitmap, mode: effectiveMode, tolerance: tol };
        ensureWorker().postMessage(req, [bitmap]);
      } catch (err) {
        if (id === reqIdRef.current) failBackToPick(err);
      }
    },
    [ensureWorker, failBackToPick, swapResult]
  );

  const handleFile = useCallback(
    (file: File) => {
      if (!isAcceptedImage(file)) {
        setPickError(t('nomi.customFigure.errFailed'));
        return;
      }
      setPickError(null);
      fileRef.current = file;
      void runMatte(file, 'auto', tolerance);
    },
    [runMatte, t, tolerance]
  );

  const redoHeuristic = useCallback(() => {
    const file = fileRef.current;
    if (file) void runMatte(file, 'heuristic', tolerance);
  }, [runMatte, tolerance]);

  const useOriginal = useCallback(async () => {
    const file = fileRef.current;
    if (!file) return;
    setMatteFailed(false);
    setProgress({ phase: 'process' });
    const id = ++reqIdRef.current;
    try {
      const { blob, aspect } = await encodeOriginal(file);
      if (id !== reqIdRef.current) return;
      setProgress(null);
      swapResult({ blob, url: URL.createObjectURL(blob), aspect, method: 'passthrough' });
      setHeadBox(defaultHeadBoxFor(aspect));
      setStep('frame');
    } catch (err) {
      if (id === reqIdRef.current) failBackToPick(err);
    }
  }, [failBackToPick, swapResult]);

  const backToPick = useCallback(() => {
    reqIdRef.current++;
    swapResult(null);
    setProgress(null);
    setMatteFailed(false);
    setModelUnavailable(false);
    setPickError(null);
    setStep('pick');
  }, [swapResult]);

  /** Confirm: upload webp → create a reusable library figure. Failure keeps the frame step retryable. */
  const confirm = useCallback(async () => {
    if (!result || submitting) return;
    setSubmitting(true);
    setFrameError(null);
    try {
      const sourcePath = await uploadFileViaHttp(new File([result.blob], 'figure.webp', { type: result.blob.type || 'image/webp' }));
      const figure = await ipcBridge.companion.createFigure.invoke({
        source_path: sourcePath,
        name: name.trim(),
        aspect: round4(result.aspect),
        head_box: { x: round4(headBox.x), y: round4(headBox.y), w: round4(headBox.w), h: round4(headBox.h) },
        size_tier: sizeTier,
      });
      onDone(figure);
    } catch (err) {
      setFrameError(
        err instanceof Error && err.message === 'FILE_TOO_LARGE'
          ? t('nomi.customFigure.errTooLarge')
          : `${t('nomi.customFigure.errFailed')}: ${String(err)}`
      );
    } finally {
      setSubmitting(false);
    }
  }, [headBox, name, onDone, result, sizeTier, submitting, t]);

  // Reset to a clean pick step on every open.
  useEffect(() => {
    if (!open) return;
    setStep('pick');
    setPickError(null);
    setDragging(false);
    setProgress(null);
    setMatteFailed(false);
    setModelUnavailable(false);
    swapResult(null);
    setTolerance(DEFAULT_TOLERANCE);
    setSizeTier('m');
    setName('');
    setFrameError(null);
    setSubmitting(false);
    fileRef.current = null;
    // Prewarm the matting model the moment the wizard opens, so it's cached by
    // the time the user finishes picking an image (download runs on the main
    // thread via the backend proxy; failure is non-fatal — matting degrades).
    void ensureMattingModel().catch(() => {});
  }, [open, swapResult]);

  // Closing invalidates in-flight work and frees the worker (wasm memory).
  useEffect(() => {
    if (open) return;
    reqIdRef.current++;
    workerRef.current?.terminate();
    workerRef.current = null;
    // Free the matte blob + object URL right away — the wizard component stays
    // mounted in its host, so waiting for the next open-reset leaks MBs.
    swapResult(null);
  }, [open, swapResult]);
  useEffect(
    () => () => {
      workerRef.current?.terminate();
      workerRef.current = null;
      setResult((prev) => {
        if (prev) URL.revokeObjectURL(prev.url);
        return null;
      });
    },
    []
  );

  // Paste capture while the pick step is up — capture phase + stopImmediatePropagation
  // keeps the global PasteService (chat sendbox uploads) from also consuming it.
  useEffect(() => {
    if (!open || step !== 'pick') return;
    const onPaste = (e: ClipboardEvent) => {
      const file = e.clipboardData?.files?.[0];
      if (!file || !isAcceptedImage(file)) return;
      e.preventDefault();
      e.stopImmediatePropagation();
      handleFile(file);
    };
    document.addEventListener('paste', onPaste, true);
    return () => document.removeEventListener('paste', onPaste, true);
  }, [open, step, handleFile]);

  const stepIndex = step === 'pick' ? 1 : step === 'matte' ? 2 : 3;
  const progressText =
    progress?.phase === 'download'
      ? t('nomi.customFigure.downloadingModel')
      : progress?.phase === 'infer'
        ? t('nomi.customFigure.matting')
        : t('nomi.customFigure.processing');
  const downloadPercent =
    progress?.phase === 'download' && progress.total
      ? Math.min(100, Math.round(((progress.loaded ?? 0) / progress.total) * 100))
      : null;

  const renderPick = (): React.ReactNode => (
    <div className='flex flex-col gap-12px'>
      <div
        onClick={() => inputRef.current?.click()}
        onDragOver={(e) => {
          e.preventDefault();
          setDragging(true);
        }}
        onDragLeave={() => setDragging(false)}
        onDrop={(e) => {
          e.preventDefault();
          setDragging(false);
          const file = e.dataTransfer?.files?.[0];
          if (file) handleFile(file);
        }}
        className={classNames(
          'group flex flex-col items-center justify-center gap-12px h-260px rd-16px cursor-pointer transition-all duration-200 border-2 border-dashed',
          dragging
            ? 'border-[var(--color-primary)] bg-primary-1 scale-[1.01]'
            : 'border-[var(--color-border-2)] bg-gradient-to-b from-[var(--color-fill-1)] to-[var(--color-fill-2)] hover:border-[var(--color-primary)]'
        )}
      >
        <span
          className={classNames(
            'flex items-center justify-center w-64px h-64px rd-full text-30px transition-all duration-200',
            dragging ? 'bg-[var(--color-primary)] text-white scale-110' : 'bg-primary-1 text-primary-6 group-hover:scale-105'
          )}
        >
          <IconUpload />
        </span>
        <div className='flex flex-col items-center gap-3px'>
          <span className='text-14px font-600 text-t-primary'>{t('nomi.customFigure.dropHint')}</span>
          <span className='text-12px text-t-tertiary'>{t('nomi.customFigure.pasteHint')}</span>
        </div>
      </div>
      <input
        ref={inputRef}
        type='file'
        accept={ACCEPT}
        className='hidden'
        onChange={(e) => {
          const file = e.target.files?.[0];
          if (file) handleFile(file);
          e.target.value = '';
        }}
      />
      {pickError && <Alert type='error' content={pickError} />}
      <span className='text-12px text-t-tertiary text-center'>{t('nomi.customFigure.copyrightHint')}</span>
    </div>
  );

  const renderMatte = (): React.ReactNode => (
    <div className='flex flex-col gap-16px'>
      {progress && (
        <div className='flex flex-col items-center gap-12px py-48px rd-16px bg-fill-1'>
          <Spin size={28} />
          <span className='text-13px font-500 text-t-secondary'>{progressText}</span>
          {downloadPercent != null && (
            <div className='flex flex-col items-center gap-4px w-280px'>
              <Progress percent={downloadPercent} color='var(--color-primary)' trailColor='var(--color-fill-3)' />
              <span className='text-11px text-t-tertiary'>{t('nomi.customFigure.downloadingModel')}</span>
            </div>
          )}
        </div>
      )}
      {!progress && matteFailed && <Alert type='error' content={t('nomi.customFigure.errMatteFailed')} />}
      {!progress && modelUnavailable && <Alert type='warning' content={t('nomi.customFigure.modelUnavailable')} />}
      {!progress && result && (
        <div className='flex gap-16px justify-center flex-wrap'>
          {/* light / dark twin previews — judge the cutout edge on both grounds */}
          <div className='flex flex-col items-center gap-6px'>
            <div className='flex items-center justify-center w-200px h-200px rd-12px bg-[#f4f4f7] border border-solid border-[var(--color-border-2)] overflow-hidden shadow-[0_4px_16px_rgba(0,0,0,0.06)]'>
              <img src={result.url} alt='' draggable={false} className='max-w-[90%] max-h-[90%] object-contain' />
            </div>
            <span className='text-11px text-t-tertiary'>{t('nomi.customFigure.previewLight')}</span>
          </div>
          <div className='flex flex-col items-center gap-6px'>
            <div className='flex items-center justify-center w-200px h-200px rd-12px bg-[#26262b] overflow-hidden shadow-[0_4px_16px_rgba(0,0,0,0.18)]'>
              <img src={result.url} alt='' draggable={false} className='max-w-[90%] max-h-[90%] object-contain' />
            </div>
            <span className='text-11px text-t-tertiary'>{t('nomi.customFigure.previewDark')}</span>
          </div>
        </div>
      )}
      {!progress && result && (
        <div className='flex items-center gap-12px flex-wrap rd-12px bg-fill-2 px-14px py-10px'>
          <span className='text-13px text-t-secondary shrink-0'>{t('nomi.customFigure.tolerance')}</span>
          <Slider min={4} max={32} value={tolerance} onChange={(v) => setTolerance(v as number)} style={{ width: 150 }} />
          <div className='flex-1' />
          <Button size='small' onClick={redoHeuristic}>
            {t('nomi.customFigure.redo')}
          </Button>
          <Button size='small' onClick={() => void useOriginal()}>
            {t('nomi.customFigure.useOriginal')}
          </Button>
        </div>
      )}
    </div>
  );

  const renderFrame = (): React.ReactNode =>
    result && (
      <div className='flex flex-col gap-14px'>
        {result.aspect < NARROW_ASPECT && <Alert type='warning' content={t('nomi.customFigure.narrowWarn')} />}
        {frameError && <Alert type='error' content={frameError} />}
        <FrameStep
          imageUrl={result.url}
          aspect={result.aspect}
          headBox={headBox}
          onHeadBoxChange={setHeadBox}
          sizeTier={sizeTier}
          onSizeTierChange={setSizeTier}
        />
        <div className='flex items-center gap-10px rd-12px bg-fill-2 px-14px py-10px'>
          <span className='text-13px text-t-secondary shrink-0'>{t('nomi.customFigure.nameLabel')}</span>
          <Input
            value={name}
            onChange={setName}
            placeholder={t('nomi.customFigure.namePlaceholder')}
            maxLength={40}
            allowClear
            style={{ flex: 1 }}
          />
        </div>
      </div>
    );

  /** Unified footer: compact right-aligned modal actions. */
  const renderFooter = (): React.ReactNode => (
    <div className='figure-modal-footer flex items-center justify-end gap-8px pt-10px'>
      <Button type='text' status='danger' onClick={onClose} disabled={submitting}>
        {t('nomi.customFigure.discard')}
      </Button>
      {step === 'matte' && (
        <>
          <Button onClick={backToPick} disabled={!!progress}>
            {t('nomi.customFigure.repick')}
          </Button>
          <Button type='primary' disabled={!result || !!progress} onClick={() => setStep('frame')}>
            {t('nomi.customFigure.next')}
          </Button>
        </>
      )}
      {step === 'frame' && (
        <>
          <Button onClick={backToPick} disabled={submitting}>
            {t('nomi.customFigure.repick')}
          </Button>
          <Button type='primary' loading={submitting} onClick={() => void confirm()}>
            {submitting ? t('nomi.customFigure.uploading') : t('nomi.customFigure.done')}
          </Button>
        </>
      )}
    </div>
  );

  return (
    <Modal
      title={t('nomi.customFigure.wizardTitle')}
      visible={open}
      onCancel={onClose}
      footer={null}
      maskClosable={false}
      unmountOnExit
      style={{ width: 680 }}
    >
      <div className='flex flex-col gap-18px'>
        <Steps current={stepIndex} size='small'>
          <Steps.Step title={t('nomi.customFigure.stepPick')} />
          <Steps.Step title={t('nomi.customFigure.stepMatte')} />
          <Steps.Step title={t('nomi.customFigure.stepFrame')} />
        </Steps>
        {step === 'pick' && renderPick()}
        {step === 'matte' && renderMatte()}
        {step === 'frame' && renderFrame()}
        {renderFooter()}
      </div>
    </Modal>
  );
};

export default CustomFigureWizard;
