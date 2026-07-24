import { useCallback, useEffect, useState } from 'react';
import { ipcBridge } from '@/common';
import type { IMcpServer } from '@/common/config/storage';
import { ensureBackendMcpCatalog } from './catalog';
import { parseExtensionMcpServers, type ExtensionMcpServerContribution } from './extensionCatalog';

/**
 * MCP server state hook.
 * Combines backend-managed user servers with extension-contributed servers.
 */
export const useMcpServers = () => {
  const [mcpServers, setMcpServers] = useState<IMcpServer[]>([]);
  const [extensionMcpServers, setExtensionMcpServers] = useState<ExtensionMcpServerContribution[]>([]);
  const [isMcpServersLoading, setIsMcpServersLoading] = useState(true);

  useEffect(() => {
    void ensureBackendMcpCatalog()
      .then(({ allServers }) => {
        setMcpServers(allServers);
      })
      .catch((error) => {
        console.error('[useMcpServers] Failed to load MCP catalog:', error);
        setMcpServers([]);
      })
      .finally(() => {
        setIsMcpServersLoading(false);
      });

    void ipcBridge.extensions.getMcpServers
      .invoke()
      .then((extServers) => {
        if (!extServers || extServers.length === 0) {
          setExtensionMcpServers([]);
          return;
        }

        const converted = parseExtensionMcpServers(extServers);
        if (converted.length !== extServers.length) {
          console.warn(
            `[useMcpServers] Ignored ${extServers.length - converted.length} malformed extension MCP contribution(s)`
          );
        }
        setExtensionMcpServers(converted);
      })
      .catch((error) => {
        console.error('[useMcpServers] Failed to load extension MCP servers:', error);
        setExtensionMcpServers([]);
      });
  }, []);

  const saveMcpServers = useCallback((serversOrUpdater: IMcpServer[] | ((prev: IMcpServer[]) => IMcpServer[])) => {
    return new Promise<void>((resolve) => {
      setMcpServers((prevServers) => {
        const nextServers = typeof serversOrUpdater === 'function' ? serversOrUpdater(prevServers) : serversOrUpdater;
        queueMicrotask(resolve);
        return nextServers;
      });
    });
  }, []);

  return {
    mcpServers,
    isMcpServersLoading,
    allMcpServers: [...mcpServers, ...extensionMcpServers],
    extensionMcpServers,
    setMcpServers,
    saveMcpServers,
  };
};
