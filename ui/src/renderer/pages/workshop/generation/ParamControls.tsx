/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

/**
 * Per-mode parameter panel for the generation card. Image mode exposes size
 * presets + custom W×H + count + quality; video mode exposes duration,
 * resolution, aspect, audio, and watermark; text mode has no extra parameters.
 * Compact, theme-variable-driven controls — no Arco chrome, so the card reads as
 * one surface.
 */

import React from 'react';
import { useTranslation } from 'react-i18next';
import type { GenMode, ModelOption } from './genTypes';
import {
  IMAGE_COUNT_MAX,
  IMAGE_COUNT_MIN,
  IMAGE_QUALITIES,
  IMAGE_SIZE_PRESETS,
  VIDEO_ASPECTS,
  VIDEO_RESOLUTIONS,
  VIDEO_SECONDS_MAX,
  VIDEO_SECONDS_MIN,
  readImageParams,
  readVideoParams,
} from './genConstants';
import {
  isLocalZImageModel,
  localZImageSizePresets,
  LOCAL_Z_IMAGE_DIMENSION_MAX,
  LOCAL_Z_IMAGE_DIMENSION_MIN,
  LOCAL_Z_IMAGE_DIMENSION_STEP,
  normalizeImageParamsForModel,
  normalizeLocalZImageDimension,
} from './localZImage';

// ─── Reusable compact widgets ───────────────────────────────────────────────────

const FieldRow: React.FC<{ label: string; children: React.ReactNode }> = ({ label, children }) => (
  <div className='flex min-w-0 flex-col gap-5px'>
    <span className='text-10px font-600 uppercase tracking-wide text-[var(--color-text-3)]'>{label}</span>
    {children}
  </div>
);

interface PillOption {
  key: string;
  label: string;
}

const PillGroup: React.FC<{ options: PillOption[]; value: string; onSelect: (key: string) => void; fill?: boolean }> = ({
  options,
  value,
  onSelect,
  fill = false,
}) => (
  <div className={fill ? 'flex w-full flex-wrap gap-4px' : 'flex flex-wrap gap-4px'}>
    {options.map((opt) => {
      const active = opt.key === value;
      return (
        <div
          key={opt.key}
          role='button'
          tabIndex={0}
          onClick={() => onSelect(opt.key)}
          onKeyDown={(e) => (e.key === 'Enter' || e.key === ' ') && onSelect(opt.key)}
          className={[
            'nodrag rounded-7px border border-solid px-8px py-4px text-11px font-500 cursor-pointer transition-colors select-none text-center',
            fill ? 'box-border flex-1 min-w-max max-w-full' : '',
            active
              ? 'border-[rgb(var(--primary-6))] bg-[rgba(var(--primary-6),0.12)] text-[rgb(var(--primary-6))]'
              : 'border-[var(--color-border-2)] bg-[var(--color-fill-1)] text-[var(--color-text-2)] hover:border-[var(--color-border-3)]',
          ].join(' ')}
        >
          {opt.label}
        </div>
      );
    })}
  </div>
);

const Stepper: React.FC<{ value: number; min: number; max: number; suffix?: string; onChange: (v: number) => void }> = ({
  value,
  min,
  max,
  suffix,
  onChange,
}) => {
  const step = (delta: number): void => onChange(Math.min(max, Math.max(min, value + delta)));
  const btn = 'grid h-24px w-24px place-items-center rounded-6px cursor-pointer select-none text-[var(--color-text-2)] hover:bg-[var(--color-fill-2)] hover:text-[var(--color-text-1)] transition-colors';
  return (
    <div className='nodrag inline-flex items-center gap-6px rounded-8px border border-solid border-[var(--color-border-2)] bg-[var(--color-fill-1)] px-4px py-2px'>
      <div role='button' tabIndex={0} onClick={() => step(-1)} onKeyDown={(e) => (e.key === 'Enter' || e.key === ' ') && step(-1)} className={btn}>
        −
      </div>
      <span className='min-w-32px text-center text-12px font-600 tabular-nums text-[var(--color-text-1)]'>
        {value}
        {suffix ?? ''}
      </span>
      <div role='button' tabIndex={0} onClick={() => step(1)} onKeyDown={(e) => (e.key === 'Enter' || e.key === ' ') && step(1)} className={btn}>
        +
      </div>
    </div>
  );
};

