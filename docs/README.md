# NomiFun Documentation

This folder contains the current technical, operator, and contributor
documentation for **NomiFun**. Normative current behavior lives under
`architecture/` and `contributing/`. The `continuity/` area is retained for
historical decisions, handoff context, and release audits; it does not override
the current architecture or contributor standards.

> New to the project? Start with
> [Getting Started -> Introduction](getting-started/introduction.md).
> Chinese docs start at [README.zh.md](README.zh.md).

## Start Here

| Need | Read |
| --- | --- |
| Understand what NomiFun is | [getting-started/introduction.md](getting-started/introduction.md) |
| Install or run locally | [getting-started/installation.md](getting-started/installation.md) |
| Try the app quickly | [getting-started/quick-start.md](getting-started/quick-start.md) |
| Understand the current architecture | [architecture/overview.md](architecture/overview.md) |
| Build or package the project | [contributing/building-and-packaging.md](contributing/building-and-packaging.md) |
| Look up flags, env vars, or API groups | [reference/](reference/) |
| Contribute to the project | [../CONTRIBUTING.md](../CONTRIBUTING.md) |
| Change database schema or identifiers | [contributing/data-and-identifier-standards.md](contributing/data-and-identifier-standards.md) |
| Community expectations | [../CODE_OF_CONDUCT.md](../CODE_OF_CONDUCT.md) |
| Report a security issue | [../SECURITY.md](../SECURITY.md) |
| Release notes and release process | [../CHANGELOG.md](../CHANGELOG.md), [../RELEASING.md](../RELEASING.md) |

## Current Documentation

```text
docs/
├── getting-started/      introduction, installation, quick start
├── guides/               current product/operator guides
├── architecture/         current system architecture and implementation map
├── reference/            configuration, API overview, troubleshooting, FAQ
├── contributing/         development, project structure, data/ID standards, build/package notes
├── continuity/           historical decisions, handoff notes, and release audits
├── skills/               exported skill docs for external agents
└── images/               screenshot manifest and referenced images
```

Current top-level user surfaces include conversations, terminals, model
management, presets, MCP, open capabilities, requirements/AutoWork,
scheduled tasks, companions, knowledge, and feature-gated computer/browser
automation. The frontend source of truth is
`ui/src/renderer/components/layout/Router.tsx`.

## Editing Rules

- Keep English and Simplified Chinese siblings in sync when both exist.
- Prefer linking to source files for implementation facts rather than repeating
  fragile line-by-line state.
- Do not document redirected legacy UI paths as primary navigation.
- When a feature is not surfaced in `Router.tsx`, do not present it as an active
  user feature even if backend routes still exist.
- For scripts, use `package.json` and `bun run help` as the source of truth.
- For database and identifier work, use
  [Data and Identifier Standards](contributing/data-and-identifier-standards.md)
  and the linked executable schema/registry as the source of truth.
