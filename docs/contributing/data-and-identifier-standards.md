# Data and Identifier Standards

This is the repository-wide contributor standard for database schema, identity,
logical references, dataset reset, backup/restore, and related protocol
boundaries. It is mandatory for new code and changes to existing code.

The authority order is:

1. [`001_v3_baseline.sql`](../../crates/backend/nomifun-db/migrations/001_v3_baseline.sql)
   and
   [`id_schema_contract.rs`](../../crates/backend/nomifun-db/src/id_schema_contract.rs)
   — executable schema and runtime registry;
2. [`architecture/id-system.md`](../architecture/id-system.md) and
   [`architecture/data-and-storage.md`](../architecture/data-and-storage.md) —
   architecture contract and storage behavior;
3. this page — contributor workflow and review checklist;
4. `docs/continuity/` — historical context, decisions, handoff, and audit
   evidence. It cannot override the current architecture contract.

If implementation and documentation disagree, do not invent a compatibility
exception. Update the implementation and its authoritative documentation
together, or stop and request an architecture decision.

## 1. Mandatory product-table shape

Every NomiFun-owned persistent product table must begin with the same technical
primary key:

```sql
id INTEGER PRIMARY KEY AUTOINCREMENT
```

This applies to entity, relationship, value-object, singleton, cache,
event/outbox, and dependent tables. SQLite internal tables, migration metadata,
and temporary tables are not product tables.

The technical `id` is local to one table and one active dataset. It is:

- an internal storage key and primary-index entry;
- represented as `i64` inside repository/storage code;
- allowed to have gaps and never assumed to be contiguous;
- regenerated on restore, clone, or hard reset;
- forbidden as a product API, event, file, manifest, backup-graph, or
  cross-dataset locator.

Do not rename this column to `row_id`, `table_id`, or another variant. Do not
copy one table's technical `id` into another table.

The current v3 lineage is a clean baseline, not a promise to preserve every
historical migration path. A schema change must first establish whether the
v3 baseline, a new lineage, or an explicitly approved release migration is the
correct target. Do not add compatibility migrations, legacy mappers, or
dual-read/dual-write paths merely to avoid making that decision.

## 2. Stable business IDs

Add a business ID only when an entity must remain addressable outside its local
database row: across databases, devices, managed files, APIs, events, backup
graphs, or side stores.

Use one field-specific name, for example:

```text
user_id
conversation_id
message_id
provider_id
execution_id
knowledge_base_id
webhook_id
credential_id
```

The value is a bare canonical UUIDv7:

```text
0190f5fe-7c00-7a00-8000-000000000003
```

The fixed contract is:

- standard `8-4-4-4-12` representation, exactly 36 characters;
- lowercase hexadecimal and standard hyphens;
- UUID version 7 and the RFC UUID variant;
- full 128-bit value, without truncation;
- stored as SQLite `TEXT` and serialized as a JSON string;
- no `prefix_UUIDv7`, suffix, braces, compact form, whitespace, or custom
  separator.

The field name, table meaning, and Rust/TypeScript domain type identify the
entity kind. The UUID text must not encode a type prefix. Do not introduce a
generic wire `id` when a named business field is available.

Do not give every row a UUID for visual uniformity. Internal-only relation,
singleton, cache, and event rows keep only the mandatory technical `id` unless
there is a real product or cross-store locator requirement.

## 3. Distinguish ID categories

The suffix `_id` does not by itself decide semantics. Before adding or
validating a field, classify it as one of these:

| Category | Rule |
| --- | --- |
| Technical row ID | The fixed local `id`; never crosses a repository or product boundary. |
| Stable business ID | Named bare UUIDv7 for a product-addressable entity. |
| Natural key | Domain-defined value such as a name, slug, URL, locale, or singleton key; preserve its format. |
| External ID | Opaque value issued by a provider or protocol, such as a platform user/chat ID, ACP session ID, or remote task ID. |
| Operation token | Request ID, idempotency key, nonce, workspace token, or receipt token; purpose-specific and not an entity identity. |
| Document identity | Canvas node/edge or similar document-local identity; not a database primary key. |

Protocol-specific UUIDv7 values need the same explicit classification. For
example, `message_correlations.turn_message_id` is a wire-scoped protocol
owner token used before projection; it is not a parent reference to
`messages.message_id`. Conversely, a field such as `messages.msg_id` must
follow the logical-reference registry when it links to another message. Do not
infer these rules from a column suffix alone.

## 4. Logical references replace physical foreign keys

Product DDL must not contain:

```text
FOREIGN KEY
REFERENCES
CREATE TRIGGER
ON DELETE CASCADE
ON UPDATE CASCADE
*_row_id
```

Store exactly one logical reference for one relationship:

- reference the parent's named business ID when the parent is
  product-addressable;
- use an owner business ID plus a sequence, natural key, or composite
  condition for internal-only rows;
- preserve an explicitly named opaque external ID when another system owns the
  identifier;