const Toggle: React.FC<{ label: string; checked: boolean; onChange: (v: boolean) => void }> = ({ label, checked, onChange }) => (
  <div
    role='switch'
    aria-checked={checked}
    tabIndex={0}
    onClick={() => onChange(!checked)}
    onKeyDown={(e) => (e.key === 'Enter' || e.key === ' ') && onChange(!checked)}
    className='nodrag flex items-center justify-between gap-8px rounded-8px px-2px py-2px cursor-pointer select-none'
  >
    <span className='text-11px text-[var(--color-text-2)]'>{label}</span>
    <span
      className={[
        'relative h-16px w-28px shrink-0 rounded-full transition-colors',
        checked ? 'bg-[rgb(var(--primary-6))]' : 'bg-[var(--color-fill-3)]',
      ].join(' ')}
    >
      <span
        className='absolute top-2px h-12px w-12px rounded-full bg-white transition-all'
        style={{ left: checked ? 14 : 2 }}
      />
    </span>
  </div>
);

const NumberBox: React.FC<{
  value: number;
  min?: number;
  max?: number;
  step?: number;
  normalize?: (value: number) => number;
  onChange: (v: number) => void;
}> = ({ value, min, max, step, normalize, onChange }) => (
  <input
    type='number'
    value={value}
    min={min}
    max={max}
    step={step}
    onChange={(e) => {
      const n = Number(e.target.value);
      if (Number.isFinite(n)) onChange(Math.round(n));
    }}
    onBlur={() => {
      if (normalize) onChange(normalize(value));
    }}
    onKeyDown={(e) => e.stopPropagation()}
    className='nodrag w-full min-w-0 box-border rounded-7px border border-solid border-[var(--color-border-2)] bg-[var(--color-fill-1)] px-8px py-5px text-12px text-[var(--color-text-1)] outline-none focus:border-[rgb(var(--primary-6))]'
  />
);

// ─── Panel ───────────────────────────────────────────────────────────────────

export interface ParamControlsProps {
  mode: GenMode;
  model?: ModelOption | null;
  params: Record<string, unknown>;
  onChange: (patch: Record<string, unknown>) => void;
}

