/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import { WEBUI_DEFAULT_PORT } from '@/common/config/constants';
import { webui, type IWebUIStatus } from '@/common/adapter/ipcBridge';
import { configService } from '@/common/config/configService';
import { Message } from '@arco-design/web-react';
import React, { createContext, useCallback, useContext, useEffect, useMemo, useState } from 'react';
import { useTranslation } from 'react-i18next';

const DESKTOP_WEBUI_ENABLED_KEY = 'webui.desktop.enabled';

export interface WebuiServerContextValue {
  /** Latest known server status (null until first load). */
  status: IWebUIStatus | null;
  /** Authoritative "is the LAN server running" flag (tracks the real backend state). */
  running: boolean;
  /** A start/stop transition is in flight. */
  starting: boolean;
  /**
   * Whether WebUI lifecycle ops are real (vs DEGRADE_STUB). Equals "am I the
   * desktop shell" — a remote browser cannot control the listener serving it.
   */
  lifecycleSupported: boolean;
  /**
   * The admin password generated on first enable. The backend returns it exactly
   * ONCE from `webui_start` and never broadcasts it, so it is captured here and
   * shared across every surface (titlebar drawer + settings page) — whichever
   * triggered the start, both can show it.
   */
  initialPassword: string | null;
  /** localhost + each NIC URL, deduped, in display order. */
  accessUrls: string[];
  start: () => Promise<void>;
  stop: () => Promise<void>;
  refresh: () => Promise<void>;
  /** Forget the one-time password (call after the admin changes it). */
  clearInitialPassword: () => void;
}

const noop = async () => {};

const DEGRADED_VALUE: WebuiServerContextValue = {
  status: null,
  running: false,
  starting: false,
  lifecycleSupported: false,
  initialPassword: null,
  accessUrls: [],
  start: noop,
  stop: noop,
  refresh: noop,
  clearInitialPassword: () => {},
};

const WebuiServerContext = createContext<WebuiServerContextValue>(DEGRADED_VALUE);

/**
 * WebuiServerProvider — the single source of truth for the WebUI LAN server.
 *
 * Mounted once in `Layout` so the titlebar quick-entry and the Settings page
 * share ONE `webui.statusChanged` subscription, one start/stop path, and one
 * capture of the one-time initial password. Inert (degraded) outside the desktop
 * shell, where the lifecycle ops are stubs.
 */
