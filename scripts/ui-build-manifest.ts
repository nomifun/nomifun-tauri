import { randomUUID } from 'node:crypto';

export interface UiBuildManifest {
  schema: 1;
  app_version: string;
  api_contract_version: number;
  frontend_build_id: string;
}

export function createUiBuildManifest(
  appVersion: string,
  rawApiContractVersion: string,
  buildIdFactory: () => string = randomUUID
): UiBuildManifest {
  if (!appVersion.trim()) {
    throw new Error('UI app version must not be blank');
  }
  const contractSource = rawApiContractVersion.trim();
  const apiContractVersion = Number.parseInt(contractSource, 10);
  if (!/^\d+$/.test(contractSource) || !Number.isSafeInteger(apiContractVersion) || apiContractVersion < 1) {
    throw new Error(`Invalid ui-api-contract-version.txt value: ${JSON.stringify(contractSource)}`);
  }
  const buildId = buildIdFactory();
  if (!buildId.trim()) {
    throw new Error('UI frontend build id must not be blank');
  }

  return {
    schema: 1,
    app_version: appVersion,
    api_contract_version: apiContractVersion,
    frontend_build_id: buildId,
  };
}