const ParamControls: React.FC<ParamControlsProps> = ({ mode, model, params, onChange }) => {
  const { t } = useTranslation();

  if (mode === 'text') return null;

  if (mode === 'image') {
    const localZImage = isLocalZImageModel(model);
    const effectiveParams = normalizeImageParamsForModel(model, params);
    const p = readImageParams(effectiveParams);
    const sizePresets = localZImage ? localZImageSizePresets() : IMAGE_SIZE_PRESETS;
    const presetOptions: PillOption[] = sizePresets.map((s) => ({
      key: s.key,
      label: t(`workshopGeneration.size.${s.labelKey}`, { defaultValue: s.key }),
    }));
    const qualityOptions: PillOption[] = IMAGE_QUALITIES.map((q) => ({
      key: q,
      label: t(`workshopGeneration.quality.${q}`, { defaultValue: q }),
    }));
    return (
      <div className='flex min-w-0 flex-col gap-11px'>
        <FieldRow label={t('workshopGeneration.param.size', { defaultValue: '尺寸' })}>
          <PillGroup
            options={presetOptions}
            value={p.preset}
            fill
            onSelect={(key) => {
              const preset = sizePresets.find((s) => s.key === key);
              if (preset) onChange({ preset: preset.key, width: preset.width, height: preset.height });
            }}
          />
          <div className='grid w-full grid-cols-[minmax(0,1fr)_auto_minmax(0,1fr)] items-center gap-6px'>
            <NumberBox
              value={p.width}
              min={localZImage ? LOCAL_Z_IMAGE_DIMENSION_MIN : undefined}
              max={localZImage ? LOCAL_Z_IMAGE_DIMENSION_MAX : undefined}
              step={localZImage ? LOCAL_Z_IMAGE_DIMENSION_STEP : undefined}
              normalize={localZImage ? normalizeLocalZImageDimension : undefined}
              onChange={(v) => onChange({ width: v, preset: 'custom' })}
            />
            <span className='text-11px text-[var(--color-text-3)]'>×</span>
            <NumberBox
              value={p.height}
              min={localZImage ? LOCAL_Z_IMAGE_DIMENSION_MIN : undefined}
              max={localZImage ? LOCAL_Z_IMAGE_DIMENSION_MAX : undefined}
              step={localZImage ? LOCAL_Z_IMAGE_DIMENSION_STEP : undefined}
              normalize={localZImage ? normalizeLocalZImageDimension : undefined}
              onChange={(v) => onChange({ height: v, preset: 'custom' })}
            />
          </div>
        </FieldRow>

        {localZImage ? (
          <div className='rounded-8px bg-[var(--color-fill-1)] px-9px py-6px text-11px leading-17px text-[var(--color-text-2)]'>
            {t('workshopGeneration.param.localSingleImage', { defaultValue: '本地模型每次生成 1 张图片' })}
          </div>
        ) : (
          <div className='flex items-end justify-between gap-12px'>
            <FieldRow label={t('workshopGeneration.param.count', { defaultValue: '数量' })}>
              <Stepper value={p.count} min={IMAGE_COUNT_MIN} max={IMAGE_COUNT_MAX} onChange={(v) => onChange({ count: v })} />
            </FieldRow>
          </div>
        )}

        <FieldRow label={t('workshopGeneration.param.quality', { defaultValue: '质量' })}>
          <PillGroup options={qualityOptions} value={p.quality} fill onSelect={(key) => onChange({ quality: key })} />
        </FieldRow>
      </div>
    );
  }

  // video
  const p = readVideoParams(params);
  const resOptions: PillOption[] = VIDEO_RESOLUTIONS.map((r) => ({ key: r, label: r }));
  const aspectOptions: PillOption[] = VIDEO_ASPECTS.map((a) => ({ key: a, label: a }));
  return (
    <div className='flex min-w-0 flex-col gap-11px'>
      <FieldRow label={t('workshopGeneration.param.duration', { defaultValue: '时长' })}>
        <Stepper
          value={p.seconds}
          min={VIDEO_SECONDS_MIN}
          max={VIDEO_SECONDS_MAX}
          suffix={t('workshopGeneration.param.seconds', { defaultValue: '秒' })}
          onChange={(v) => onChange({ seconds: v })}
        />
      </FieldRow>
      <FieldRow label={t('workshopGeneration.param.resolution', { defaultValue: '分辨率' })}>
        <PillGroup options={resOptions} value={p.resolution} fill onSelect={(key) => onChange({ resolution: key })} />
      </FieldRow>
      <FieldRow label={t('workshopGeneration.param.aspect', { defaultValue: '比例' })}>
        <PillGroup options={aspectOptions} value={p.aspect} fill onSelect={(key) => onChange({ aspect: key })} />
      </FieldRow>
      <div className='flex flex-col gap-4px pt-2px'>
        <Toggle
          label={t('workshopGeneration.param.audio', { defaultValue: '生成声音' })}
          checked={p.generate_audio}
          onChange={(v) => onChange({ generate_audio: v })}
        />
        <Toggle
          label={t('workshopGeneration.param.watermark', { defaultValue: '添加水印' })}
          checked={p.watermark}
          onChange={(v) => onChange({ watermark: v })}
        />
      </div>
    </div>
  );
};

export default ParamControls;
