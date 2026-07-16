/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

export type SpeechInputAvailability = 'record' | 'unsupported';

export type SpeechInputEnvironment = {
  hasMediaDevices: boolean;
  hasMediaRecorder: boolean;
  hostname: string;
  isDesktopShell: boolean;
  isSecureContext: boolean;
};

const LOCAL_HOSTNAMES = new Set(['localhost', '127.0.0.1', '::1']);

/**
 * A microphone control must never silently turn into a file picker. Desktop
 * packages expose the system microphone through their WebView; browsers need
 * a secure context (with the standard localhost exception).
 */
export const getSpeechInputAvailabilityForEnvironment = (
  environment: SpeechInputEnvironment
): SpeechInputAvailability => {
  const canUseLiveRecording =
    environment.hasMediaDevices &&
    environment.hasMediaRecorder &&
    (environment.isDesktopShell || environment.isSecureContext || LOCAL_HOSTNAMES.has(environment.hostname));

  return canUseLiveRecording ? 'record' : 'unsupported';
};
