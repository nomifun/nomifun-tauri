# NomiFun ID / Data-Storage Refactor Continuity Area

This directory is the cross-account, cross-platform handoff entry point for
the identifier and schema refactor. The authoritative working documents are
currently written in Chinese; start with [`README.zh.md`](README.zh.md).

This is not the repository-wide contributor standard. For current mandatory
rules, read [Data and Identifier Standards](../contributing/data-and-identifier-standards.md)
and the canonical architecture contract in
[`../architecture/id-system.md`](../architecture/id-system.md). The files here
record decisions, handoff context, implementation evidence, and release audit
status; they must not override those current sources of truth.

Status: `V3 CONTRACT IMPLEMENTED / RELEASE AUDIT PENDING`. The clean v3
baseline, the principal Rust/Gateway/UI ID hard cut, and repository-wide Rust
all-targets/UI static checks are implemented. Full workspace tests and the real
desktop reset/release matrix are still open, so this is not a release-complete
claim.

## Legacy v2 context and v3 contract

The `id-contract-v2` implementation described in the historical evidence is
not the target architecture. The current v3 contract uses bare, canonical
36-character UUIDv7 values without entity prefixes for distributed business
IDs. Every product-owned persistent table uses
`id INTEGER PRIMARY KEY AUTOINCREMENT` as its technical row key. This `id` is
not automatically a business ID.

The accepted v3 direction removes SQLite physical foreign keys and the
`*_row_id` dual-key convention. Relations use indexed, field-specific logical
references: product-addressable entities use named business IDs, while
internal-only rows are scoped by an owner business ID plus a sequence,
natural key, or composite condition. Technical `id` values are not inter-table
locators. Application services and orphan audits own referential integrity.

It uses a new dataset lineage and does not migrate or load historical product
data. The reset must cover the complete managed dataset, not only the main
SQLite file.

Stable wire objects expose field-specific identifiers such as
`conversation_id`, `execution_id`, `knowledge_base_id`, `canvas_id`,
`asset_id`, `mcp_server_id`, `webhook_id`, `credential_id`,
`creation_task_id`, `conversation_artifact_id`, and `intervention_id`.
These are bare canonical UUIDv7 values. The SQLite `id` column remains an
internal technical key and is never used as a product wire locator.
