# Changelog

NomiFun is pre-1.0. Until the first public release, this file records release
notes at a high level rather than a complete historical log.

## Unreleased

## v0.3.0 - 2026-07-24

- Rebuilt the persistence and identifier architecture around a v3 data
  contract: local technical rows use integer identities, stable business
  references use canonical UUIDv7 values, and cross-domain relationships use
  explicit logical-reference policies.
- Added a guarded whole-dataset reset lifecycle for pre-v3 installations,
  including managed-root inventory, quarantine/retired-dataset receipts,
  crash-safe recovery, generation isolation, and stricter v3-only
  backup/restore validation.
- Improved conversation and agent reliability with idempotent message delivery,
  durable execution state, safer retries, stronger terminal/process cleanup,
  bounded knowledge writeback, and more consistent provider/model routing.
- Hardened AutoWork, requirement execution, scheduled-task delivery, channel
  routing, and notification synchronization across reconnects and retries.
- Added a Skill Market tab to the independent Skills capability, with bounded
  ClawHub and SkillHub ranking sync, tag/search filtering, localized skill
  descriptions, and a reviewed installation draft handoff to Nomi.
- Reduced bundled built-in skills and made OfficeCLI opt-in to keep default
  installations smaller and avoid injecting unused capabilities.
- **Breaking upgrade:** upgrading from an earlier data contract does not migrate
  local product data into v3. On first launch, the previous managed dataset is
  retired/quarantined and a clean v3 dataset is initialized. Dataset-owned
  credentials and integrations must be configured again; arbitrary external
  user workspaces are not deleted.
- Packaging note: this Windows-first release publishes the Windows x64
  installer and signed Tauri updater assets. macOS and Linux packages can be
  appended later from their native build machines.
- The Windows installer is updater-signed but not Authenticode-signed, so manual
  downloads may show a SmartScreen or unknown-publisher warning.

## v0.1.13 - 2026-07-01

- Improved orchestration reliability and control: DAG node pre-configuration,
  per-node model selection, explicit in-conversation approval before execution,
  and fixes for broken DAG lines, orphaned running nodes, one-node planning, and
  blank pending states.
- Added graceful handling for providers/models that do not support image input:
  image capability tracking, proactive image removal, retry without interrupting
  the conversation, and a visible in-conversation notice.
- Expanded browser-use controls with silent mode defaults, managed/system
  browser source selection, persistent encrypted browser login, a one-click
  browser login action, and screenshot context for silent approvals.
- Fixed WebUI credential persistence across restarts and added per-model context
  window configuration.
- Polished updater error handling, local update test clients, README screenshots,
  provider quick links, and contact assets.
- Packaging note: this Mac-side release publishes macOS installer and updater
  assets. Windows and Linux packages must be added later from their native build
  machines.

## v0.1.12 - 2026-07-01

- Documentation overhaul for public website and open-source preparation.
- Clarified desktop, web, remote access, AutoWork, scheduled tasks, and
  packaging documentation.
- Removed proprietary PDF skill assets from the bundled built-in skills.

## Release Note Policy

Every public release should include:

- User-facing changes.
- Breaking configuration or data migration notes.
- Security-relevant changes.
- Packaging and updater notes.
- Known limitations.

Use calendar dates or semantic versions consistently once public releases
begin.
