/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

export type UpdaterInstallReason =
  | 'app_bundle_not_found'
  | 'app_translocation'
  | 'mounted_volume'
  | 'cross_device'
  | 'metadata_unavailable';

export interface UpdaterInstallContext {
  platform: string;
  appBundlePath: string | null;
  tempDir: string;
  appDeviceId: number | null;
  tempDeviceId: number | null;
  autoInstallSupported: boolean;
  reason: UpdaterInstallReason | null;
}

export const AUTO_INSTALL_UNSUPPORTED_ERROR = 'NOMIFUN_UPDATER_AUTO_INSTALL_UNSUPPORTED';

export interface InstallUpdateDependencies {
  getContext: () => Promise<UpdaterInstallContext>;
  install: () => Promise<void>;
  relaunch: () => Promise<void>;
}

export async function installUpdateWithPreflight(deps: InstallUpdateDependencies): Promise<void> {
  const context = await deps.getContext();
  if (!context.autoInstallSupported) {
    throw new Error(`${AUTO_INSTALL_UNSUPPORTED_ERROR}:${context.reason ?? 'metadata_unavailable'}`);
  }
  await deps.install();
  await deps.relaunch();
}
