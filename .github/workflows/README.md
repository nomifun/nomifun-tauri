# GitHub Actions Workflows Are Forbidden

This repository intentionally does not use GitHub Actions. Do not add, restore,
generate, or rename any `.yml` or `.yaml` workflow into this directory. This
includes disabled, manual-only, scheduled, reusable, release-only, and
temporary workflows.

Use repository-local scripts and documented manual commands for builds, tests,
packaging, and releases. Any change that introduces a GitHub Actions workflow
must be rejected or removed.
