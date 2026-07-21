# Repository Rules

## GitHub Actions Are Absolutely Forbidden

This is a non-negotiable, repository-wide rule. It applies to every branch,
worktree, contributor, coding agent, and automated tool.

- Never create, restore, generate, stage, commit, merge, or rename a GitHub
  Actions workflow into `.github/workflows/`.
- No `.yml` or `.yaml` workflow file is allowed in that directory. Disabled,
  manual-only, scheduled, reusable, release-only, and temporary workflows are
  all prohibited without exception.
- Never enable GitHub Actions in the repository settings or through the GitHub
  API or CLI.
- Do not bypass this rule by placing equivalent GitHub Actions configuration in
  another path, generating it during release work, or restoring it from Git
  history.
- Build, test, packaging, and release automation must use local scripts and
  documented manual commands instead of repository-hosted workflows.
- Historical documentation that mentions CI or GitHub Actions is descriptive
  only and does not override this rule.

Before completing any change, verify that no workflow YAML exists under
`.github/workflows/`. If a task conflicts with this rule, stop and report the
conflict. The rule must be explicitly changed by the repository owner before
such work can proceed.