export const WebuiServerProvider: React.FC<{ children: React.ReactNode }> = ({ children }) => {
  const { t } = useTranslation();
  const lifecycleSupported = webui.lifecycleSupported;

  const [status, setStatus] = useState<IWebUIStatus | null>(null);
  const [running, setRunning] = useState(false);
  const [starting, setStarting] = useState(false);
  const [initialPassword, setInitialPassword] = useState<string | null>(null);

  const refresh = useCallback(async () => {
    if (!lifecycleSupported) return;
    try {
      const data = await webui.getStatus.invoke();
      if (data) {
        setStatus(data);
        // The Switch must track the *real* server state, not the persisted
        // preference (a silently-failed auto-restore must read as "off").
        setRunning(data.running);
        if (data.initialPassword) setInitialPassword(data.initialPassword);
      } else {
        setRunning(false);
        setStatus(
          (prev) =>
            prev || {
              running: false,
              port: WEBUI_DEFAULT_PORT,
              allowRemote: false,
              localUrl: `http://localhost:${WEBUI_DEFAULT_PORT}`,
              adminUsername: 'admin',
            }
        );
      }
    } catch (error) {
      console.error('[WebuiServer] Failed to load status:', error);
      setRunning(false);
    }
  }, [lifecycleSupported]);

  useEffect(() => {
    void refresh();
  }, [refresh]);

  // Single live subscription — keeps every consumer in lock-step with a
  // main-process auto-restore or an external stop without a reload.
  useEffect(() => {
    if (!lifecycleSupported) return undefined;
    const unsubscribe = webui.statusChanged.on((data) => {
      setRunning(data.running === true);
      if (data.running) {
        setStatus((prev) => ({
          ...(prev || { adminUsername: 'admin' }),
          running: true,
          port: data.port ?? prev?.port ?? WEBUI_DEFAULT_PORT,
          allowRemote: prev?.allowRemote ?? true,
          localUrl: data.localUrl ?? `http://localhost:${data.port ?? WEBUI_DEFAULT_PORT}`,
          networkUrl: data.networkUrl,
          networkUrls: data.networkUrls ?? prev?.networkUrls,
          lanIP: prev?.lanIP,
          adminUsername: data.adminUsername || prev?.adminUsername || 'admin',
          passwordSet: data.passwordSet ?? prev?.passwordSet,
          initialPassword: prev?.initialPassword,
        }));
      } else {
        setStatus((prev) => (prev ? { ...prev, running: false } : null));
      }
    });
    return () => unsubscribe();
  }, [lifecycleSupported]);

  const start = useCallback(async () => {
    if (!lifecycleSupported || starting) return;
    setStarting(true);
    setRunning(true);
    try {
      const result = await webui.start.invoke();
      // The backend returns running:false + an error string on failure (e.g.
      // could not bind the port) — surface the real reason, don't fake success.
      if (!result.running) {
        setRunning(false);
        Message.error(result.error || t('settings.webui.operationFailed'));
        return;
      }
      if (result.initialPassword) setInitialPassword(result.initialPassword);
      setStatus((prev) => ({
        ...(prev || { adminUsername: 'admin' }),
        running: true,
        port: result.port || WEBUI_DEFAULT_PORT,
        allowRemote: true,
        localUrl: result.localUrl || `http://localhost:${result.port || WEBUI_DEFAULT_PORT}`,
        networkUrl: result.networkUrl,
        networkUrls: result.networkUrls,
        lanIP: result.lanIP,
        adminUsername: result.adminUsername || prev?.adminUsername || 'admin',
        passwordSet: result.passwordSet ?? prev?.passwordSet,
        initialPassword: result.initialPassword ?? prev?.initialPassword,
      }));
      await configService.set(DESKTOP_WEBUI_ENABLED_KEY, true);
      Message.success({ content: t('settings.webui.startSuccess'), duration: 1500 });
    } catch (error) {
      setRunning(false);
      console.error('[WebuiServer] start error:', error);
      Message.error(t('settings.webui.operationFailed'));
    } finally {
      setStarting(false);
    }
  }, [lifecycleSupported, starting, t]);

  const stop = useCallback(async () => {
    if (!lifecycleSupported || starting) return;
    setStarting(true);
    setRunning(false);
    setStatus((prev) => (prev ? { ...prev, running: false } : null));
    try {
      await configService.set(DESKTOP_WEBUI_ENABLED_KEY, false);
      await webui.stop.invoke();
      Message.success({ content: t('settings.webui.stopSuccess'), duration: 1500 });
    } catch (error) {
      console.error('[WebuiServer] stop error:', error);
      Message.error(t('settings.webui.operationFailed'));
    } finally {
      setStarting(false);
    }
  }, [lifecycleSupported, starting, t]);

  // Only real LAN/network URLs are useful for WebUI remote access. The localhost
  // URL is reachable only on the host machine (where one would just use the
  // desktop app), so listing it misleads users into thinking it is a shareable
  // address — exclude it from the displayed access URLs.
  const accessUrls = useMemo(() => {
    return (status?.networkUrls ?? []).filter((u, i, arr) => Boolean(u) && arr.indexOf(u) === i);
  }, [status?.networkUrls]);

  const clearInitialPassword = useCallback(() => setInitialPassword(null), []);

  const value = useMemo<WebuiServerContextValue>(
    () => ({ status, running, starting, lifecycleSupported, initialPassword, accessUrls, start, stop, refresh, clearInitialPassword }),
    [status, running, starting, lifecycleSupported, initialPassword, accessUrls, start, stop, refresh, clearInitialPassword]
  );

  return <WebuiServerContext.Provider value={value}>{children}</WebuiServerContext.Provider>;
};

export const useWebuiServer = (): WebuiServerContextValue => useContext(WebuiServerContext);