- never store both `conversation_id` and `conversation_row_id`, or any
  equivalent business-ID/row-ID pair.

Logical references are not informal conventions. Every reference must be
registered in `id_schema_contract.rs` (including JSON/side-store references
where applicable) with:

- child table and column, parent table and target column;
- kind, value contract, nullability, and scope/predicates;
- required index;
- delete policy: `RESTRICT`, application-level `CASCADE`, `SET_NULL`, or
  `KEEP_HISTORY`;
- restore/clone rebuild policy;
- orphan-audit policy/query.

Repositories and services are the only normal write path. They must validate
parents, enforce aggregate ownership, and apply the registered delete policy
inside explicit transactions. Application-level `CASCADE` is service behavior,
never a SQLite cascade or trigger. Raw SQL is limited to controlled fixtures,
diagnostics, and maintenance.

After restore/import, run the complete registered orphan audit across the
database and managed side stores. A missing physical FK is not permission to
accept unchecked orphan rows.

## 5. Dataset lineage and hard reset

v3 is intentionally incompatible with historical product datasets. Startup
must identify dataset lineage/generation before opening the product database.
An absent dataset initializes as v3; a historical or incompatible managed
dataset is retired/quarantined as a whole and replaced with a new empty v3
dataset.

Forbidden compatibility behavior:

- table-by-table conversion or historical data migration;
- legacy ID normalization or old-to-new maps;
- compatibility reads, aliases, dual-read, or dual-write;
- selectively copying old JSON, caches, workspace indexes, or side-store rows;
- continuing startup after a partial reset.

The reset scope includes the SQLite database and its WAL/SHM sidecars plus all
managed side stores. Reset writes and finalizes the generation/reset receipt
before serving requests. A reset failure is fail-closed. External
user-owned workspaces are not deleted, but their historical database
references are not imported into v3.

## 6. Backup, restore, and clone

Backups and restores operate on the v3 managed dataset as one unit:

- accept only v3 manifests and lineage;
- preserve stable business UUIDv7 values;
- rebuild technical `id` values in the destination;
- reconstruct logical references from registered business IDs, natural keys,
  external IDs, JSON, and side-store references;
- reject business-ID collisions and partial installs;
- run the complete orphan audit after installation.

Source technical row IDs are never portable graph identity. Clone preserves the
supplied business IDs; it does not silently mint or rewrite them.

## 7. Boundary and type rules

`nomifun-common` owns bare UUIDv7 generation and strict validation. Stable
business IDs should use small Rust domain newtypes and TypeScript branded
types where the boundary benefits from it.

At HTTP, WebSocket, MCP, Gateway, event, cache, filesystem, and backup
boundaries:

- use the named business or external field type from the domain model;
- keep technical `id` inside repository/storage implementation details;
- reject malformed values instead of coercing them to `0`, empty strings, or
  another ID kind;
- do not expose a generic numeric `id` as a portable product locator.

## 8. Required tests and verification

A database or identifier change must add or update focused tests for the
affected contract. At minimum, cover the applicable items:

- every product table has `id INTEGER PRIMARY KEY AUTOINCREMENT`;
- no product DDL contains physical FKs, `REFERENCES`, triggers, database
  cascades, or `*_row_id`;
- accepted business IDs are canonical UUIDv7 and old/prefixed/short formats
  are rejected;
- logical-reference registry entries, indexes, scope checks, and delete
  policies;
- repository parent validation and transaction behavior;
- orphan audits, including JSON/side-store references;
- dataset lineage detection, hard reset, generation, and reset receipt;
- backup/restore/clone preservation and technical-ID rebuild.

Delete stale or misleading tests instead of preserving pseudo-coverage. Use
the narrowest targeted test first; reserve expensive workspace-wide gates for
the final integration stage.

For documentation-only changes, run at least:

```bash
git diff --check
```

## 9. New table or identifier checklist

Before opening a review:

1. Read this standard and the architecture ID contract.
2. Add the fixed `id` column to every new product table.
3. Decide whether a named business UUIDv7 is genuinely required.
4. Classify every `_id` field as business, natural, external, token, document,
   or logical reference.
5. Remove technical row-ID propagation and physical FK/trigger designs.
6. Register every logical relation, index, lifecycle policy, rebuild policy,
   and orphan audit.
7. Keep technical IDs out of all wire, event, file, and backup contracts.
8. Add focused schema, repository, boundary, reset, or restore tests.
9. Review the complete managed-data reset impact.
10. Keep English and Simplified Chinese documentation synchronized.

## 10. Repository hygiene

Do not commit local data, secrets, generated builds, dependency trees, or
intermediate products. In particular, keep these out of Git:

```text
.tmp*/
.tmp***
target/
build.noindex/
node_modules/
dist/
coverage/
*.o
*.rmeta
*.rlib
```

If a tool produces a non-standard temporary directory or dependency artifact,
add it to the local ignore policy before continuing. Never "clean up" another
developer's generated files by adding them to a commit.
