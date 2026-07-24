# Identifier System

This document is the canonical v3 identifier architecture contract for
NomiFun. It applies to product database tables, Rust domain models,
HTTP/WebSocket/MCP payloads, runtime registries, managed files, backups, and
imports. The contributor-facing mandatory workflow is
[Data and Identifier Standards](../contributing/data-and-identifier-standards.md).
The continuity documents are historical/audit context and cannot override this
contract.

## Core rules

The v3 design separates five concepts:

```text
table technical key
stable business ID
internal technical row
natural/external key
operation token
```

They are not interchangeable.

1. Every NomiFun-owned persistent product table has the same technical primary
   key:

   ```sql
   id INTEGER PRIMARY KEY AUTOINCREMENT
   ```

2. An entity that needs a stable product locator across databases, devices,
   files, APIs, events, or managed stores has a separately named, bare UUIDv7
   business field such as `user_id`, `conversation_id`, `message_id`,
   `mcp_server_id`, `webhook_id`, `credential_id`, or `creation_task_id`.
3. A relation, singleton, cache, or event row that is never addressed outside
   its owning persistence subsystem uses only its integer `id`. It does not
   receive a UUID merely for uniformity, and that `id` never becomes a product
   wire locator.
4. Relationships are logical references maintained by repositories and
   services. The product schema contains no physical foreign keys,
   `REFERENCES` clauses, triggers, or database cascades.
5. v3 is a new dataset lineage. Historical datasets are reset as a whole; rows
   and old identifier formats are not migrated into v3.

## Technical primary key

Every product table, including relationship, value-object, singleton, cache,
and dependent tables, must declare:

```sql
id INTEGER PRIMARY KEY AUTOINCREMENT
```

SQLite internal tables, migration metadata, and temporary tables are not
product tables.

The technical `id`:

- identifies a row inside one table and one active dataset;
- is the table's primary index entry;
- is represented as `i64` in Rust;
- is never exported as a technical row key through an API, event, managed
  filename/manifest, or dataset boundary;
- is not a cross-dataset identity, public locator, filename contract, or
  distributed ID, and is not copied into stable event or file references;
- must not be assumed to be contiguous.

`AUTOINCREMENT` fixes the schema shape and prevents reuse of previously issued
positive row IDs. It does not turn the integer into a stable business ID.

## Stable business IDs

Only entities that require identity outside the local database row receive a
named business ID:

```text
user_id
conversation_id
message_id
provider_id
execution_id
knowledge_base_id
```

The value is a bare, canonical UUIDv7:

```text
0190f5fe-7c00-7a00-8000-000000000003
```

The contract is:

- exactly 36 characters in the standard `8-4-4-4-12` form;
- lowercase hexadecimal;
- RFC UUID variant;
- UUID version 7;
- no prefix, suffix, braces, compact form, whitespace, or alternate separator;
- a JSON string and SQLite `TEXT`;
- generated with the full 128-bit UUID value, without truncation.

The field name, table meaning, and Rust/TypeScript domain type identify the
entity kind. The UUID text does not encode the kind.

A typical stable entity table is:

```sql
CREATE TABLE conversations (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    conversation_id TEXT NOT NULL UNIQUE
                    CHECK (
                        length(conversation_id) = 36
                        AND lower(conversation_id) = conversation_id
                        AND conversation_id
                            GLOB '????????-????-7???-[89ab]???-????????????'
                        AND replace(conversation_id, '-', '')
                            NOT GLOB '*[^0-9a-f]*'
                    )
);
```

Examples of stable v3 entities include users, conversations, messages,
terminal sessions, providers, requirements, agent executions and templates,
Agent Execution Participant/Step/Attempt/Template Participant, knowledge
bases, attachments, remote agents, user presets, workshop canvases/assets,
and Channel Plugin/User/Session. Requirements use `requirement_id` plus a
human-facing `display_no`. The Agent Execution and Channel child entities use
`participant_id`, `step_id`, `attempt_id`, `template_participant_id`,
`channel_plugin_id`, `channel_user_id`, and `channel_session_id`.
Managed Companion side-store records that can be addressed again through an
API, file, or another record—memories, suggestions, learn runs, session
windows, collected events, skills, and skill patterns—also use distinct named
UUIDv7 newtypes. A temporary evolution summary that exists only in one call
result remains an operation token and is not promoted to a stable entity.

## Internal technical rows

Some relation, singleton, cache, and event rows do not need an independent
business identity. They still have the mandatory table `id`, but that value is
strictly internal to the active SQLite dataset. It is not a product locator,
numeric string, public API field, event identity, managed filename, or portable
backup identity.

Entities that are addressed by a product API, runtime registry, managed file,
backup graph, or another managed store use a named UUIDv7 business field even
when their lifecycle is installation-local. In the current v3 baseline this
includes MCP servers, webhooks, connector credentials, creation tasks,
conversation artifacts, and IDMM interventions. Their table `id` remains only
the technical primary key.

No product wire contract introduces an integer business ID or generic `id`
alias. If a future internal-only subsystem needs an integer handle, it must
remain explicitly scoped to that subsystem and must not cross the product
boundary.

## Logical references

v3 removes physical foreign keys from all product schemas. Product DDL must
not contain:

```text
FOREIGN KEY
REFERENCES
CREATE TRIGGER
ON DELETE CASCADE
ON UPDATE CASCADE
*_row_id
```

A relationship stores exactly one reference:

