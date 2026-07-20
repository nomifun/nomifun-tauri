import React, { createContext, useCallback, useContext, useEffect, useMemo, useRef, useState } from 'react';
import { isDesktopShell } from '@renderer/utils/platform';
import { configService } from '@/common/config/configService';
import { AUTH_EXPIRED_EVENT } from '@/common/adapter/httpBridge';
import { consumeQrLoginResume } from './qrLoginResume';
import { parseUserId, type UserId } from '@/common/types/ids';
// M6: CSRF removed with legacy webserver — stub functions for compatibility, re-implement in M7
const withCsrfToken = <T extends Record<string, unknown>>(data: T): T => data;
const hasValidCsrfToken = (): boolean => true;
const clearCookie = (_name: string, _path?: string): void => {};
const CSRF_COOKIE_NAME = 'csrf-token';

type AuthStatus = 'checking' | 'authenticated' | 'unauthenticated';

export interface AuthUser {
  id: UserId;
  username: string;
}

const parseAuthUser = (value: unknown): AuthUser | null => {
  if (!value || typeof value !== 'object') return null;
  const raw = value as { id?: unknown; username?: unknown };
  if (typeof raw.username !== 'string') return null;
  return { id: parseUserId(raw.id), username: raw.username };
};

interface LoginParams {
  username: string;
  password: string;
  remember?: boolean;
}

interface SetupParams {
  username: string;
  password: string;
}

type LoginErrorCode =
  | 'invalidCredentials'
  | 'tooManyAttempts'
  | 'serverError'
  | 'networkError'
  | 'csrfError'
  | 'unknown';

interface LoginResult {
  success: boolean;
  message?: string;
  code?: LoginErrorCode;
  shouldClearCache?: boolean;
}

interface AuthContextValue {
  ready: boolean;
  user: AuthUser | null;
  status: AuthStatus;
  /** First-run flag: no admin exists yet, so the login screen creates one. */
  needsSetup: boolean;
  login: (params: LoginParams) => Promise<LoginResult>;
  /** First-run only: create the initial admin from the typed credentials and log in. */
  setup: (params: SetupParams) => Promise<LoginResult>;
  logout: () => Promise<void>;
  refresh: () => Promise<void>;
  clearAuthCache: () => void;
}

const AuthContext = createContext<AuthContextValue | undefined>(undefined);

const AUTH_USER_ENDPOINT = '/api/auth/user';
const AUTH_STATUS_ENDPOINT = '/api/auth/status';
const AUTH_SETUP_ENDPOINT = '/api/auth/setup';

// The bundled desktop shell (Electron or Tauri) runs a private, single-user,
// no-auth backend (`--local`), so it must skip the WebUI login flow entirely.
// Keyed on `window.__backendPort` (injected by both desktop runtimes) rather
// than `window.electronAPI` — the latter is absent under Tauri, which used to
// leave the desktop stuck on the login screen.
const isDesktopRuntime = isDesktopShell();

// Clear expired auth cache including cookies and localStorage
// 清除过期的认证缓存，包括 Cookie 和 localStorage
function clearAuthCache(): void {
  if (typeof window === 'undefined') return;

  try {
    // Clear CSRF cookie
    clearCookie(CSRF_COOKIE_NAME);
    clearCookie(CSRF_COOKIE_NAME, '/');

    // Clear localStorage auth-related items
    const keysToRemove: string[] = [];
    for (let i = 0; i < localStorage.length; i++) {
      const key = localStorage.key(i);
      if (key && (key.includes('auth') || key.includes('csrf') || key.includes('token'))) {
        keysToRemove.push(key);
      }
    }
    keysToRemove.forEach((key) => localStorage.removeItem(key));
  } catch (error) {
    console.error('Failed to clear auth cache:', error);
  }
}

async function fetchCurrentUser(signal?: AbortSignal): Promise<AuthUser | null> {
  try {
    const response = await fetch(AUTH_USER_ENDPOINT, {
      method: 'GET',
      credentials: 'include',
      signal,
      cache: 'no-store',
    });

    if (!response.ok) {
      return null;
    }

    const data = (await response.json()) as {
      success: boolean;
      user?: unknown;
    };
    const user = parseAuthUser(data.user);
    if (data.success && user) {
      return user;
    }
  } catch (error) {
    if ((error as Error).name === 'AbortError') {
      return null;
    }
    console.error('Failed to fetch current user:', error);
  }

  return null;
}

/**
 * Probe whether the backend still needs first-run setup (no admin exists yet).
 * Also seeds the CSRF cookie as a side effect of this GET's response, so the
 * subsequent setup/login POST already has a token to echo.
 */
