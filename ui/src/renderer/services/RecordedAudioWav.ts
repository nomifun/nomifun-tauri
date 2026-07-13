/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

const LOCAL_ASR_SAMPLE_RATE = 16_000;
const WAV_HEADER_BYTES = 44;

const writeAscii = (view: DataView, offset: number, value: string): void => {
  for (let index = 0; index < value.length; index += 1) {
    view.setUint8(offset + index, value.charCodeAt(index));
  }
};

/**
 * Average decoded channels into one mono PCM stream.
 *
 * AudioBuffer channels normally have identical lengths. The slightly more
 * defensive implementation below also handles uneven test/custom inputs by
 * averaging only channels that contain the current sample.
 */
export const downmixAudioChannels = (channels: readonly Float32Array[]): Float32Array => {
  const sampleCount = channels.reduce((maximum, channel) => Math.max(maximum, channel.length), 0);
  const mono = new Float32Array(sampleCount);

  for (let sampleIndex = 0; sampleIndex < sampleCount; sampleIndex += 1) {
    let sum = 0;
    let contributors = 0;
    for (const channel of channels) {
      if (sampleIndex < channel.length) {
        sum += channel[sampleIndex];
        contributors += 1;
      }
    }
    mono[sampleIndex] = contributors > 0 ? sum / contributors : 0;
  }

  return mono;
};

/**
 * Resample mono PCM without relying on browser-only OfflineAudioContext.
 *
 * Downsampling uses a weighted box average to avoid simply discarding two out
 * of every three samples when converting common 48 kHz recordings to 16 kHz.
 * Upsampling uses linear interpolation.
 */
export const resampleMonoPcm = (
  input: Float32Array,
  sourceSampleRate: number,
  targetSampleRate = LOCAL_ASR_SAMPLE_RATE
): Float32Array => {
  if (!Number.isFinite(sourceSampleRate) || sourceSampleRate <= 0) {
    throw new RangeError('sourceSampleRate must be greater than zero');
  }
  if (!Number.isFinite(targetSampleRate) || targetSampleRate <= 0) {
    throw new RangeError('targetSampleRate must be greater than zero');
  }
  if (input.length === 0) {
    return new Float32Array();
  }
  if (sourceSampleRate === targetSampleRate) {
    return input.slice();
  }

  const sourceSamplesPerOutput = sourceSampleRate / targetSampleRate;
  const outputLength = Math.max(1, Math.round(input.length / sourceSamplesPerOutput));
  const output = new Float32Array(outputLength);

  if (sourceSamplesPerOutput > 1) {
    for (let outputIndex = 0; outputIndex < outputLength; outputIndex += 1) {
      const sourceStart = outputIndex * sourceSamplesPerOutput;
      const sourceEnd = Math.min(input.length, (outputIndex + 1) * sourceSamplesPerOutput);
      const firstSourceIndex = Math.floor(sourceStart);
      const lastSourceIndex = Math.ceil(sourceEnd);
      let weightedSum = 0;
      let totalWeight = 0;

      for (let sourceIndex = firstSourceIndex; sourceIndex < lastSourceIndex; sourceIndex += 1) {
        if (sourceIndex < 0 || sourceIndex >= input.length) {
          continue;
        }
        const overlapStart = Math.max(sourceStart, sourceIndex);
        const overlapEnd = Math.min(sourceEnd, sourceIndex + 1);
        const weight = Math.max(0, overlapEnd - overlapStart);
        weightedSum += input[sourceIndex] * weight;
        totalWeight += weight;
      }

      output[outputIndex] = totalWeight > 0 ? weightedSum / totalWeight : 0;
    }
    return output;
  }

  for (let outputIndex = 0; outputIndex < outputLength; outputIndex += 1) {
    const sourcePosition = Math.min(input.length - 1, outputIndex * sourceSamplesPerOutput);
    const leftIndex = Math.floor(sourcePosition);
    const rightIndex = Math.min(input.length - 1, leftIndex + 1);
    const fraction = sourcePosition - leftIndex;
    output[outputIndex] = input[leftIndex] + (input[rightIndex] - input[leftIndex]) * fraction;
  }

  return output;
};

/** Encode mono floating-point PCM as a standard little-endian PCM16 WAV. */
export const encodePcm16Wav = (samples: Float32Array, sampleRate = LOCAL_ASR_SAMPLE_RATE): ArrayBuffer => {
  if (!Number.isFinite(sampleRate) || sampleRate <= 0) {
    throw new RangeError('sampleRate must be greater than zero');
  }

  const bytesPerSample = 2;
  const dataBytes = samples.length * bytesPerSample;
  const buffer = new ArrayBuffer(WAV_HEADER_BYTES + dataBytes);
  const view = new DataView(buffer);

  writeAscii(view, 0, 'RIFF');
  view.setUint32(4, 36 + dataBytes, true);
  writeAscii(view, 8, 'WAVE');
  writeAscii(view, 12, 'fmt ');
  view.setUint32(16, 16, true);
  view.setUint16(20, 1, true);
  view.setUint16(22, 1, true);
  view.setUint32(24, sampleRate, true);
  view.setUint32(28, sampleRate * bytesPerSample, true);
  view.setUint16(32, bytesPerSample, true);
  view.setUint16(34, 16, true);
  writeAscii(view, 36, 'data');
  view.setUint32(40, dataBytes, true);

  for (let index = 0; index < samples.length; index += 1) {
    const finiteSample = Number.isFinite(samples[index]) ? samples[index] : 0;
    const clamped = Math.max(-1, Math.min(1, finiteSample));
    const pcm16 = clamped < 0 ? Math.round(clamped * 0x8000) : Math.round(clamped * 0x7fff);
    view.setInt16(WAV_HEADER_BYTES + index * bytesPerSample, pcm16, true);
  }

  return buffer;
};

/**
 * Decode a browser MediaRecorder blob, downmix it, resample it to the
 * 16 kHz/mono format expected by local ASR runtimes, and return PCM16 WAV.
 */
export const convertRecordedAudioToWav = async (blob: Blob): Promise<Blob> => {
  const AudioContextConstructor =
    typeof AudioContext !== 'undefined'
      ? AudioContext
      : typeof window !== 'undefined'
        ? (window as Window & { webkitAudioContext?: typeof AudioContext }).webkitAudioContext
        : undefined;

  if (!AudioContextConstructor) {
    throw new Error('AudioContext is unavailable');
  }

  const audioContext = new AudioContextConstructor();
  try {
    const decoded = await audioContext.decodeAudioData(await blob.arrayBuffer());
    const channels = Array.from({ length: decoded.numberOfChannels }, (_, channelIndex) =>
      decoded.getChannelData(channelIndex)
    );
    const mono = downmixAudioChannels(channels);
    const resampled = resampleMonoPcm(mono, decoded.sampleRate, LOCAL_ASR_SAMPLE_RATE);
    return new Blob([encodePcm16Wav(resampled, LOCAL_ASR_SAMPLE_RATE)], { type: 'audio/wav' });
  } finally {
    try {
      await audioContext.close();
    } catch {
      // A decode failure can leave a partially initialized context. The
      // original MediaRecorder blob is still usable as a backend fallback.
    }
  }
};