- if the parent has a stable business ID, store that named UUIDv7 field;
- internal-only relation, dependent, and event rows are scoped by a parent
  business ID plus a sequence, natural key, or composite condition; they do
  not propagate technical `id` values into other tables;
- if the value is issued by another system, store an explicitly named opaque
  external ID.

The current v3 baseline has no `INTEGER` relationship that targets another
table's technical `id`.

Stable-parent example:

```sql
CREATE TABLE messages (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    message_id      TEXT NOT NULL UNIQUE,
    conversation_id TEXT NOT NULL
);

CREATE INDEX idx_messages_conversation_id
    ON messages(conversation_id);
```

Stable Cron-parent example:

```sql
CREATE TABLE cron_jobs (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    cron_job_id TEXT NOT NULL UNIQUE
);

CREATE TABLE cron_job_runs (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    cron_job_run_id TEXT NOT NULL UNIQUE,
    cron_job_id     TEXT NOT NULL
);

CREATE INDEX idx_cron_job_runs_cron_job_id
    ON cron_job_runs(cron_job_id);
```

`messages.conversation_id` logically targets
`conversations.conversation_id`; `cron_job_runs.cron_job_id` logically targets
`cron_jobs.cron_job_id`. Neither relationship is declared to SQLite.

Do not store both `conversation_id` and `conversation_row_id`, or any equivalent
business-ID/row-ID pair, for the same relationship.

### Application integrity

Removing physical foreign keys does not remove integrity requirements. Every
logical reference must be registered with:

- child table and field;
- parent table and target field;
- data type, nullability, and scope;
- required index;
- delete policy: `RESTRICT`, application-level `CASCADE`, `SET_NULL`, or
  `KEEP_HISTORY`;
- restore/clone rebuild policy;
- orphan-audit query.

Repositories and services must validate parents, write related rows, and apply
delete policies in explicit transactions. The `CASCADE` policy here is an
application transaction policy, never a SQLite cascade or trigger. Bulk
restore/import is followed by a complete orphan audit. Business code must not
bypass this boundary and write logical-reference columns directly.

## Natural keys, external IDs, and tokens

Natural keys such as skill names, extension slugs, model names, URLs, locales,
tags, and singleton keys retain their domain-specific formats. Relationship
and singleton tables still have an auto-increment `id`; business uniqueness is
expressed with additional `UNIQUE` constraints.

External identifiers such as `acp_session_id`, `platform_user_id`,
`platform_chat_id`, `remote_task_id`, and provider request IDs remain opaque
and are validated only against their source protocol.

Request IDs, idempotency keys, capability nonces, workspace tokens, and other
short-lived operation values are not entity IDs. They must use purpose-specific
field names and must never become table primary keys or logical business IDs by
accident.

## Rust and protocol boundaries

`nomifun-common` owns bare UUIDv7 generation and strict validation. Stable
business IDs use small domain newtypes around the same canonical string form.
Technical row IDs may use `i64` internally within a repository, but are not
domain or wire identifiers.

At every boundary:

- stable business IDs are canonical UUIDv7 strings;
- technical row IDs remain inside repository/storage implementation details;
- external IDs remain explicitly typed opaque values;
- invalid values fail rather than becoming `0`, an empty string, or another
  ID kind;
- an absent optional business ID in JSON is represented by an omitted field;
  explicit `null`, retired aliases, and wrong JSON types violate the data
  contract;
- routes, DTOs, caches, events, and filesystem manifests use the same business
  or external field type as the domain model; a technical `id` is never the
  portable value at those boundaries.

## v3 hard reset, backup, and restore

v3 hard reset does not migrate historical datasets.

At startup, before opening the product database:

1. acquire the dataset/reset lock;
2. detect the dataset contract and generation;
3. require the exact baseline checksum, complete ID schema registry, every
   physical/registered JSON ID value, the logical-reference orphan audit, and
   managed side-store ID/filesystem indexes such as Workshop and Companion to
   pass before accepting the dataset as current v3;
4. if it is historical or incompatible, move the complete managed dataset to
   a retired/quarantine location;
5. create a new empty v3 dataset and baseline schema;
6. write and finalize a reset receipt before serving requests.

No table-by-table conversion, dual-read, alias column, legacy-ID mapping, or
selective business-data copy is permitted. A reset failure is fatal; the
application must not continue with mixed old and new state. External
user-owned workspaces are not deleted, but v3 does not carry their historical
database references forward.

Only v3 backup manifests are accepted by v3 restore. Restore preserves every
stable business UUIDv7 and rebuilds technical `id` values. Logical relations
are reconstructed from business UUIDv7 values, natural keys, external IDs, and
registered JSON/side-store references rather than source row IDs. Clone also
preserves the supplied business UUIDv7; it does not mint or implicitly rewrite
business UUIDs. If a clone would collide with an existing business UUID, it
fails closed without a partial write. Technical `id` values are dataset-local
and are never treated as portable graph identity.

## Adding an identifier-bearing entity

Before adding a table or entity:

1. add `id INTEGER PRIMARY KEY AUTOINCREMENT` to the product table;
2. decide whether the entity truly needs cross-dataset identity;
3. if yes, add one named, unique bare UUIDv7 business field and domain newtype;
4. if no, keep `id` internal and identify the row by its owner plus a sequence,
   natural key, or composite condition when needed;
5. classify every relationship as a business, natural, or external logical
   reference; never target another table's technical `id`;
6. add its index, delete/rebuild policy, and orphan audit to the registry;
7. add schema and boundary tests for the selected representation.