async function fetchNeedsSetup(signal?: AbortSignal): Promise<boolean> {
  try {
    const response = await fetch(AUTH_STATUS_ENDPOINT, {
      method: 'GET',
      credentials: 'include',
      signal,
      cache: 'no-store',
    });
    if (!response.ok) return false;
    const data = (await response.json()) as { success?: boolean; needs_setup?: boolean };
    // Only trust the flag from a well-formed success response — defends against
    // a proxy / cached error page that happens to deserialize with a truthy
    // needs_setup (mirrors the data.success check fetchCurrentUser already does).
    return data.success === true && Boolean(data.needs_setup);
  } catch (error) {
    if ((error as Error).name === 'AbortError') return false;
    console.error('Failed to fetch auth status:', error);
    return false;
  }
}

export const AuthProvider: React.FC<React.PropsWithChildren> = ({ children }) => {
  const [user, setUser] = useState<AuthUser | null>(null);
  const [status, setStatus] = useState<AuthStatus>('checking');
  const [ready, setReady] = useState(false);
  const [needsSetup, setNeedsSetup] = useState(false);
  const abortRef = useRef<AbortController | null>(null);

  const refresh = useCallback(async () => {
    if (isDesktopRuntime) {
      setStatus('authenticated');
      setUser(null);
      setNeedsSetup(false);
      setReady(true);
      return;
    }

    const qrLoginUser = consumeQrLoginResume();
    if (qrLoginUser) {
      setUser(qrLoginUser);
      setStatus('authenticated');
      setNeedsSetup(false);
      setReady(true);
      configService.reload().catch(() => {});
      if (typeof window !== 'undefined' && (window as any).__websocketReconnect) {
        (window as any).__websocketReconnect();
      }
      return;
    }

    abortRef.current?.abort();
    const controller = new AbortController();
    abortRef.current = controller;
    setStatus('checking');

    // Probe first-run status (also seeds the CSRF cookie) before the login
    // screen decides between "sign in" and "create admin".
    setNeedsSetup(await fetchNeedsSetup(controller.signal));

    const currentUser = await fetchCurrentUser(controller.signal);
    if (currentUser) {
      setUser(currentUser);
      setStatus('authenticated');
    } else {
      setUser(null);
      setStatus('unauthenticated');
    }
    setReady(true);
  }, []);

  useEffect(() => {
    void refresh();
    return () => {
      abortRef.current?.abort();
    };
  }, [refresh]);

  useEffect(() => {
    if (isDesktopRuntime || typeof window === 'undefined') return undefined;
    const handleAuthExpired = () => {
      abortRef.current?.abort();
      setUser(null);
      setStatus('unauthenticated');
      setNeedsSetup(false);
      setReady(true);
      clearAuthCache();
    };
    window.addEventListener(AUTH_EXPIRED_EVENT, handleAuthExpired);
    return () => window.removeEventListener(AUTH_EXPIRED_EVENT, handleAuthExpired);
  }, []);

  const login = useCallback(async ({ username, password, remember }: LoginParams): Promise<LoginResult> => {
    try {
      if (isDesktopRuntime) {
        setReady(true);
        return { success: true };
      }

      // Check CSRF token availability before login
      // If token is missing, clear cache and inform user
      const csrfTokenValid = hasValidCsrfToken();
      if (!csrfTokenValid) {
        console.warn('CSRF token missing or invalid, clearing cache');
        clearAuthCache();
        // Allow login to proceed anyway - server will set new token
      }

      // P1 安全修复：登录请求需要 CSRF Token / P1 Security fix: Login needs CSRF token
      // Backend route is /login; web-host's static-server explicitly proxies it.
      const response = await fetch('/login', {
        method: 'POST',
        headers: {
          'Content-Type': 'application/json',
        },
        credentials: 'include',
        body: JSON.stringify(withCsrfToken({ username, password, remember })),
      });

      const data = (await response.json()) as {
        success: boolean;
        message?: string;
        user?: unknown;
      };
      const authenticatedUser = parseAuthUser(data.user);

      if (!response.ok || !data.success || !authenticatedUser) {
        let code: LoginErrorCode = 'unknown';
        let message = data?.message ?? 'Login failed';
        let shouldClearCache = false;

        if (response.status === 401) {
          code = 'invalidCredentials';
        } else if (response.status === 403) {
          // CSRF validation failed - clear cache
          code = 'csrfError';
          message = 'Security token expired. Please try again.';
          shouldClearCache = true;
        } else if (response.status === 429) {
          code = 'tooManyAttempts';
        } else if (response.status >= 500) {
          code = 'serverError';
        } else if (!csrfTokenValid) {
          // If we knew CSRF was invalid and login failed, suggest cache clear
          code = 'csrfError';
          message = 'Login failed due to cached data. Please clear your browser cache and try again.';
          shouldClearCache = true;
        }

        // Clear cache on CSRF-related errors
        if (shouldClearCache) {
          clearAuthCache();
        }

        return {
          success: false,
          message,
          code,
          shouldClearCache,
        };
      }

      setUser(authenticatedUser);
      setStatus('authenticated');
      setReady(true);

      // Settings were unreachable (401/403) before login; now that we're
      // authenticated, re-fetch them so theme/language/preferences populate
      // without a manual page refresh.
      configService.reload().catch(() => {});

      // Re-enable WebSocket reconnection after successful login (WebUI mode only)
      if (typeof window !== 'undefined' && (window as any).__websocketReconnect) {
        (window as any).__websocketReconnect();
      }

      return { success: true };
    } catch (error) {
      console.error('Login request failed:', error);

      // Check if error is related to CSRF token parsing
      const errorMessage = (error as Error).message;
      if (errorMessage?.includes('parse') || errorMessage?.includes('csrf') || errorMessage?.includes('cookie')) {
        // CSRF or cookie parsing error - clear cache
        clearAuthCache();
        return {
          success: false,
          message: 'Login failed due to cached data. Please clear your browser cache and try again.',
          code: 'csrfError',
          shouldClearCache: true,
        };
      }

      return {
        success: false,
        message: 'Network error. Please try again.',
        code: 'networkError',
      };
    }
  }, []);

  // First-run only: the very first visitor's typed username + password become
  // the initial admin. Backend `POST /api/auth/setup` works ONLY while no admin
  // exists (409 once initialized) and logs us in on success (sets the cookie).
  const setup = useCallback(async ({ username, password }: SetupParams): Promise<LoginResult> => {
    try {
      if (isDesktopRuntime) {
        setReady(true);
        return { success: true };
      }

      const response = await fetch(AUTH_SETUP_ENDPOINT, {
        method: 'POST',
        headers: {
          'Content-Type': 'application/json',
        },
        credentials: 'include',
        body: JSON.stringify({ username, password }),
      });

      const data = (await response.json()) as {
        success: boolean;
        message?: string;
        user?: unknown;
      };
      const authenticatedUser = parseAuthUser(data.user);

      if (!response.ok || !data.success || !authenticatedUser) {
        let code: LoginErrorCode = 'unknown';
        let message = data?.message ?? 'Setup failed';
        if (response.status === 409) {
          // Already initialized (e.g. pre-seeded or a racing visitor won) —
          // drop back to the normal sign-in screen.
          setNeedsSetup(false);
          message = data?.message ?? 'Already initialized. Please sign in.';
        } else if (response.status === 429) {
          code = 'tooManyAttempts';
        } else if (response.status >= 500) {
          code = 'serverError';
        }
        // 400 (weak password / invalid username) falls through as 'unknown' and
        // surfaces the backend message verbatim.
        return { success: false, message, code };
      }

      setUser(authenticatedUser);
      setStatus('authenticated');
      setNeedsSetup(false);
      setReady(true);

      // Re-fetch settings now that we're authenticated (see login()).
      configService.reload().catch(() => {});

      if (typeof window !== 'undefined' && (window as any).__websocketReconnect) {
        (window as any).__websocketReconnect();
      }

      return { success: true };
    } catch (error) {
      console.error('Setup request failed:', error);
      return {
        success: false,
        message: 'Network error. Please try again.',
        code: 'networkError',
      };
    }
  }, []);

  const logout = useCallback(async () => {
    if (isDesktopRuntime) {
      setUser(null);
      setStatus('authenticated');
      setReady(true);
      return;
    }

    try {
      await fetch('/logout', {
        method: 'POST',
        // Logout also needs CSRF token / 登出同样需要 CSRF Token
        headers: {
          'Content-Type': 'application/json',
        },
        credentials: 'include',
        body: JSON.stringify(withCsrfToken({})),
      });
    } catch (error) {
      console.error('Logout request failed:', error);
    } finally {
      setUser(null);
      setStatus('unauthenticated');
      // Clear cache on logout for security
      clearAuthCache();
    }
  }, []);

  const value = useMemo<AuthContextValue>(
    () => ({
      ready,
      user,
      status,
      needsSetup,
      login,
      setup,
      logout,
      refresh,
      clearAuthCache,
    }),
    [login, setup, logout, ready, refresh, status, user, needsSetup]
  );

  return <AuthContext.Provider value={value}>{children}</AuthContext.Provider>;
};

export function useAuth(): AuthContextValue {
  const context = useContext(AuthContext);
  if (!context) {
    throw new Error('useAuth must be used within an AuthProvider');
  }
  return context;
}
