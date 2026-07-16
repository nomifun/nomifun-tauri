/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import React from 'react';

interface RouteErrorBoundaryProps {
  children: React.ReactNode;
  /** Clears a captured route error when navigation changes the rendered target. */
  resetKey?: string;
  /** Root failures need an application reload; route failures can retry in place. */
  scope?: 'route' | 'application';
}

interface RouteErrorBoundaryState {
  error: Error | null;
  componentStack: string | null;
}

/**
 * RouteErrorBoundary — 路由级错误边界
 *
 * The app previously had NO error boundary, so any render/throw inside a route
 * blanked the entire window (white screen) with no visible cause. This boundary
 * wraps each lazily-loaded route element (via `withRouteFallback`) so a crash in
 * one page renders a readable error panel — message + stack + React component
 * stack — instead of taking down the whole shell. The surrounding app chrome
 * (titlebar, primary sidebar) stays alive, and the error text is selectable so
 * it can be copied for diagnosis.
 *
 * React Router can reuse a boundary instance when only a route parameter
 * changes. `resetKey` therefore clears stale failures explicitly on navigation.
 */
class RouteErrorBoundary extends React.Component<RouteErrorBoundaryProps, RouteErrorBoundaryState> {
  state: RouteErrorBoundaryState = { error: null, componentStack: null };

  static getDerivedStateFromError(error: Error): Partial<RouteErrorBoundaryState> {
    return { error };
  }

  componentDidCatch(error: Error, info: React.ErrorInfo): void {
    // Surface to the console too (devtools, if available) — keep the on-screen
    // panel as the primary channel since release builds may not expose devtools.
    // eslint-disable-next-line no-console
    console.error(
      `[RouteErrorBoundary] ${this.props.scope === 'application' ? 'application' : 'route'} crashed:`,
      error,
      info.componentStack
    );
    this.setState({ componentStack: info.componentStack ?? null });
  }

  componentDidUpdate(previousProps: RouteErrorBoundaryProps): void {
    if (previousProps.resetKey !== this.props.resetKey && this.state.error) {
      this.setState({ error: null, componentStack: null });
    }
  }

  private handleReset = (): void => {
    this.setState({ error: null, componentStack: null });
  };

  private handleApplicationReload = (): void => {
    window.location.reload();
  };

  render(): React.ReactNode {
    const { error, componentStack } = this.state;
    if (!error) return this.props.children;
    const isApplicationFailure = this.props.scope === 'application';

    return (
      <div
        role='alert'
        style={{
          height: '100%',
          width: '100%',
          overflow: 'auto',
          padding: '24px',
          boxSizing: 'border-box',
          background: '#1b1115',
          color: '#ffd9d9',
          fontFamily: 'ui-monospace, SFMono-Regular, Menlo, Consolas, monospace',
          fontSize: '13px',
          lineHeight: 1.55,
        }}
      >
        <div style={{ fontSize: '15px', fontWeight: 700, color: '#ff6b6b', marginBottom: '12px' }}>
          {isApplicationFailure
            ? '应用渲染出错（已捕获，未显示空白窗口）'
            : '页面渲染出错（已被路由错误边界捕获，未影响其它页面）'}
        </div>
        <div style={{ fontWeight: 700, marginBottom: '8px', userSelect: 'text' }}>
          {error.name}: {error.message}
        </div>
        <button
          type='button'
          onClick={isApplicationFailure ? this.handleApplicationReload : this.handleReset}
          style={{
            marginBottom: '16px',
            padding: '4px 12px',
            border: '1px solid #ff6b6b',
            borderRadius: '6px',
            background: 'transparent',
            color: '#ffd9d9',
            cursor: 'pointer',
          }}
        >
          {isApplicationFailure ? '重新加载应用' : '重试'}
        </button>
        {error.stack ? (
          <>
            <div style={{ opacity: 0.7, marginBottom: '4px' }}>Stack</div>
            <pre style={{ whiteSpace: 'pre-wrap', userSelect: 'text', margin: '0 0 16px' }}>{error.stack}</pre>
          </>
        ) : null}
        {componentStack ? (
          <>
            <div style={{ opacity: 0.7, marginBottom: '4px' }}>Component stack</div>
            <pre style={{ whiteSpace: 'pre-wrap', userSelect: 'text', margin: 0 }}>{componentStack}</pre>
          </>
        ) : null}
      </div>
    );
  }
}

export default RouteErrorBoundary;
