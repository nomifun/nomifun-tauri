/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import { buildBackendAuthHeaders, getBaseUrl } from '@/common/adapter/httpBridge';
import type { SpeechToTextResult } from '@/common/types/provider/speech';

const MAX_AUDIO_FILE_SIZE_MB = 30;
const MAX_AUDIO_FILE_SIZE_BYTES = MAX_AUDIO_FILE_SIZE_MB * 1024 * 1024;

const getAudioExtension = (mimeType: string) => {
  switch (mimeType) {
    case 'audio/mp4':
    case 'audio/x-m4a':
      return 'm4a';
    case 'audio/mpeg':
      return 'mp3';
    case 'audio/ogg':
    case 'audio/ogg;codecs=opus':
      return 'ogg';
    case 'audio/wav':
    case 'audio/wave':
      return 'wav';
    default:
      return 'webm';
  }
};

const createAudioFileName = (mimeType: string) => {
  return `speech-input.${getAudioExtension(mimeType)}`;
};

const ensureAudioSize = (blob: Blob) => {
  if (blob.size > MAX_AUDIO_FILE_SIZE_BYTES) {
    throw new Error('STT_FILE_TOO_LARGE');
  }
};

const parseWebResponse = async (response: XMLHttpRequest): Promise<SpeechToTextResult> => {
  let payload: {
    data?: SpeechToTextResult;
    error?: string;
    msg?: string;
    success: boolean;
  };
  try {
    payload = JSON.parse(response.responseText) as typeof payload;
  } catch {
    throw new Error('STT_REQUEST_FAILED');
  }

  if (!payload.success || !payload.data) {
    throw new Error(payload.error || payload.msg || 'STT_REQUEST_FAILED');
  }

  return payload.data;
};

export async function transcribeAudioBlob(blob: Blob, languageHint?: string): Promise<SpeechToTextResult> {
  ensureAudioSize(blob);

  const mimeType = blob.type || 'audio/webm';
  const file_name = createAudioFileName(mimeType);

  const formData = new FormData();
  // The backend's multipart contract uses `file`, `fileName`, `mimeType` and
  // `languageHint`. Keep this path for both Tauri and WebUI: local ASR can
  // consume the recorded bytes directly, while cloud providers continue to
  // use the same endpoint as before.
  formData.append('file', blob, file_name);
  formData.append('fileName', file_name);
  formData.append('mimeType', mimeType);
  if (languageHint) {
    formData.append('languageHint', languageHint);
  }

  return new Promise<SpeechToTextResult>((resolve, reject) => {
    const xhr = new XMLHttpRequest();
    xhr.open('POST', `${getBaseUrl()}/api/stt`);
    // Desktop dev is cross-origin (`http://localhost:5173` ->
    // `http://127.0.0.1:<backend-port>`). Setting `withCredentials` here makes
    // the browser require `Access-Control-Allow-Credentials: true` plus a
    // non-wildcard origin, while the trusted desktop backend intentionally uses
    // wildcard CORS and authenticates with `x-nomi-local-trust`. The browser
    // then reports a generic XHR network error before `/api/stt` is reached.
    //
    // WebUI requests are same-origin and already send cookies by default, so
    // explicit credential mode is unnecessary in both environments.
    for (const [name, value] of Object.entries(buildBackendAuthHeaders('POST'))) {
      xhr.setRequestHeader(name, value);
    }

    xhr.addEventListener('load', () => {
      if (xhr.status === 413) {
        reject(new Error('STT_FILE_TOO_LARGE'));
        return;
      }
      if (xhr.status < 200 || xhr.status >= 300) {
        let detail = '';
        try {
          const payload = JSON.parse(xhr.responseText) as { code?: string; error?: string; msg?: string };
          detail = [payload.code, payload.error || payload.msg].filter(Boolean).join(': ');
        } catch {
          detail = xhr.responseText.trim();
        }
        reject(new Error(`STT_REQUEST_FAILED:${detail || `${xhr.status} ${xhr.statusText}`}`));
        return;
      }

      parseWebResponse(xhr).then(resolve).catch(reject);
    });

    xhr.addEventListener('error', () => {
      reject(new Error('STT_NETWORK_ERROR'));
    });

    xhr.addEventListener('abort', () => {
      reject(new Error('STT_ABORTED'));
    });

    xhr.send(formData);
  });
}
