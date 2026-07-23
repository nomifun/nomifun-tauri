import classNames from 'classnames';
import React from 'react';
import { useLayoutContext } from '@/renderer/hooks/context/LayoutContext';
import { SettingsViewModeProvider } from '@/renderer/components/settings/SettingsModal/settingsViewContext';
import { resolveExtensionAssetUrl } from '@/renderer/utils/platform';
import { type IExtensionSettingsTab } from '@/common/adapter/ipcBridge';
import { useExtensionSettingsTabs } from '@/renderer/hooks/system/useExtensionSettingsTabs';
import { Computer, Cpu, Earth, Info, Lightning, Puzzle, Send, System } from '@icon-park/react';
import { useTranslation } from 'react-i18next';
import { useLocation, useNavigate } from 'react-router-dom';
import { useExtI18n } from '@/renderer/hooks/system/useExtI18n';
import { BUILTIN_TAB_IDS, LEGACY_ANCHOR_REMAP } from './SettingsSider';
import './settings.css';

interface SettingsPageWrapperProps {
  children: React.ReactNode;
  className?: string;
  contentClassName?: string;
}

type NavItem = { label: string; icon: React.ReactElement; path: string; id: string };

type TranslateFn = (key: string, options?: { defaultValue?: string }) => string;

export function getBuiltinSettingsNavItems(t: TranslateFn): NavItem[] {
  const builtinMap: Record<string, NavItem> = {
    'execution-engines': {
      id: 'execution-engines',
      label: t('settings.executionEngineHub.railTitle'),
      icon: <Cpu theme='outline' size='16' />,
      path: 'execution-engines',
    },
    capabilities: {
      id: 'capabilities',
      label: t('settings.capabilities', { defaultValue: 'Capabilities' }),
      icon: <Lightning theme='outline' size='16' />,
      path: 'capabilities',
    },
    webhook: {
      id: 'webhook',
      label: t('webhook.label'),
      icon: <Send theme='outline' size='16' />,
      path: 'webhook',
    },
    display: {
      id: 'display',
      label: t('settings.display'),
      icon: <Computer theme='outline' size='16' />,
      path: 'display',
    },
    webui: {
      id: 'webui',
      // Tab id stays 'webui' (extension anchors + deep links depend on it);
      // the label is now WebUI-only — IM channels moved to per-companion settings.
      label: t('settings.webui'),
      icon: <Earth theme='outline' size='16' />,
      path: 'webui',
    },
    system: { id: 'system', label: t('settings.system'), icon: <System theme='outline' size='16' />, path: 'system' },
    'browser-use': {
      id: 'browser-use',
      label: t('settings.browserUseNav'),
      icon: <Earth theme='outline' size='16' />,
      path: 'browser-use',
    },
    'computer-use': {
      id: 'computer-use',
      label: t('settings.computerUseNav'),
      icon: <Computer theme='outline' size='16' />,
      path: 'computer-use',
    },
    about: { id: 'about', label: t('settings.about'), icon: <Info theme='outline' size='16' />, path: 'about' },
  };

  return BUILTIN_TAB_IDS.map((id) => builtinMap[id]);
}

const SettingsPageWrapper: React.FC<SettingsPageWrapperProps> = ({ children, className, contentClassName }) => {
  const layout = useLayoutContext();
  const isMobile = layout?.isMobile ?? false;
  const navigate = useNavigate();
  const { pathname } = useLocation();
  const { t } = useTranslation();

  const extensionTabs = useExtensionSettingsTabs();

  const { resolveExtTabName } = useExtI18n();

  const menuItems = React.useMemo(() => {
    const builtins = getBuiltinSettingsNavItems(t);

    // Insert extension tabs at their anchor, or (unanchored) at the end of the
    // "Application" group — before "about" — to keep them inside that group.
    const result = [...builtins];
    const unanchored: IExtensionSettingsTab[] = [];
    const beforeMap = new Map<string, IExtensionSettingsTab[]>();
    const afterMap = new Map<string, IExtensionSettingsTab[]>();

    for (const tab of extensionTabs) {
      if (!tab.position) {
        unanchored.push(tab);
        continue;
      }
      const { relative_to: rawAnchor, placement } = tab.position;
      const anchor = LEGACY_ANCHOR_REMAP[rawAnchor] ?? rawAnchor;
      if (!result.some((item) => item.id === anchor)) {
        unanchored.push(tab);
        continue;
      }
      const map = placement === 'before' ? beforeMap : afterMap;
      let list = map.get(anchor);
      if (!list) {
        list = [];
        map.set(anchor, list);
      }
      list.push(tab);
    }

    const toNavItem = (tab: IExtensionSettingsTab): NavItem => {
      const resolvedIcon = resolveExtensionAssetUrl(tab.icon) || tab.icon;
      return {
        id: tab.id,
        label: resolveExtTabName(tab),
        icon: resolvedIcon ? (
          <img src={resolvedIcon} alt='' className='w-16px h-16px object-contain' />
        ) : (
          <Puzzle theme='outline' size='16' />
        ),
        path: `ext/${tab.id}`,
      };
    };

    for (let i = result.length - 1; i >= 0; i--) {
      const id = result[i].id;
      const afters = afterMap.get(id);
      if (afters) result.splice(i + 1, 0, ...afters.map(toNavItem));
      const befores = beforeMap.get(id);
      if (befores) result.splice(i, 0, ...befores.map(toNavItem));
    }

    if (unanchored.length > 0) {
      const aboutIdx = result.findIndex((item) => item.id === 'about');
      const idx = aboutIdx >= 0 ? aboutIdx : result.length;
      result.splice(idx, 0, ...unanchored.map(toNavItem));
    }

    return result;
  }, [t, extensionTabs, resolveExtTabName]);

  const containerClass = classNames(
    'settings-page-wrapper w-full min-h-full box-border overflow-y-auto',
    isMobile ? 'px-16px py-14px' : 'px-12px md:px-40px py-32px',
    className
  );

  const contentClass = classNames('settings-page-content mx-auto w-full md:max-w-1024px', contentClassName);

  return (
    <SettingsViewModeProvider value='page'>
      <div className={containerClass}>
        {isMobile && (
          <div className='settings-mobile-top-nav'>
            {menuItems.map((item) => {
              const active = pathname.includes(`/settings/${item.path}`);
              return (
                <button
                  key={item.path}
                  type='button'
                  className={classNames('settings-mobile-top-nav__item', {
                    'settings-mobile-top-nav__item--active': active,
                  })}
                  onClick={() => {
                    void navigate(`/settings/${item.path}`, { replace: true });
                  }}
                >
                  <span className='settings-mobile-top-nav__icon'>{item.icon}</span>
                  <span className='settings-mobile-top-nav__label'>{item.label}</span>
                </button>
              );
            })}
          </div>
        )}
        <div className={contentClass}>{children}</div>
      </div>
    </SettingsViewModeProvider>
  );
};

export default SettingsPageWrapper;
