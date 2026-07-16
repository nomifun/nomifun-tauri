# Canonical UUIDv7 Hard-Cut Repair Design

## Decision

NomiFun will enforce the current entity-ID contract without interpreting,
rewriting, or migrating identifiers from the retired 16-character short-ID
format as compatible data. The
desktop-companion shared store receives a destructive version-7 epoch. Existing
companion profiles survive only when their persisted identity is canonical, and
figure-library entries survive only when their canonical identity and managed
file can be proven. Every regular workspace is preserved or orphaned without
reattachment, and downloaded model assets survive unchanged.

This is a cross-platform storage and boundary repair. Windows is the first
reported surface, not a separate implementation or compatibility target.

## Proven root cause

Commit `2c0f975a` replaced the retired entity generator
`{prefix}_{16-character-base32}` with the canonical form:

```text
{registered-prefix}_{lowercase-hyphenated-RFC9562-UUIDv7}
```

The main SQLite database hard-cut to a clean ID-contract-v2 schema. The
independent desktop-companion database did not. Its v5-to-v6 migration in
`nomifun-companion/src/store.rs` scanned only:

- `companion_memories`;
- `companion_threads`;
- `companion_skills`.

It neither inspected nor reset `companion_learn_runs`, then stamped the file as
version 6. A historical row such as `plr_0fh3k123456789ab` therefore remains in
a database that every later boot considers current.

The read side completes the failure chain:

1. `row_to_learn_run` and `list_learn_runs` read `id` as an unchecked `String`.
2. `CompanionService::get_status` returns the latest row as `last_learn`.
3. Companion-list, companion-status, learn-history, run-now, and WebSocket
   payloads reuse the same unchecked model.
4. `ipcBridge.ts` calls `parseCompanionLearnRunId` at the wire boundary.
5. The frontend correctly throws `InvalidEntityIdError` because the persisted
   value is not a canonical `plr_<uuidv7>`.

The database is already version 6, so editing the old v5-to-v6 function cannot
repair affected installations. The memory-bundle importer also deserializes
learn-run IDs as ordinary strings and calls an unchecked insert method, so a
legacy bundle can recontaminate a manually cleaned database.

The same audit found equivalent gaps in companion suggestions, session windows,
runtime-state owners and conversation pointers, evolution-feedback IDs,
Workshop document/archive IDs, and the embedded delegation execution receipt.

## Goals

- Eliminate the reported `companion-learn-run` error at its storage source.
- Make every new NomiFun entity ID use its registered prefix plus an
  application-normalized lowercase, hyphenated RFC 9562 UUIDv7.
- Prevent an invalid entity ID from being generated, written through a domain
  API, inserted through an import, returned from disk, or restored from a
  pre-contract side store.
- Apply the same semantics on Windows, macOS, and Linux.
- Make a pre-v7 companion domain start with a newly initialized v7 shared
  domain, with no retained shared config, history row, event, staging file, or
  quarantine ledger.
- Preserve current companion profiles only when their existing identity is
  canonical, preserve only provable canonical figures, preserve every regular
  workspace without attaching it to an invalid identity, and preserve model
  assets unchanged.
- Keep the frontend strict so it remains a final invariant detector.
- Close the confirmed same-class gaps in the main database's structured JSON,
  public-agent and preview-history side stores, Workshop, embedded execution,
  destructive reset, and main-dataset epoch rotation.

## Non-goals and compatibility boundary

- Do not convert a retired short ID into a UUIDv7.
- Do not retain legacy rows in a quarantine table.
- Do not accept companion export version 1, Workshop archive version 1, an old
  `pet/nomi` layout, or a legacy profile under a compatibility reader.
- Do not relax frontend ID parsing or catch-and-hide `InvalidEntityIdError`.
- Do not delete a valid companion profile, a provably valid indexed figure, any
  companion workspace, or the regenerable model cache during the companion-store
  v7 reset.
- Do not apply NomiFun entity-ID rules to external provider IDs or operation
  keys such as OpenAI `call_...`, Anthropic `toolu_...`, platform message IDs,
  JSON-RPC request numbers, or `wso` Workshop operation IDs.
- Do not redesign backup product UX or add merge/compatibility semantics for
  old bundles. This repair may bump the exact format and harden closed-family
  snapshot/restore, but it rejects any main or companion side store that does
  not prove the new contract before installation.
- Do not recursively scan or delete arbitrary user-owned external directories
  when a malformed historical config has lost the bridge path. Such content is
  detached and unreachable from the fresh companion domain; only mirror files
  whose ownership/path can be proven are removed.

## Options considered

### 1. Patch only `companion_learn_runs`

Deleting invalid learn runs and validating that one mapper is the smallest
change. It leaves the already-stamped v6 file, unchecked sibling tables, import
bypass, old event data, and other prefix/UUID violations in place. A later UI
path would fail in the same way.

### 2. Extend v6 quarantine and migrate rows individually

This preserves more history, but it is the approach that already missed
several ID-bearing tables and JSON fields. It also violates the explicit
no-history-compatibility requirement and creates an indefinite alternate data
lineage.

### 3. Introduce a v7 store epoch and strict end-to-end boundaries

This is the selected design. Every pre-v7 companion database is rebuilt as one
empty transaction. Canonical current side-store entries may survive, while
legacy entries and legacy import formats are discarded. Generation, domain
writes, SQL storage, disk reads, imports, and frontend adapters each enforce the
same contract.

The trade-off is deliberate companion-history loss on the first upgraded boot:
memories, suggestions, learn runs, companion thread registrations, runtime
state, evolved skills, feedback, and session digests start empty.

## Selected architecture

The implementation is divided into six independently reviewable workstreams:
canonical registry/main structured references, companion v7 reset/boundaries,
portable import/lifecycle coherence, auxiliary side-store epochs, Workshop
identity enforcement, and embedded execution/frontend verification. They share
one canonical ID contract and ship only after the combined completion gate, but
each workstream has focused tests and can be reviewed without loading the
others' internals.

### 1. One canonical ID primitive

`nomifun-common` remains the authoritative Rust implementation. A durable ID is
valid only when all of the following hold:

- the prefix is the registered prefix for the entity type;
- exactly one underscore separates prefix and UUID;
- the UUID is 36 characters, lowercase, and hyphenated;
- the UUID version is 7;
- the UUID variant bits are `0b10`, the IETF variant defined by RFC 9562;
- the string contains no whitespace, braces, compact form, uppercase hex, or
  alternate separator.

[RFC 9562](https://www.rfc-editor.org/rfc/rfc9562.html) permits upper- or
lowercase hexadecimal in its generic text ABNF; NomiFun deliberately narrows
that representation to lowercase so one entity has one canonical application
string. UUIDv7 generation and bit validation follow section 5.7: a 48-bit
Unix-millisecond timestamp, version bits `0111`, and variant bits `10`.

Typed IDs are used wherever an ID crosses a domain, persistence, import, or
wire boundary. Conversion to `&str` is limited to SQL binding, filesystem path
components after validation, and transparent JSON serialization.

The typed-ID declaration macro also emits one authoritative runtime registry:
stable entity type name, exact prefix, parser, and mint function. Portable
graphs resolve `entity_type` through that registry, require `id_prefix` to equal
the registered prefix, parse `id` with the registered type, and mint clone IDs
through the registered mint function. Unknown entity types and syntactically
valid but unregistered prefixes are rejected.

All literal calls that mint a registered entity through
`generate_prefixed_id("...")` move to the corresponding newtype's `new()`.
The untyped generator becomes internal to the ID macro/registry. A separately
named operation-key helper remains available for explicitly catalogued,
non-entity leases, queue tokens, and idempotency keys such as `wso`,
`execeffect`, `execengine`, `execloop`, and `browseroob`; using that helper in a
persisted entity field is rejected in review and by boundary tests.

Crates below `nomifun-common` in the dependency graph may generate the same
canonical text directly with `Uuid::now_v7()` when importing the common type
would invert layering. They must have a test that parses the result and checks
canonical form, version, variant, and prefix.

### 2. Companion v7 reset coordinator

A focused companion contract-reset coordinator runs from application bootstrap
after the exclusive data-directory server lock is acquired and before
`CompanionService::start`. The production entry point requires a borrowed,
non-cloneable reset guard owned by `ServerEnvironment`; code that merely has a
path cannot invoke destructive cleanup. `CompanionService::start` stops running
legacy migrations and instead asserts that no pending reset exists and that the
store is already version 7 before it constructs config/registry consumers, the
live pool, collectors, learners, archivers, or background workers.

The coordinator owns the durable marker directory
`<data_dir>/companion/.id-contract-v7-reset.pending/`, outside the `shared`,
`companions`, `figures`, `workspaces`, and `models` paths it may inspect or
clear. Before `armed`, it durably installs an immutable `plan.json` containing
the reset mode, exact source-trigger enum and proof hash, source file-family
identity/fingerprints, detected versions, source and pre-minted target companion
installation IDs, the main installation identity and planned (possibly not yet
published) storage generation supplied by the bootstrap lifecycle context, the
exact SQLite-family existence bitmap and staging basename, the exact readable pre-reset
companion-thread conversation IDs, and any proven external memory-bridge
cleanup inventory. A hard-cut trigger is one of `pre_v7`,
`missing_with_residue`, `confirmed_corrupt`, `schema_violation`,
`value_or_owner_closure_violation`, or `skill_bijection_violation`; the proof
hash covers the exact evidence for that arm. The later immutable phase files are
`armed`, optional `family_transition_started`, optional
`source_family_cleared`, `store_committed`, `side_clean`,
`main_detached`, `memory_bridge_clean`, and `external_skills_clean`; phase
contents carry plan hashes and removal counts, never user content. The
immutable mode is `hard_cut` for a
pre-v7, corrupt, schema-invalid, value-invalid, or companion-skill
metadata/body-incoherent store and `side_cleanup` for cleanable filesystem
metadata beside an otherwise proven v7 store. A third `dataset_detach` mode is
authorized only by a durable main-dataset reset plan whose `main_committed`
predicate currently holds; it empties all companion business state and
main-dataset references while retaining the same canonical
profile/figure/workspace/model boundary as a companion hard cut. Its plan
records the main plan hash and generation. Creating a phase
means writing and `sync_all`-ing a uniquely named temporary file, renaming it
to the previously absent phase name, and syncing the marker directory. Arming
also syncs the `companion/` parent. Completion removes the marker directory and
syncs `companion/`. No destructive action starts until `armed` is durable.
If a crash leaves a marker directory without `armed` and without any later
phase, bootstrap verifies that no planned mutation occurred, removes only that
unarmed directory durably, and reclassifies the untouched stores. A later phase
without `armed`, a changed plan hash, or a main installation/generation that no
longer matches the plan is ambiguous and fails closed.

The v7 manifest contains a singleton `companion_contract_meta` row with
`contract_version = 7`, a canonical UUIDv7 `installation_id`, and an optional
`installed_by_plan_hash`. `fresh_create` pre-mints a new installation ID;
`hard_cut`/`dataset_detach` pre-mint another and install the exact plan hash in
the same transaction as the empty schema; `side_cleanup` retains the current
identity. Runtime journals bind to both main and companion installation IDs.
This row is reset provenance, not retained user data, and lets recovery
distinguish its own committed target from an unrelated valid v7 replacement.

Here and below, a SQLite source family is not only DB/WAL/SHM. Its exact
closed-family manifest covers the canonical main file plus fixed-basename
`-wal`, `-shm`, and rollback `-journal` members. A present rollback journal is
parsed from a bounded regular-file copy to determine whether it names a
super-journal. NomiFun never uses SQLite `ATTACH`, so a non-empty, malformed,
absolute, escaping, missing, or otherwise unprovable super-journal reference is
an unsupported/ambiguous transaction: bootstrap fails before arming or opening
the canonical DB and does not delete either journal. A proven single-database
rollback journal with no super-journal is copied, fingerprinted, quarantined or
removed as one fixed family member with its matching DB. WAL and rollback
journal modes present together are likewise ambiguous for an openable source;
they fail closed rather than letting SQLite choose one. A missing-main
hard-cut may remove planned orphan sidecars only after the same bounded journal
proof. Before a fresh/reset target is installed or opened, the canonical
`-wal`, `-shm`, and `-journal` paths must all be proven absent. This prevents a
hot rollback journal from being deleted or mismatched contrary to [SQLite's
rollback-journal rules](https://www.sqlite.org/howtocorrupt.html#deleting_a_hot_journal)
and prevents either WAL or rollback state from being applied to a different
database file.

Phase files are progress hints, not substitutes for the state they describe.
Before advancing or completing, recovery revalidates the full predicate for
every existing phase and repeats idempotent work when the predicate is false.
Before a phase is written, every created/changed file is synced and every
directory whose entries changed through create, rename, move, or unlink is
passed through the platform's `durable_entry_change` primitive. Linux uses a
parent-directory `fsync`. On macOS, every temporary/data file and every changed
parent directory is opened as a Rust `File` and passed to `sync_all()`, whose
Apple implementation requests `F_FULLFSYNC`; a plain libc `fsync` is not an
acceptable substitute because it does not provide the required ordered media
flush. Failure or lack of directory `F_FULLFSYNC` support retains the marker
and refuses startup. This follows [Apple's durability distinction for
`F_FULLFSYNC`](https://developer.apple.com/library/archive/documentation/System/Conceptual/ManPages_iPhoneOS/man2/fsync.2.html)
and the [Rust Apple `sync_all` implementation](https://doc.rust-lang.org/src/std/sys/fs/unix.rs.html).
Windows does not pretend that `FlushFileBuffers` has a
documented directory-handle contract: publications and moves use
`MoveFileExW(MOVEFILE_WRITE_THROUGH)`, while a removal first moves the entry
with write-through to the plan's exact private tombstone name and then deletes
it; the next write-through phase is recorded only after absence is re-proven.
Recovery inventories and finishes any such tombstone before service exposure.
A platform that cannot establish its required primitive fails with the marker
pending. Moves durably record both source and destination parents. Completing
a Windows marker similarly write-through-renames it to a recognizable sibling
completion tombstone before GC, so a crash cannot turn a reappearing marker
into fresh destructive authority.

Throughout this design, a plan hash means lowercase SHA-256 over the canonical
serialization of the immutable plan body with any self-hash slot omitted;
temporary files and phase records are not part of that digest. A proof hash is
likewise over an explicitly versioned, canonical evidence object. Thus no hash
definition is self-referential or dependent on recovery progress.

SQLite classification and trigger reproof never open a canonical source DB
family directly. A nominal read-only WAL open is not observationally pure:
SQLite may create missing `-shm`/`-wal` files when the containing directory is
writable. Before any migration-history, schema, value, owner-closure, or
`quick_check` query, bootstrap therefore runs a non-authorizing probe state
machine under the same exclusive guard:

1. Atomically install the bounded regular file
   `<data_dir>/.id-contract-read-probe.intent.pending` through the exact fixed
   `.intent.tmp` name and flush the data-dir parent. The fully synced intent
   contains only a purpose, operation UUIDv7, exact direct-child `building`,
   `ready`, and `gc` basenames, and the fixed internal member-name allowlist; it
   contains no source/external path and authorizes mutation only inside those
   three private roots. If a crash leaves `.intent.tmp` before publication,
   bootstrap may remove only that exact no-follow bounded regular file. A
   published intent cannot be partial because no-replace rename follows file
   sync; an invalid published intent fails closed.
2. In the intent's mode-0700, no-follow `building` root, snapshot and copy every
   present fixed-basename member of the exact closed SQLite family—DB, matching
   hot WAL/SHM, and rollback journal—under fixed internal names. Parse only the
   bounded private journal copy and continue only for the proven single-DB,
   no-super-journal form defined above. Sync every copy and the directory,
   re-read every source identity and
   fingerprint, write/sync the complete ready manifest, then no-replace-rename
   the whole root to its planned `ready` basename and flush the parent. Only
   that freshly published ready root may be queried. A hot WAL or rollback
   journal is never separated from its matching DB.
3. SQLite opens only the writable private ready copy, where it may create or
   rebuild its own SHM, apply/remove its copied rollback journal, or checkpoint
   without changing source evidence; `immutable=1` is not used to ignore hot
   state. The result remains in process memory. After close, rename the root to
   the exact `gc` tombstone, flush the parent, remove only allowlisted regular
   members, remove/flush the root, and remove the intent last. Recheck the
   still-closed source family after GC before deriving a proof hash or arming a
   plan.

At startup, a valid pending intent is cleanup authority only: any partial
`building`, SQLite-mutated `ready`, partial `gc`, or combination reachable by a
crash is discarded member-by-member and the probe restarts from source. Missing
or truncated allowlisted members are expected partial progress. Each private
root must be the exact recorded direct-child real directory and may contain
only fixed-name bounded regular files; a symlink/reparse point, nested
directory, extra entry, changed root identity, or name outside the allowlist
fails startup without touching it or any source. Cleanup is idempotent on
Windows tombstones and Linux/macOS directory flushes. Armed trigger reproof
uses a new probe run and compares the unchanged source family with the
immutable plan immediately before it durably records transition authority.
This preserves the DB/WAL/rollback pairing required by [SQLite's hot-journal
guidance](https://www.sqlite.org/howtocorrupt.html#mispairing_database_files_and_hot_journals)
and avoids the sidecar creation documented for [read-only WAL
access](https://www.sqlite.org/wal.html#read_only_databases).

The algorithm is:

1. Use non-destructive `fresh_create` only when the complete identity-bearing
   closure is absent: no DB/shared payload, profile directory entry, figure
   index/image/other directory entry,
   companion-owned main conversation/reference/token/binding/session,
   evolved-skill root, `.ops-v7`/import journal, operation commit/projection
   outbox, or legacy layout exists. Unowned orphan
   workspaces and model-cache files may exist. Create v7 at a unique sibling
   staging path, close/sync/fully prove it, then atomically install it only if
   the canonical path is still absent and sync `shared/`. A missing DB beside
   even canonical profiles/figures is a lost/partial store, so it takes
   `hard_cut`; retention still preserves those canonical assets while the mode
   cleans ghost main references and external residue.
2. If an existing store's detected version is greater than 7, fail without
   arming or writing.
   For a structurally valid v7 store, first inventory any v7 operation/import
   journals and SQL commit/outbox rows. A fully hashed legal journal authorizes
   only its planned transient mismatch and is recovered under the bootstrap
   barrier before final closure classification; an impossible/missing/hash-
   mismatched journal fails closed. Ordinary incomplete operations therefore do
   not masquerade as corruption and trigger a destructive hard cut.
   If the store independently proves a pre-v7, missing-with-residue,
   confirmed-corrupt, schema-invalid, or value-invalid `hard_cut` trigger, the
   reset plan instead freezes every parseable journal and its planned main/path
   footprint and supersedes it under the broader reset authority. It does not
   require commit/outbox reads from an unreadable DB. The normal hard-cut main
   scan removes the complete typed companion footprint, valid planned bridge
   paths use the proven bridge inventory, and local journal/commit/projection
   residue is cleared with the reset tables. A malformed/unarmed journal is not
   authority to touch an external path; it is discarded only as local
   app-private residue after the main closure is clean. This destructive branch
   is available only after the hard-cut trigger itself is re-proven under the
   reset guard.
3. Treat any other existing store as proven current only when
   `PRAGMA user_version = 7` and
   runtime schema- and value-contract checks confirm every required table,
   index, ID constraint, persisted ID value, and the absence of retired tables.
   The schema proof is an exact manifest and normalized SQL hash for every
   allowed user table, index, view, trigger, and virtual-table shadow object
   (apart from named SQLite internals), plus `table_xinfo`, foreign-key,
   unique-index, and declared `CHECK` structure; any extra, missing, or altered
   object is a contract violation.
   The proof uses one offline roster snapshot and also requires owner/reference
   closure: every memory scope, thread, runtime state, skill, session window,
   suggestion action, and declared nested owner resolves to the final roster;
   every thread/window conversation resolves in the proven main DB and its
   typed companion/session markers agree. A syntactically canonical orphan is
   a contract violation and starts `hard_cut`, not best-effort reconciliation.
   An existing marker resumes its recorded mode. A lower version, malformed
   current schema/value, or non-empty shared data without that proof starts a
   `hard_cut`. Beside a proven store, invalid profile/figure metadata, malformed
   shared config or sequence state, an invalid/unknown event journal, or an
   unrecognized app-private `shared/` entry starts `side_cleanup`. A mismatch
   between `companion_skills` metadata and any managed evolved-skill body starts
   `hard_cut`, because selecting one side as historical truth would be an
   implicit compatibility migration. Before arming `side_cleanup`, compute the
   profiles it would remove and prove that no companion table, main structured
   reference, channel preference, access token, knowledge binding, or managed
   skill path is owned by any of those IDs. If any owner reference exists,
   classify `hard_cut` before the immutable mode is armed; `side_cleanup` is
   permitted to keep database rows/main references byte-identical only after
   this zero-reference proof.
4. Durably create `armed`.
5. Every `hard_cut` and `dataset_detach` uses one closed-family replacement;
   no destructive mode opens or rewrites the canonical source with SQLite. If
   `family_transition_started` is absent, prove every handle closed and use a
   fresh private probe to re-prove the immutable trigger/proof hash, complete
   original family bitmap/identities/fingerprints, and any locked owner/skill
   closure. `missing_with_residue` additionally proves the canonical main file
   absent; `dataset_detach` instead re-proves its authorizing main-plan hash and
   `main_committed`. Only while every member is still in its original planned
   state does the coordinator durably record `family_transition_started`,
   immediately before the first family mutation.

   Once `family_transition_started` exists, recovery deliberately does not try
   to recompute the old trigger or original bitmap from a family that this plan
   may already have changed. It validates the immutable plan/phase hash and the
   member transition lattice instead. Each originally present `-shm`, `-wal`,
   proven rollback `-journal`, and finally main DB is accepted only as the exact
   original present value, the plan's exact Windows removal tombstone, or
   already absent due to this transition; an initially absent member must stay
   absent. For `missing_with_residue`, only its planned orphan sidecars enter
   that lattice. Flush `shared/` after each member change and prove the complete
   canonical family absent before recording `source_family_cleared`, including
   an originally empty missing-with-residue family. Profiles, figures, main
   references, and other residue are not SQLite-family members and remain for
   later typed cleanup/retention phases. A changed/reappearing identity or any
   state outside the lattice fails closed with zero further deletion.

   Create the exact v7 manifest at the plan's unique sibling staging basename;
   in one `synchronous=FULL` transaction write the pre-minted companion
   installation ID and plan hash and set `PRAGMA user_version = 7` last. Close,
   checkpoint to a standalone main file, sync, and run the full proof against
   the staged target, then atomically install it only while the canonical main,
   `-wal`, `-shm`, and `-journal` paths remain absent. Recovery accepts a
   canonical DB after transition only when it is that complete empty target
   identity carrying this plan hash and has no stale sidecar; that proves a
   prior staged install committed before its later phase. SQLite is never
   allowed to auto-create a main file beside old hot state.
6. For `side_cleanup`, no family transition phase is legal. Immediately before
   the first side-file mutation, a new private probe must re-prove the exact
   current-v7 schema/value/owner closure, original family fingerprints, and the
   armed zero-reference side-cleanup proof. A mismatch retains the marker and
   performs zero cleanup. For a destructive mode, step 5's exact empty target
   provenance is mandatory. The coordinator never escalates or repurposes an
   immutable mode.
7. Consequently every destructive trigger—including a valid populated v7 DB
   beside an incoherent evolved-skill store—starts from only the latest empty
   manifest rather than dropping/migrating selected historical objects or
   silently choosing one side. Unknown tables, retired `pet_*` objects, and the
   v6 quarantine ledger disappear only because the entire old family is gone;
   no row recovery or compatibility SQL runs against it.
8. Leave rows untouched only for a valid-v7 `side_cleanup`, or when an
   interrupted `hard_cut`/`dataset_detach` already satisfies the complete empty
   reset-table predicate and its metadata carries the plan's exact target
   installation ID and hash.
9. After the committed store passes the complete schema/value proof and its
   bootstrap handle is closed, durably create `store_committed`.
10. In `hard_cut` or `dataset_detach`, remove every `shared/` entry except the
   committed `memory.db` family, so old config, sequence state, events, import
   staging, and unknown files cannot survive; also remove pending/completed v2
   import journals and v7 operation journals whose SQL commit/outbox tables
   were reset. Load defaults on the next
   service start. In `side_cleanup`, retain a strictly valid current config and
   event corpus byte-for-byte, but delete a malformed config, rebuild a missing
   or malformed sequence file from the safe watermark, clear the entire event
   corpus if any file/line/kind/declared ID is invalid, and delete unknown
   app-private shared entries. It never copies or repairs an invalid record.
11. In every mode, discard legacy layouts and remove invalid profile/figure
   metadata while preserving workspaces and other approved current data. The
   mode's typed profile rewrite clears every dangling optional catalog/figure
   binding and invalid optional non-identity field in `hard_cut`, `side_cleanup`,
   and `dataset_detach`; it never depends on changing the immutable mode. In
   `dataset_detach`, that rewrite additionally clears every provider/model,
   applied-preset, and other declared main-dataset reference even when the old
   target still resolves, without changing the companion ID, persona, figure
   binding, or workspace.
   Reconstruct the display sequence watermark as the maximum `seq` of retained
   profiles and every leading numeric component in a direct top-level workspace
   leaf. New allocation must additionally skip any destination path that
   already exists, so an orphaned workspace can never attach to a future
   profile through sequence/name reuse. Sync every changed parent, verify the
   complete side-store predicate, and durably create `side_clean`.
12. In `hard_cut`, clear the stale companion footprint in the already-proven
    main database through one `BEGIN IMMEDIATE`, `synchronous=FULL`
    transaction. The deletion set is the union of the conversation IDs frozen
    in the plan and every main conversation whose schema-aware `extra` marks it
    as a companion session or carries a typed `companion_id`. Delete those
    conversations and their non-FK IDMM audit rows, but first explicitly delete
    every `channel_sessions` row whose `conversation_id` is in that frozen set:
    its real FK is `ON DELETE SET NULL`, so relying on cascade would leave a
    reusable ghost session. Normal foreign-key cascades remove the remaining
    messages, artifacts, and links. Clear every remaining channel session owned
    by a companion-bound plugin so the next inbound turn creates a fresh
    conversation. The final predicate proves no session remains from the
    deleted conversation set even when its plugin was already unbound/deleted.
    Compute the final
    retained roster from the just-cleaned canonical profiles: preserve
    channel-plugin bindings, channel preferences, access tokens, and knowledge
    bindings only for IDs in that roster, and delete/null every such reference
    whose owner is absent. `dataset_detach` instead proves that the new main
    dataset contains none of those references; `side_cleanup` proves that it
    did not change them. Revalidate the transaction predicate before writing
    `main_detached`.
13. When `hard_cut`/`dataset_detach` clears a config with
    `bridge_to_memory_dir`, or `side_cleanup` resets such a config, clean only
    the proven companion-owned mirror set frozen in the plan. Preflight builds
    one evidence graph from strict config, a validated readable
    `companion_memory_mirrors` ledger, and every fully hashed legal pending
    bridge-operation plan before any of them is cleared. Config is not
    privileged over stronger ledger evidence. Each root has one of three frozen
    states: `not_configured` only when all available sources agree that no root
    or ledger projection exists; `proven_owned` when the evidence graph closes
    over an absolute root plus exact source ID/filename/body/index hashes (so a
    malformed config with a complete ledger remains cleanable); or
    `unreachable_unproven` only when every durable source fails to recover the
    historical destination. Different roots or hashes are ambiguous and fail
    closed unless one valid pending bridge-change plan exactly explains its old
    and new roots and current phase. The last state still
    deletes/quarantines the local config and records one content-free safety
    warning, but never guesses or scans/deletes an arbitrary user directory;
    after reset the companion has no reference to it. For `proven_owned`, hold
    the same cross-process per-root lock as the `nomi-memory` writer and require
    the frozen index hash before replacement, so a concurrent append is neither
    overwritten nor mistaken for reset residue. Under no-follow
    ancestor checks, an owned mirror is a regular `companion-*` memory file
    whose strict companion frontmatter, name, body, and recomputed content hash
    agree (and, when the old DB is readable, agree with a source memory). Its
    `MEMORY.md` entry is frozen when present but is not required because the
    existing bridge write treats index append as best-effort. Any owned-looking
    ambiguity, symlink/reparse point, changed hash, or unprovable remaining
    memory fails closed before deletion. Remove only those frozen files, then
    atomically rebuild and sync `MEMORY.md` from the remaining proven
    non-companion memories; never delete the external root or unrelated
    memories. Record `memory_bridge_clean` only after the proven files are
    absent and the remaining index/file bijection is durable, or after the
    `unreachable_unproven` detachment predicate proves no live companion
    config/ledger/journal retains that path.
14. In `hard_cut` or `dataset_detach`, durably remove the three
    companion-evolution managed roots `skills/companion/`, `skills/_drafts/`,
    and `skills/shared/`; other user skill roots are outside this ownership
    boundary and remain untouched. A `side_cleanup` proves the current
    metadata/body bijection without changing valid skill files. Record
    `external_skills_clean` only after the no-follow absence/bijection predicate
    holds and every changed parent is flushed.
15. Verify the resulting store, side-store scan, main-detach predicate, memory
    bridge, and external-skill predicate again even when every phase already exists. If a
    retained profile changed after `main_detached`, repeat the idempotent side
    cleanup and main transaction against the new final roster before durably
    removing the marker directory.

The list of companion tables reset as one unit is:

- `companion_memories`;
- `companion_suggestions`;
- `companion_learn_runs`;
- `companion_state`;
- `companion_threads`;
- `companion_runtime_state`;
- `companion_skills`;
- `skill_pattern_stats`;
- `evolution_feedback`;
- `companion_session_windows`;
- `companion_import_commits`;
- `companion_operation_commits`;
- `companion_projection_outbox`;
- `companion_memory_mirrors`;
- `companion_id_v6_quarantine`.

`companion_contract_meta` is not retained business state: hard cut and dataset
detach replace it with the planned target provenance in the same transaction;
side cleanup verifies and retains it.

Fresh databases are born directly at version 7. No v0-to-v6 migration rung is
executed for a preexisting file. Re-running after a crash is idempotent: a
durable `armed` marker plus the mode-specific version-7
schema/value/emptiness predicate and exact target installation/plan provenance
proves that the DB phase committed even if `store_committed` was not yet
written. Otherwise the original trigger and source proof must still match under
the write lock; the mode string alone proves nothing. The data-dir guard
separately prevents a second production process from entering at all.

The existing single-connection bootstrap remains. The multi-connection live
pool is created only after the reset transaction and side cleanup finish. All
formal writes/imports reject invalid current-layout profile, figure, config,
event, sequence, and evolved-skill entries. The strict scan also runs before
every version-7 registry scan. Cleanable side-store tampering discovered on a
later boot arms `side_cleanup` before removal and must complete before the
service becomes visible; valid shared events/configuration remain
byte-identical. A cross-media skill mismatch arms `hard_cut` because it
invalidates the database metadata/body unit rather than merely one filesystem
index.

### 3. Retained and discarded companion filesystem data

The `hard_cut` mode applies these exact retention rules; `side_cleanup` applies
the profile/figure/workspace rules plus the strict shared-side-store actions
declared below:

| Path/domain | v7 action |
| --- | --- |
| `companion/shared/memory.db*` | Rebuild business schema; do not preserve rows |
| Every other `companion/shared/` entry | In `hard_cut`/`dataset_detach`, delete config, sequence state, events, import staging, and unknown files. In `side_cleanup`, keep only exact current-schema config/events, rebuild sequence state, and clear the whole affected private corpus rather than retaining an invalid record |
| `companion/companions/<id>/` | Preserve only when directory name and embedded profile ID are the same canonical `CompanionId` and the profile validates |
| `companion/workspaces/` | Preserve the entire tree; the companion-only reset never follows symlinks, deletes, or reattaches these user project files |
| Legacy profile-local `workspace/` | Before deleting invalid profile metadata, move a regular non-symlink workspace to `companion/workspaces/orphaned/<uuidv7>/`; if that move fails, retain the source and fail cleanup; remove only the symlink itself when the entry is a symlink |
| `companion/figures/` | Preserve only an entry whose index is a regular non-symlink file and parses, whose typed ID matches its canonical derived regular-file name, and whose image is a regular non-symlink file; remove invalid entries/files; a corrupt index makes every figure unprovable, so remove its managed images before installing an empty index |
| `companion/models/` | Preserve unchanged; it is a regenerable non-identity cache |
| `skills/companion/`, `skills/_drafts/`, `skills/shared/` | Companion-evolution managed content: clear in `hard_cut`/`dataset_detach` so reset `companion_skills` metadata cannot leave executable residue; preserve only when a current-v7 `side_cleanup` proves the metadata/body bijection |
| Legacy `pet/`, `pets/`, or `companion/nomi/` data | Before deletion, extract every recognized regular workspace or managed `workspaces/` tree into a fresh `companion/workspaces/orphaned/<uuidv7>/`, and move any regular legacy `models/` tree unchanged under `companion/models/orphaned/<uuidv7>/`; use the same move-or-fail rule. Delete the remaining legacy data; do not import profiles, figures, or IDs |

The scan is strict and repeatable. A filesystem removal failure leaves the
marker pending and prevents companion workers from starting, so stale data
cannot become live; cleanup is not a best-effort operation.

The shared-side predicate is schema-aware. Config is exact current-schema JSON;
every declared entity field (including `default_companion_id`, provider/model,
preset, MCP, and knowledge references) parses with its newtype, every roster
reference resolves, and every declared main-catalog reference resolves in the
same proven final-main snapshot. A canonical-but-missing optional catalog
snapshot is cleared as a whole by the typed config rewrite in the already
selected mode; no replacement ID is invented. Sequence state is an exact bounded
integer object whose watermark is at least the safe maximum derived below.
Events are regular JSONL files from an explicit kind registry; each kind has an
exact payload decoder and typed ID slots. Unknown kinds, unknown payload shapes,
malformed lines, unresolved owned references, and prefix/type mismatches make
the complete event corpus unprovable. The shared directory itself has an exact
entry whitelist covering the DB family, current config/sequence/event layout,
and the v2 import journals; an unlisted app-private entry is not silently
ignored.

Workspace extraction is deliberately narrow: candidates are a direct
`workspace/` child of `companion/nomi/` or `pet/nomi/`, or of one profile
directory under `pet/companions/`, `pet/pets/`, `pets/`,
`companion/pets/`, or `companion/companions/`, plus an exact managed root at
`pet/workspaces/`, `pets/workspaces/`, or `companion/nomi/workspaces/`. A
managed root is moved as one tree without traversing or following links inside
it. Before inspecting or moving a candidate, every ancestor from the data root
is verified with no-follow metadata to be a real directory rather than a
symlink or Windows reparse point. If that proof fails, cleanup retains the
whole legacy root and fails closed. The scan does not recursively treat an
arbitrary directory named `workspace` as user content and never reads a legacy
ID to construct the orphan destination.

Legacy model salvage recognizes only `pet/models/`, `pets/models/`, and
`companion/nomi/models/`. It moves a regular non-symlink root as one inert
orphan tree; it never promotes legacy configuration or identity metadata into
the active model cache. The same all-ancestors no-follow/reparse proof applies.

A profile identity envelope is valid only when `config.json` is a regular
non-symlink file that decodes as the current `CompanionProfileConfig`, the
directory and embedded IDs are the same canonical `CompanionId`, and every
declared durable ID in its model, applied-preset snapshot, and custom-figure
metadata parses through its registered type. Provider, preset, MCP, knowledge,
and other declared main-catalog IDs must also resolve against the same final
main snapshot. A missing optional catalog target clears its complete optional
model/snapshot/binding with the mode-local typed profile rewrite while retaining
the profile identity; an identity-envelope/owner target cannot be repaired and
classifies `hard_cut`. A retained profile's optional
`figure.webp` must be a regular non-symlink image that passes the existing
magic, byte, and dimension limits. Once the identity envelope passes, a
dangling canonical library-figure reference, invalid non-ID appearance
metadata, or invalid optional image is removed with the same mode-local typed
profile rewrite;
no replacement ID is invented. An unparseable ID fails the identity envelope.
Failure to prove that envelope removes the profile only after workspace
salvage.

A library index is provable only when it is strict current-schema JSON with
unique canonical `FigureId` entries and valid finite metadata. Duplicate IDs
make the index corrupt rather than allowing an arbitrary winner. Cleanup first
inventories direct directory entries using their returned safe paths; it never
joins an unvalidated index string into a deletion path. A managed-image
candidate is any direct regular non-symlink `.webp` child of `figures/`, and it
is provable only when named exactly `<FigureId>.webp`, passes the existing
image validation limits, and is referenced by exactly one retained entry. The
retained index and provable managed images therefore form a bijection.
Unindexed, invalidly named, duplicate, missing, and malformed images/entries
are not retained; every other unexpected direct entry is removed no-follow
from this managed directory. Cleanup writes and syncs the complete cleaned
index through an atomic rename, rewrites every retained profile whose canonical
`figure_id` became dangling to clear that binding, then removes only inventory
paths excluded by the index. The pending marker makes every crash position
resume and re-prove the bijection before service exposure.

### 4. Companion typed storage boundaries

Persisted companion records use existing typed IDs rather than public `String`
fields for:

- `CompanionMemory.id` and optional scope owner;
- `CompanionSuggestion.id`;
- `CompanionLearnRun.id`;
- `CompanionThread.conversation_id` and `companion_id`;
- `SessionWindow.id`, `companion_id`, and `conversation_id`;
- runtime-state companion owner and the `companion_active_thread` value;
- `CompanionEvolutionFeedback.id`.

The storage boundary has three defenses:

1. **Domain writes:** constructors and store methods accept typed IDs or parse
   an external string before beginning a write.
2. **SQLite constraints:** every fixed ID column checks the exact prefix,
   canonical length and separators, lowercase hex alphabet, UUIDv7 version
   nibble, and RFC 9562 IETF-variant nibble (`8`, `9`, `a`, or `b`). Raw SQL
   cannot insert a retired short ID, compact UUID, uppercase UUID, UUIDv4, or
   wrong prefix.
3. **Disk reads:** every mapper reconstructs the typed ID and returns a
   controlled internal-invariant error on malformed persisted data. It never
   serializes malformed text to HTTP or WebSocket.

This checked-read rule covers current config/sequence, registry profiles,
figure index, event/stats JSONL, evolved-skill body/projection, and every list/
search/export path. A genuinely missing optional file may select an explicitly
documented default; a present-but-invalid file/row never becomes a default,
empty description, filtered entry, warning-and-continue, or partial list. If
tampering occurs after boot proof, the first checked read closes the companion
visibility barrier and reports recovery-required until bootstrap recovery or a
durable `side_cleanup` re-establishes the predicate.

Tagged JSON receives a schema-aware validator rather than a recursive
string-prefix heuristic. In particular, a `create_skill` suggestion action
validates its `companion_id`, and the runtime state validates a conversation ID
only for the key whose value is defined as `companion_active_thread`. User text
that merely begins with `wsa_`, `conv_`, or another prefix is not treated as an
entity reference.

Generation uses the registered newtype's `new()` method whenever the crate can
access it. The learner mints `CompanionLearnRunId::new()`; suggestion, window,
memory, and feedback creation follow the same rule.

#### Runtime cross-store mutation protocol

The reset-time closure must remain true after normal use. Companion operations
that span the main DB, companion DB, or filesystem therefore share a v7 durable
operation journal and visibility barrier under
`companion/.ops-v7/pending/<operation-uuidv7>/`. Plans are typed, hashed, synced,
and installed before the first mutation; SQL transactions record an outbox row
last; filesystem projection uses the atomic primitives below. Recovery either
finishes the exact committed plan or applies its explicitly declared
compensation. It never relies on a best-effort hook.

The barrier is one process-wide lifecycle permit, not a convention local to
`CompanionService`. Every entry point that can create, bind, update, or delete a
companion-owned reference takes a shared permit before reading its owner and
holds it through its final DB/filesystem commit. This includes conversation
create/`extra` update, channel setting/plugin/session bind, generic preference
batch, access-token and knowledge binding, memory/skill/bridge mutation, and
their internal/background variants. After acquiring the permit, each writer
also rejects an owner present in the durable pending-owner tombstone registry.
The registry is built from fully validated operation journals before routes or
workers are published; a malformed journal fails startup rather than leaving
the target writable.

A companion delete takes the exclusive lifecycle permit and drains every
shared holder before it inventories anything. While still exclusive, it
installs the hashed journal and pending-owner tombstone, executes every phase,
re-proves the final cross-store closure, removes the profile last, clears the
journal/tombstone durably, and only then releases the permit. All DB
transactions and per-root bridge/skill locks are acquired after the lifecycle
permit in one documented lock order. A writer that started before deletion is
therefore drained before the frozen plan, and one that starts later cannot add
a reference between main detach and profile removal. Bootstrap recovery owns
the same exclusive permit before any service exists.

Deleting a companion freezes its conversation/session/reference and managed
path inventory. It deletes the exact main conversations, channel sessions,
preferences/bindings/tokens/knowledge references, every companion-DB owner row
and declared JSON reference (including private memories, suggestions, runtime,
skills, feedback, and session windows), and that owner's private/draft skill
projections. The workspace is moved to an unattachable orphan name rather than
deleted. Only after every DB/skill/workspace predicate holds does it remove the
profile and complete the journal. The profile is therefore the last ownership
anchor removed; any failure leaves a resumable operation and blocks exposure of
that owner. Existing post-removal best-effort cleanup hooks become notifications
after the durable predicate, not cleanup authorities.

Session plans have explicit non-overlapping variants. A `companion_thread` plan
pre-mints typed companion/conversation IDs, owner, workspace, and idempotency
key; it inserts the exact marked main conversation and the single
`companion_threads` row allowed for that companion. A
`companion_channel_binding` plan binds one exact `channel_sessions` row to that
shared thread conversation; multiple channel sessions may point to the same
thread, but none inserts another companion-thread row. An
`unbound_channel_session` atomically creates/binds its main conversation and
channel-session row entirely in the main DB and never touches
`companion_threads`. Each phase is journaled; recovery finishes the exact
matching variant when its owner/main row still agrees, otherwise deletes only
the planned main/session rows and preserves the workspace. A crash between
stores cannot leave an unjournaled hidden conversation, and the closure scanner
requires the mode-specific main/thread/channel bijection.

The v7 `companion_skills` schema stores validated canonical Markdown and its
SHA-256 hash as the single truth together with metadata/status. Files are
recoverable projections with one exact state predicate: `draft` has only
`skills/_drafts/<owner>/<name>/SKILL.md`; active private has only
`skills/companion/<owner>/<name>/SKILL.md`; active shared has only
`skills/shared/<name>/SKILL.md`; archived/rejected has no executable projection.
Create, edit, accept, reject, gift, archive, and delete commit the body/status
plus projection-outbox row atomically, then publish/remove synced files and
clear the outbox behind the skill visibility barrier. Accept removes the draft;
reject removes every executable projection. Boot recovery regenerates exact
projections from DB truth before any agent injection. Direct truncate/write,
file-first DB inserts, and warn-only rollback paths are removed.

When the external memory bridge is enabled, the v7 DB maintains a typed
`companion_memory_mirrors` ledger containing source memory ID, absolute root,
filename, body hash, and projection provenance. Save/update/delete/archive and
reactivation mutate the memory, ledger, and mirror-outbox in one companion SQL
transaction; the proven file and index projection are atomically
published/replaced/removed before the operation reports success. Recovery
holds the same cross-process per-root lock as the `nomi-memory` writer and uses
an index hash compare-and-swap before atomic replacement; it retries the exact
hash/path, so bridge projection is no longer a best-effort
append that can create untracked partial state. Disabling or changing the bridge
is its own durable detach operation: freeze/clean every old-ledger projection
and old index entry, prove the old root clean, update config last, then enable
the new root. Factory/reset planning reads both strict config and this ledger
before either is cleared.

### 5. Companion export/import contract v2

The companion and memory bundles implemented by `nomifun-companion::export`
keep `format: "nomifun-export"` and move from version 1 to exactly version 2.
Readers accept exactly version 2; the previous `<= current` behavior is
removed.

Before any domain row, profile, event, or managed file is written, import
performs a complete preflight:

- exact manifest format, kind, and version;
- a kind-specific entry whitelist, rejection of duplicate archive names,
  backslashes, colons, absolute/parent paths, symlinks, and non-regular files;
- at most 4,096 file entries, 64 MiB uncompressed per entry, and 512 MiB total
  expanded bytes, enforced both from ZIP metadata and by bounded streaming;
- typed deserialization of every memory, learn run, profile, owner, and state
  reference;
- schema-aware validation of every imported event kind and its declared ID
  fields;
- a complete immutable import plan containing normalized records, destination
  paths, content hashes, skips, and conflicts.

Archive extraction and content-only validation may run before the visibility
barrier closes. The destination-aware portion—duplicate queries, conflict
checks, clone-ID allocation, reference existence, and destination path checks—
runs only after import holds the exclusive barrier permit and has drained all
shared permits. It rechecks every destination immediately before hashing and
durably validating the immutable plan, so normal service activity cannot make
the plan stale between preflight and commit.

Unknown event kinds or untyped event payloads are rejected rather than copied
as opaque JSON. Memory merge keeps the existing active-near-duplicate skip. A
same-ID record is idempotent only when every normalized persisted field is
equal; any differing field is a conflict. The same rule applies to learn runs.
An event destination that already exists is skipped only when its bytes hash
identically and otherwise conflicts. A companion bundle is an explicit clone:
preflight mints a new canonical `CompanionId` and rewrites every declared
internal reference through a complete map. No v2 path silently overwrites a
same-ID or same-path object.

SQLite and the filesystem cannot share one atomic transaction, so import uses
a durable recovery protocol rather than claiming cross-media atomicity. Each
preflight is staged under
`<data_dir>/companion/.import-v2/pending/<canonical-uuidv7>/`. Its immutable
`plan.json` and payload files are hashed and synced before the durable phase
file `validated` is created. The v7 SQLite schema contains
`companion_import_commits(import_id, plan_hash, kind, committed_at)`. One SQL
transaction on a dedicated `synchronous=FULL` connection applies every planned
database mutation and inserts that commit row last. The row, not the later
filesystem phase file, is authoritative proof that the database transaction
committed.

After SQL commit, the importer records `db_committed`. Each planned file is
copied to a unique sibling temporary file in the destination directory, hashed
while bounded, `sync_all`-ed, and published with atomic no-replace semantics;
the destination parent is then flushed. A final path already present is complete
only when its full hash equals the immutable plan; any other preexisting value
is a conflict and is never overwritten. A crash can therefore expose only the
old/absent destination or the complete planned bytes, never a partial final
file. After every final hash and directory predicate passes, it records
`files_installed`;
phase files use the same temp-write, `sync_all`, rename, and parent-directory
sync protocol as the v7 reset marker. It then reloads the affected roster state
behind the closed barrier and atomically renames the whole pending journal to
`companion/.import-v2/completed/<import_id>/`, syncing both journal parents.
That directory name is the durable completion tombstone. Only after the rename
does a small SQL transaction delete the commit row. Visibility then reopens and
the live request emits/returns one success carrying `import_id`; boot recovery
never replays an old success notification. Completed directories are private
GC residue and may be removed recursively after the commit row is gone. A
partial GC remains unambiguously completed by its parent directory and is
retried without closing visibility.

A crash before SQL commit has no domain writes and its pending journal is
discarded. A crash after SQL commit with a pending journal completes forward
from the authoritative commit row. A completed journal with a commit row only
needs row cleanup; a completed journal without one only needs GC. A pending
journal without `validated` and without a SQL commit row is private extraction
debris and is removed. A commit row without its validated hashed pending plan,
a later phase without `validated`, a missing journal for a commit row, or any
hash mismatch is ambiguous and fails closed instead of guessing or rolling
back one medium independently of the other.

`CompanionService` owns an import visibility barrier. Every externally visible
companion read/write other than the importing request, every WebSocket
publication, and every collector, learner, archiver, and registry mutation
holds a shared permit; import closes the barrier and drains existing permits
before it commits. In-memory roster publication happens after
`files_installed`, while success events happen only after journal finalization.
If forward recovery cannot finish, the barrier remains closed and companion
APIs report recovery-required rather than exposing a partial bundle. Bootstrap
resolves all journals after the v7 reset coordinator and before constructing
companion workers. Thus schema/ID validation failure produces zero writes to
the companion domain and removes its private staging journal, while a later
operational failure may leave a durable private journal but never a partially
visible companion domain.

Export reads through the same checked mappers. A poisoned row therefore blocks
export instead of producing a bundle that can spread invalid data. Online
export holds a shared visibility permit. It never packages reset markers,
pending/completed import journals, or import-commit coordination rows.

The full offline backup/restore path must prove that an included companion
domain is version 7 and passes the same offline profile, figure, event, and
SQLite ID checks before the final destination-directory rename. The offline
backup format moves from version 1 to exactly version 2 and adds
`side_store_contracts: {"companion": 7}` when the companion domain is present;
readers accept exactly version 2. The field is a declaration, not sufficient
proof: backup and restore also inspect the staged companion database and typed
JSON/JSONL files. Backups whose manifest or companion domain predates this
proof are rejected. Restore never installs them and never defers identity
validation to a later boot. Offline backup is strictly read-only with respect
to every canonical source family and side store: it requires an already
installed canonical `storage-generation` and never creates one. Private probe
and bundle-staging files are the only writes. It
rejects every lifecycle marker (factory, main, storage-generation, companion,
Workshop, public-agent, preview), every pending/completed runtime/import journal
other than explicitly excluded completed GC tombstones, and any import commit,
operation commit, projection outbox, or other unfinished coordination row.
Restore rejects coordination artifacts in a bundle rather than trying to resume
a source machine's operation.

Version 2 packages the companion-evolution managed roots
`skills/companion/`, `skills/_drafts/`, and `skills/shared/` together with the
companion DB, and proves the exact metadata/body bijection before backup and in
the restore staging tree. The arbitrary external `bridge_to_memory_dir` is not
portable and its mirror files are derived duplicates, not archive members.
Version-2 restore explicitly sets that field to `None` in staged current-schema
config and requires the user to re-enable a destination bridge after restore;
it never writes the source Windows/macOS/Linux path on another machine. The same
staging transaction empties `companion_memory_mirrors` and every bridge outbox,
so no source absolute root survives invisibly in SQLite. It also mints a new
destination `companion_contract_meta.installation_id` and records the restore
plan hash; a source installation identity is never cloned across machines.

Backup never packages a live SQLite family byte-for-byte. For both the main and
companion databases, while the source is locked and no live pool exists, it
reuses the non-authorizing intent/building/ready/GC protocol above with
`purpose = offline_backup`: copy the closed DB plus matching hot WAL/SHM or
proven no-super rollback journal into the private ready root, verify that the
canonical source bitmap/identities/fingerprints did not change, and let SQLite
recover/checkpoint only that copy. SQLite's backup mechanism then writes a
second staged standalone DB in the bundle staging tree. The staged artifact is
quick-checked and runs the complete main-v3 or companion-v7 schema/value proof;
the private family is GC'd through its intent before publication, and the
source family is fingerprinted once more. A source change, malformed journal,
non-empty/unprovable super-journal reference, or probe/staging failure aborts
without a publishable bundle. Only the standalone main file is packaged;
`-wal`, host-local `-shm`, and rollback `-journal` are excluded. Restore proves
that standalone file and lets the first destination open create fresh
sidecars.

The manifest represents managed absolute paths as logical `data_root` and
`work_root` references rather than source-machine strings. Restore expands them
against the selected destination roots and rewrites every registered managed
path slot in the staging DB/config/profile set—including conversation
`extra.workspace` and companion workspace references—before running the final
schema/ID/path proof. Source/destination separators are normalized through path
components, so Windows↔macOS/Linux restore cannot leave a source-OS absolute
path live. A path outside declared logical roots rejects restore unless the
field has the explicit reset-on-restore policy above; it is never preserved by
accident. Only after this closed
graph plus evolved-skill files proves current does the destination directory
install atomically.

### 6. Main structured-reference contract and dataset hard-cut coherence

The main database replaces its generic value scanner with one authoritative,
schema-aware reference registry. Every durable slot is declared as:

```text
(table, column, optional row-key selector/discriminator,
 optional JSON pointer/cardinality, exact entity type or explicit union,
 nullability, referential rule)
```

A fixed column therefore invokes the registered parser for its expected type;
it never accepts an arbitrary syntactically valid prefix. Explicit polymorphic
slots select a finite parser union from their discriminator. Natural catalog
keys are accepted only in named union arms. In particular,
`agent_execution_events.actor_id` is checked against `actor_type` (`system`
requires no actor, `user` requires `UserId`, and `agent` requires exactly
`ConversationId | CompanionId`) together with its context columns. The missing
`creation_tasks.node_id` slot is registered as `WorkshopNodeId`. Values such as
`evil_<uuidv7>` or a `conv_<uuidv7>` in a creation-task ID column fail even
when their UUID and foreign-key graph are otherwise self-consistent.

One reusable `AgentReference` union removes the existing ambiguity between
entity IDs and catalog keys. Its entity arm is exactly `AgentId`. Its only
natural-key arms are a resolving `agent_builtin_*` row or a resolving,
namespaced extension-manifest agent key; the referenced `agent_metadata`
source/type/backend must agree with the enclosing discriminator. An arbitrary
string or backend label is not an agent reference. A field whose contract
explicitly permits a backend selector resolves it uniquely first and persists
the resulting `AgentReference`.

JSON storage uses the same registry with a kind/tag-aware decoder, never a
recursive search for strings ending in `_id`. The initial registry must cover
the complete current schema; its nontrivial review-visible entries include:

- `creation_tasks.result_asset_ids[*]` as `WorkshopAssetId`;
- `conversations.extra.remote_agent_id` as `RemoteAgentId`;
- `conversations.extra.companion_id` as `CompanionId` and
  `public_agent_id` as `PublicAgentId` under their declared session modes;
- `conversations.extra /mcp_server_ids/*` as `McpServerId`, `/agent_id` as the
  discriminator-resolved `AgentReference`, and `preset_id` through its explicit
  entity/builtin/extension union; `acp_sessions.agent_id` must equal that
  resolved conversation reference;
- `conversations.extra /session_mcp_servers/*/id` as `McpServerId`; this is a
  frozen session snapshot and therefore requires canonical type/within-array
  uniqueness but not a still-live catalog row, unlike the live
  `mcp_server_ids` list;
- `conversations.model /provider_id` as `ProviderId`, plus the tagged
  `conversations.execution_model_pool` arms: `mode=single /model/provider_id`
  and `mode=range /models/*/provider_id` as `ProviderId`, with no such pointer
  in the `automatic` arm;
- `conversations.extra /model_failover/queue/*/provider_id` as `ProviderId`;
- the session IDMM bypass slots
  `conversations.extra /idmm/fault_watch/bypass_model/provider_id`,
  `conversations.extra /idmm/decision_watch/bypass_model/provider_id`,
  `terminal_sessions.idmm /fault_watch/bypass_model/provider_id`, and
  `terminal_sessions.idmm /decision_watch/bypass_model/provider_id` as nullable
  `ProviderId` values;
- `cron_jobs.agent_config.agent_id` and every persisted preset snapshot
  `resolved_agent_id` as `AgentReference`, selected by the job/preset agent
  type and source;
- `messages.content`, discriminated by `messages.type`: text-message
  `/knowledge_writeback/written/*/kb_id` and
  `/knowledge_writeback/failures/*/kb_id` are nullable `KnowledgeBaseId`
  values, while tool-call and ACP-tool-call `/turn_id` is a required
  `MessageId` that resolves to the owning turn; provider `call_id`/tool IDs,
  `attempt_id`, raw args/input/output, error text, and user-authored message text
  are explicitly external-operation or user-content arms and remain opaque;
- `knowledge_bases.extra /source/credentialRef` as a nullable
  `ConnectorCredentialId`, with presence/resolution selected by the source-kind
  discriminator; connector-specific `scope` remains owned opaque payload;
- each `conversation_artifacts.payload.cron_job_id` as `CronJobId`, selected by
  artifact kind and required to equal the row's typed `cron_job_id` column;
- key-discriminated `client_preferences` values: `idmm_backup_provider_id`;
  provider references under `agent.model_failover $.queue[*]`,
  `nomi.collaborationModels $[*]`, `nomi.defaultModel $.id`,
  `knowledge.autogenModel $.provider_id`, `tools.imageGenerationModel $.id`,
  `tools.speechToText $.provider_id`,
  and `channels.<platform>.defaultModel $.id`; typed
  `channels.<platform>.companion_id`; exact `channels.<platform>.agent` objects
  whose `id` is `AgentReference` and agrees with `agent_type`/`backend` (the ID
  may be absent only when those fields uniquely resolve a builtin row);
  `mcp.config[*].id` under its explicit catalog-entity/builtin-natural-key
  union; plus `guid.lastSelectedAgent` under the explicit
  `AgentId | RemoteAgentId | builtin/backend/preset selection natural key`
  union;
- the checked-in generated inventory for every remaining declared ID field in
  Workshop documents/origins, agent-execution event/context JSON, cron
  configuration, preset snapshots, and other domain-owned JSON columns; this
  generated list is reviewed as part of the schema manifest and cannot contain
  an unexpanded "other" wildcard at runtime.

The conversation model, execution-pool, failover, and IDMM provider slots above
are live catalog references and must resolve to a provider row; their enclosing
kind/mode also determines whether the field may be absent. The session-MCP
snapshot exception and every other non-resolving frozen reference are named
individually rather than inferred from being JSON.

Each JSON column must be classified explicitly as domain-typed, external
protocol data, or user text. Only the latter two may be opaque, and their
classification names the owning protocol; a column cannot escape coverage
merely because it stores `TEXT`. A source-policy test inventories schema JSON
columns and fails when a new domain-owned column/pointer lacks a registry entry.
The same registry drives startup value proof, backup/restore proof, typed domain
writes, read mappers, import validation, and generated SQLite `CHECK`/JSON
constraints wherever SQLite can express the rule.

The generic client-preference API resolves every incoming key through that
key/pattern registry, validates the complete final typed value set, and applies
upserts and deletions in one transaction. A mixed batch cannot partially commit
because one known ID-bearing preference is invalid. Unknown non-domain
preference keys remain opaque settings and are never recursively searched for
ID-looking user strings; adding a domain reader for one requires registering
its exact contract first.

The main schema proof is equally exact: the migration allowlist is paired with
an allowed `sqlite_schema` object manifest/normalized SQL hashes plus
`table_xinfo`, foreign-key, unique-index, and declared `CHECK` structure. An
unknown table, index, trigger, view, virtual-table shadow, or altered constraint
beside an otherwise recognized current lineage is a concrete current-contract
violation; it cannot hide a writer or durable legacy data behind a valid
migration checksum.

That structural proof is also a mandatory pre-write safety gate. Immediately
after recognizing a current main lineage—and before operation replay,
companion/public detach, Workshop/public-agent epochs, contract stamping, or
any other bootstrap SQL mutation—a private probe must match one member of a
finite checked-in `main_schema_write_gate_v3` manifest. Its only alternatives
are the exact ID-contract-v2 baseline without the new v3 metadata, or that same
baseline with the complete exact `id_contract_installation` table and guards;
the declared Workshop/public/operation contract sentinels may be at their
explicit old or current data values, but they do not permit an alternative
table, index, view, trigger, virtual shadow, FK, `CHECK`, or partial v3 object.
Legacy row values are deliberately outside this first structural gate so the
scoped no-compatibility epochs can clear their own recognized data before the
final value/reference scan.

The locked repeat is implemented only by a new dedicated
`open_main_schema_gate_connection`, never by the existing ordinary
`init_database`/`try_init_file` path or a pool. It opens the already-existing
canonical file read-write with create disabled and no-follow flags, then—before
any schema-dependent prepare—enables SQLite defensive mode, disables trusted
schema, and installs an authorizer. It never runs migrations,
`ensure_installation_owner`, defaults/builtin materialization, journal-mode
changes/checkpoints, `ATTACH`, extension loading, or application UDF
registration. Before the gate passes, the authorizer permits only the exact
connection-local safety settings (`foreign_keys`, `synchronous = FULL`, bounded
busy timeout), `BEGIN IMMEDIATE`/`ROLLBACK`, reads of `sqlite_schema` plus the
exact migration/contract-sentinel tables, and the allowlisted read-only
`table_xinfo`/FK/index/check introspection. It denies every DML, DDL, writable
pragma, trigger action, and access to an unregistered object.

Every bootstrap transaction that will write main uses that connection and
repeats the structural gate after `BEGIN IMMEDIATE` and before its first
DML/DDL statement. Only after the locked proof passes may it replace the
authorizer with that coordinator's generated exact statement/table/known-
trigger allowlist and execute the planned write in the same transaction. If the
probe gate or locked repeat sees an unknown/altered object, it executes no
trigger-capable statement, explicitly rolls back, closes the handle, and
re-fingerprints the closed family through a private probe before classifying
`current_contract_violation` and transferring to the closed-family main hard
cut. Thus a replacement inserted between preflight and lock is harmless.

Opening/acquiring the gate may legitimately recover a hot journal or create a
WAL SHM/lock sidecar; those are physical SQLite family transitions, not
application DML/DDL. On failure they are closed and their new exact
bitmap/identities/fingerprints become the source evidence for the hard-cut
plan. The family transition is the first authorized logical main mutation; no
subordinate operation is replayed and no scoped epoch runs against untrusted
SQL. A source-policy test rejects a bootstrap main writer that calls an
ordinary initializer/pool, prepares SQL before installing the authorizer, or
does not use this guarded transaction.

Conversation create, public patch, and internal `update_extra` validate the
entire final merged object before starting a write. Declared ID fields are
typed; unknown presentation/user fields remain allowed. The old nested
`cron_job_id`/`cronJobId` input is promoted through `CronJobId`, removed from
persisted `extra`, and exposed only from the first-class column/derived response.
Those keys are forbidden in the persisted version-3 `extra` shape: finding
either during the offline proof is a current-contract violation and triggers
the dataset hard cut, rather than stamping version 3 over an obsolete duplicate
field. The same exact-shape rule applies to every retired alias in a registered
domain JSON object.
The retired phase-1 IDMM object (`enabled`/`tier`/`sidecar` and related flat
fields) is not default-deserialized into the current two-watch shape in either
conversation or terminal storage. Unknown/retired IDMM fields make the exact
version-3 object structurally invalid and trigger the dataset hard cut; no old
configuration is silently converted into a disabled current object.
The obsolete `custom_agent_id` alias is likewise forbidden in persisted
version-3 conversation extra, cron agent config, and channel preference
objects. New backend and frontend writers use the single `agent_id`/`id`
`AgentReference` slot, and new API input containing the alias is rejected
rather than promoted. Finding the alias during the version-3 proof triggers the
dataset hard cut; no arbitrary historical string is reclassified as a catalog
key.
Artifact payloads are decoded by kind at insert, update, offline scan, and read.
A message-content writer likewise decodes the final type-specific object and
validates these typed pointers before insert/update; it never treats the whole
column as opaque because some nested fields contain provider payloads.
A poisoned row returns one controlled invariant error for the operation; list
paths do not warn-and-skip it and no serializer/WebSocket receives raw data.
Present-but-invalid domain JSON never becomes semantic absence: the knowledge
`source_from_extra`, conversation failover override, execution-pool/model, and
terminal/conversation IDMM readers return a checked `Result` for stored data
instead of `.ok()`/defaulting past an invalid typed pointer. Defaults remain
legal only when the registered optional key or whole object is genuinely
absent.

The exact version-3 main manifest adds one singleton
`id_contract_installation` row: `key = 'installation'`, canonical
`owner_user_id`, `reference_contract_version = 3`, and nullable
`installed_by_plan_hash`. Its owner must exactly equal
`installation_identity.owner_user_id`; foreign-key/unique constraints and
update/delete guards make the binding immutable. A non-null plan hash is the
canonical lowercase SHA-256 of the reset plan and is legal only for a dataset
created by that plan. On an existing current ID-contract-v2 database with the
row missing or its contract value lower, bootstrap runs the complete exact-type
column/JSON scan under the exclusive data-dir guard. If every value passes, one
`synchronous=FULL` transaction creates/stamps this metadata with the existing
owner and a null plan hash, without rewriting any pre-existing row. If any
declared value is legacy, malformed, wrong-prefix, wrong-union-arm, dangling
where required, or structurally invalid, the database enters the dataset hard
cut below; no row or JSON value is converted or selectively repaired. A future
version fails without writing. Fresh ordinary databases are born at version 3
with a newly minted owner and null plan hash; reset-created databases use the
owner and non-null plan hash pre-minted by their immutable plan. A current
version-3 value is an expected contract number, not proof of health: every
bootstrap first passes the schema-only write gate, then reruns the complete
schema/value/reference scan through a source-preserving private probe after
operation/scoped-epoch arbitration. A concrete violation on that recognized
current lineage takes the same `current_contract_violation` hard-cut path; a
clean scan performs no write.

When `nomifun-db` detects the retired main-database migration lineage, creating
a new main database is a dataset-level destructive reset, not only a database
file replacement. Detection returns a structured `RetiredLineage` outcome
without renaming files. Application bootstrap, which still owns the exclusive
data-directory lock and has not constructed a live pool, runs the destructive
coordinator.

Detection is a source-preserving preflight through the private closed-family
probe protocol above, before any writable SQLite connection or WAL pragma can
touch the canonical family. Apart from its self-contained, non-authorizing
probe root—which is removed before arming—it changes no main DB member or side
store. It classifies only the retired lineage whose applied `(version,
checksum)` tuples match an embedded allowlist extracted from the retired
migrations and whose schema has the expected numeric-key sentinel columns.
Both predicates are required. The only second destructive classification is a
fully recognized current ID-contract-v2 migration lineage whose exact schema or
version-3 reference scan produced a concrete current-contract violation. A future
migration/reference version, an unknown checksum, a partially matching schema,
corruption, or any other migration error fails without arming, quarantining, or
changing files; the current broad mapping from every
`VersionMismatch`/`VersionMissing`/`VersionTooOld`/`VersionTooNew` error is
removed. The marker plan records whether the trigger was `retired_lineage` or
`current_contract_violation` and its proof hash, so recovery never widens the
classification.

The coordinator owns the durable marker directory
`<data_dir>/.id-reference-v3-dataset-reset.pending/`. Before mutation it writes
and syncs an immutable plan containing the exact unused backup and target
staging basenames; the initial existence bitmap, no-follow identities, sizes,
and content fingerprints for the exact DB/`-wal`/`-shm`/`-journal` family; the
source installation
owner when readable; the target contract; one newly minted canonical UUIDv7
target `UserId` for `installation_identity.owner_user_id`; the canonical plan
hash—lowercase SHA-256 over the canonical plan body with its hash slot omitted—
that must be installed in `id_contract_installation`; deduplicated
conversation-workspace sources and orphan destinations; and one newly minted
canonical UUIDv7 storage generation. It then creates `armed` and syncs the data
directory. Later immutable phase files are `main_family_transition_started`,
`main_quarantined`,
`main_committed`, `user_files_salvaged`, `companion_detached`, `side_clean`,
`generation_installed`, and `backup_discarded`; they use the same temp-write,
file sync, rename, and directory-sync protocol as the companion marker. The
plan's generation and destinations are never minted again during retries. Each
existing phase is treated as a hint and its full filesystem/database predicate
is revalidated; all changed source/destination parents, including an external
work-dir parent, are durably flushed before the phase is recorded.

Before arming, the plan also inventories and hashes every existing companion
reset/import/runtime-operation journal, companion operation-commit/projection-
outbox row, Workshop, public-agent, preview, and storage-generation marker,
including its legal phase set. An unknown, malformed, hash-mismatched, or
impossible subordinate state blocks the main reset before mutation. Once
`main_committed` holds, the broader main plan explicitly supersedes those frozen
operations: a pending companion hard cut finishes/re-proves its store, side,
bridge, and skill cleanup while skipping detach from the now-replaced old main,
then runs `dataset_detach` against the clean main and atomically consumes the old
marker. Each pending runtime operation applies its explicit new-main
compensation rather than replaying an old-main mutation: a requested companion
delete finishes its side/profile removal, a thread/channel create drops its
planned side registration while preserving workspaces, and skill/bridge/memory
operations are consumed by the broader dataset detach and projection cleanup.
Their commit/outbox rows and journals are then removed under the main plan.
Pending imports are discarded with the reset DB and journals; Workshop,
public-agent, and preview stores are cleared, their empty current sentinels
installed, and their markers consumed. This is plan-hash-authorized
supersession, not ordinary startup recovery. No old marker is asked to accept a
new main identity/generation, and no subordinate marker is merely deleted.

A marker directory without `armed` authorizes no mutation. If it contains only
a complete plan and temporary files, bootstrap removes it durably and repeats
the trigger-specific source-preserving private-probe classification; if it also
contains a planned backup or any later
phase, state is ambiguous and startup fails without touching the main database
or side stores.

The state machine is:

1. With no marker, either exact destructive classification only arms the durable
   plan and returns control to the coordinator. No database or side store has
   changed.
2. With `armed` but no `main_family_transition_started`, use a fresh private
   probe to re-prove exactly the trigger frozen in the plan: either the retired
   checksum/sentinel allowlist, or the same recognized current-v2 lineage plus
   the same version-3 violation proof hash. The complete original family
   bitmap, no-follow identities/fingerprints, and readable source installation
   owner must still equal the plan, and every planned backup member must still
   be absent. A mismatch fails closed with no rename. Only after that full
   pre-mutation proof does bootstrap durably record
   `main_family_transition_started`.

   Once that phase exists, recovery no longer tries to derive the old trigger
   or bitmap from a partially moved canonical family. Instead it validates the
   immutable plan/phase hash and the per-member source/destination transition
   lattice. For each initially present `-shm`, `-wal`, proven rollback
   `-journal`, and finally main DB, source-original/destination-absent is the
   sole pending state and source-absent/destination-exact-original is the sole
   completed state. Both present, both absent, a changed identity/fingerprint,
   or an unexpected destination fails closed. Every initially absent member
   must remain absent at both names. Move each pending sidecar first and the
   main file last to its exact planned backup-family name, flushing both parents
   after every rename. Only after all initially present members are exact at
   their destinations and every canonical family member is absent is
   `main_quarantined` recorded.
3. With the complete planned backup present, the canonical main path has only
   two accepted states. If the entire canonical DB/`-wal`/`-shm`/`-journal`
   family is absent,
   create the ID-contract-v2 baseline plus reference contract version 3 at the
   plan's unique sibling staging basename, using exactly the pre-minted target
   owner and plan hash in the same `synchronous=FULL` transaction. Close,
   checkpoint as required, sync, and fully prove the staging family, then
   prove the staging target is one standalone main file and
   `atomic_install_absent` it at the still-absent canonical path. A partial
   staging artifact may be removed/rebuilt only after its exact plan-private
   basename, no-follow identity, and lack of publication are proven; it never
   authorizes removal of a canonical file. If any canonical family member is
   present, accept it only when a closed-family proof shows a complete current
   database whose `installation_identity.owner_user_id` equals the plan's exact
   target owner and whose `id_contract_installation.installed_by_plan_hash`
   equals this plan hash, in addition to passing migration history, the full
   fixed/JSON registry and empty-baseline predicates, foreign keys, and
   `PRAGMA quick_check`. That is the sole crash-after-install retry state. A
   canonical file with a different identity/hash, a partial/unreadable
   canonical family, or any other current-looking replacement is unrelated:
   retain the marker and planned backup, perform zero deletion/write against
   that canonical family, and fail startup. Only after the exact target is
   closed, synced, and re-proven is `main_committed` recorded.
4. Before `main_committed`, no side store or storage-generation file is
   deleted. A missing planned backup, an unexplained combination of source and
   backup files, or a canonical DB that lacks the plan's exact target
   owner/provenance is ambiguous and fails startup without changing that DB or
   performing side cleanup.
5. After `main_committed`, salvage each initially present managed conversation
   workspace root (`<data_dir>/conversations/` and the distinct relocated
   `<work_dir>/conversations/`, if any) by moving the whole real non-reparse
   directory to its exact planned sibling under
   `orphaned-conversations/id-reference-v3-<generation>/`. The move never walks
   links inside the tree. Source absent/destination present is a completed
   retry; both present or an ancestor/root symlink/reparse point fails without
   deleting either. Sync both parents, verify every planned source/destination,
   and record `user_files_salvaged`.
6. Invoke companion `dataset_detach` with the main plan hash. Wait until its
   own marker clears, then prove its v7 tables are empty, main-dataset profile
   references are absent, and canonical profiles, provable figures, all
   companion workspaces, and model assets remain. Record `companion_detached`.
7. Remove the remaining ID-coupled metadata/derived set:
   `attachments/`, `knowledge/`, `cron/`, `workshop/`, `preview-history/`,
   `nomi-sessions/`, `nomi-health-check-sessions/`, `browser-profile/`,
   `browser-profiles/`, and `public-agents/`. The already-salvaged conversation
   roots must be absent; `companion/` is governed by the stricter detach
   predicate rather than removed wholesale. Every target is no-follow, every
   changed parent is flushed, and every failure is fatal. Record `side_clean`
   only after all absence/detach predicates are reverified.
8. Keep the main marker and its planned generation pending while the top-level
   coordinator reopens/proves the final main bootstrap DB, resolves any
   standalone companion reset/main detach and import recovery, and installs empty/current
   Workshop/public-agent/preview contracts. Only then atomically replace
   `storage-generation` with the exact planned value, sync/validate it, record
   `generation_installed`, and revalidate every final predicate. The old main
   family is recovery-only residue, never a retained compatibility backup:
   remove its planned SHM/WAL/rollback-journal members first and main file last,
   flush the parent after each unlink, prove the complete backup family absent, and record
   `backup_discarded`. A retry after `generation_installed` accepts each member
   as present-and-removable or already absent; before `main_committed`, the
   complete planned backup remains mandatory. Revalidate all predicates once
   more and durably remove the main marker. Builtins and live services remain
   unavailable throughout.

Every crash position maps to one of those observable states. In particular,
the planned backup distinguishes a crash after rename from an unrelated missing
database until the forward reset has committed, and `main_committed` is the sole
authorization for destructive side-store cleanup. The successful terminal
predicate contains neither the retired source family nor its recovery copy. A
clean main database may exist during recovery, but no runtime consumer can
combine it with retired side stores.

Storage-generation initialization therefore moves out of environment setup to
the final lifecycle step, before `AppServices::from_config`. A main reset plan
owns its unpublished generation. On a normal first boot with no main reset, a
small `.storage-generation.pending/` plan owns one newly minted canonical
UUIDv7 until the same final step installs it; an existing canonical generation
is the plan value. Companion plans bind to this lifecycle value and the final
main installation identity, never to whether the public file happens to exist.
Bootstrap loads the installed value once and only then publishes
`NOMIFUN_STORAGE_GENERATION`; any lifecycle marker prevents publication.

This broader deletion applies only when the authoritative main database itself
hard-cuts from either exact destructive trigger. It discards historical ID-coupled metadata,
but its companion detach and conversation-root salvage preserve canonical
profiles, provable figures, every regular workspace, and model assets without
reattaching them to removed main entities.

The explicitly user-authorized full factory reset remains a different,
destructive product operation: it may delete managed workspaces and the whole
companion tree. It is the highest-priority bootstrap lifecycle mode. After the
server lock, a factory marker is promoted to an immutable durable plan before
any lower-priority main/companion/Workshop/import/auxiliary marker is resumed.
The authenticated reset route creates a strict versioned request while the
current `AppConfig` is live: canonical operation UUIDv7, exact canonical data
root, exact already-resolved work root, scope, timestamp, and payload hash. It
writes/syncs a sibling temp, atomically installs the previously absent request,
and flushes the data directory. A truncated/malformed/unknown-version/hash- or
root-mismatched request fails closed; it is never deserialized as a default
`Full` authorization. The promoted plan records that frozen work directory and
the exact validated
backup/journal artifacts owned by every superseded operation. Factory reset
then consumes those subordinate plans and their planned backup families under
their normal path-containment rules; it never tries to resume them against the
fresh dataset.

Before deleting companion config, the factory plan also freezes and executes
the same three-state external memory-bridge ownership inventory as companion
hard cut. It removes only proven mirror files and rebuilds the external index;
a reparse/locked/changed proven root keeps factory reset pending, while an
unreachable historical path is detached with the explicitly scoped warning
above. Runtime `.ops-v7` journals/commit/outbox rows are part of the subordinate
inventory and are consumed rather than replayed into the fresh dataset.

Its inventory includes `public-agents/`, preview history, all companion-evolved
skill roots, every lifecycle marker/contract sentinel, and both data-dir and
distinct external work-dir managed conversations. It shares the hardened
no-follow/removal/durability helpers, not the automatic hard-cut retention set.
Every DB-family member, derived-directory removal, parent flush,
storage-generation removal, and marker transition is fatal-on-failure; the
factory plan remains pending instead of logging best-effort success. Only after
`dir-config.json` is durably removed does bootstrap resolve `work_dir` again
(CLI override if explicit, otherwise the fresh default), update the environment
and `AppConfig`, and proceed. This same-boot resolver takes explicit CLI/config
inputs and cannot inherit the pre-reset exported `NOMIFUN_WORK_DIR`. A completed full reset cannot leave a stale
dataset-reset marker/backup, public-agent config/audit, token, skill body, or
provider/knowledge/conversation reference.

One bootstrap lifecycle coordinator fixes the order for every platform after
the data root is known: acquire the exclusive lock; finish/discard any valid
non-authorizing probe intent through its private cleanup state machine; strictly
arbitrate/finish factory reset; finalize work-dir/config/environment; perform a
source-preserving, private-probe joint
classification of the main lineage, companion store, and every
`.ops-v7`/commit/outbox state. For every recognized current main, run
`main_schema_write_gate_v3` before any branch is allowed a writable main
transaction. A structural violation freezes all legal subordinate state into a
`current_contract_violation` main plan and runs the closed-family main
coordinator immediately; operation replay, companion main cleanup, and scoped
epochs are skipped against the old DB.

Only after that gate passes may bootstrap replay an operation before the final
v3 value scan, and only when the companion DB is also proven current and the
journal/commit/outbox closure binds to the exact installation and generation.
If the companion side instead re-proves an authorized pre-v7,
missing-with-residue, confirmed-corrupt, schema-invalid, or value-invalid hard
cut, freeze and supersede those operations in the companion reset plan, clean
their typed main/bridge footprint under the locked repeat of the structural
gate, and finish that reset before the value scan; never demand a commit-row
read from the bad DB. Any other ambiguous companion or operation state fails
closed.

For a still-current gated main, next complete the Workshop-v2 and
public-agent-v2 coordinators, each repeating the gate inside its write
transaction, because their scoped epochs intentionally mutate main rows. Only
then run the final main-v3 exact schema/value/reference scan; otherwise known
pre-epoch Workshop/public values could be misclassified as an unrelated dataset
violation and cause an unnecessarily broad hard cut. If the initial lineage,
the pre-write schema gate, or this later full scan requires a dataset reset,
freeze all remaining legal subordinate states into the main plan and never
replay them on the new DB. Run the main coordinator (including its authorized
companion detach), then
prove/open the final main bootstrap DB; with no old operation left, finish any
still-applicable standalone companion creation/reset and recover companion
imports. Prove or install the Workshop/public sentinels (a fresh main satisfies
their empty DB portions), run the preview coordinator, install/publish the
planned storage generation, and materialize builtins; only then create live
pools, registries, services, workers, and routes. Earlier stages use only
bootstrap connections/guards and close them before the next destructive
filesystem phase. No feature service can observe a mixture of pre- and
post-contract stores.

### 7. Workshop ID envelope

Workshop receives the same no-compatibility epoch rather than attempting to
repair frontend-owned JSON. A singleton main-DB contract value is born at
version 2. Before Workshop/creation services start, a bootstrap coordinator
under the data-dir guard handles any missing/lower version with the durable
top-level marker `.workshop-id-contract-v2-reset.pending/`: `armed`,
`db_committed`, `files_clean`. One `synchronous=FULL` transaction deletes
`creation_tasks`, `workshop_assets`, and `workshop_canvases`, writes contract
version 2 last, and commits. Only after the empty-table predicate is proven does
the coordinator remove and parent-sync `workshop/`; services remain unavailable
until both predicates hold and the marker clears. A future contract version
fails without writing. Fresh main databases are born directly at Workshop
contract version 2, and a main-dataset hard cut satisfies this epoch through
its broader reset.

Workshop keeps frontend-owned document semantics, while the backend validates
every durable ID slot it persists or imports:

- canvas `wsc` and asset `wsa` rows are minted with their registered typed IDs;
- nodes, groups, mentions, edges, and edge endpoints retain the existing
  `wsn`/`wse` validation;
- image/video `assetId` fields are typed `wsa` IDs;
- generator `resultAssetIds`, `maskAssetId`, batch primary asset, `providerId`,
  and `taskId` are validated with their registered types;
- asset origin `provider_id`, `canvas_id`, `node_id`, and `task_id` are typed at
  write, read, and import boundaries.

The backend validates declared fields according to node kind. It does not scan
arbitrary user-authored text for prefix-looking substrings. A malformed stored
document or asset/origin row returns a controlled invariant error and is never
substituted with an empty default or serialized to the frontend. Because the
one-time epoch removed all pre-contract data, such an error means post-epoch
tampering or a writer bug.

Workshop archive version 2 is exact. Import validates the manifest app/version,
the entire canvas identity envelope, all asset entries, every origin, and every
referenced file before registering an asset or creating a canvas. Clone import
then mints new typed IDs and rewrites references through complete explicit
maps. Version 1 and partially typed archives are rejected with zero writes.

### 8. Auxiliary side-store epochs

Current main-database users may never trigger a main hard cut, so side stores
with historically opaque IDs receive their own no-compatibility epochs under
the same bootstrap guard.

Each auxiliary coordinator uses an immutable hashed plan followed by `armed`,
`store_clean`, and `contract_installed` phases. It applies the same temporary
write/sync/rename/parent-flush protocol, no-armed rule, no-follow/reparse-point
rules, future-version rejection, and full-predicate revalidation as the main
coordinators. The current sentinel is written only after the empty/clean store
is durable; a phase is a retry hint, never proof by itself.

`public-agents/` moves to contract version 2, with its sentinel outside the
deletable store and a durable `.public-agent-id-contract-v2-reset.pending/`
marker. A missing/lower contract clears the complete public-agent roster,
sequence watermark, and audit corpus before writing version 2; no config or
audit line is migrated. A future version fails without writing. On every later
boot, directory/config identity is a `PublicAgentId`, provider and knowledge
references use their exact types and resolve in the proven main catalog,
sequence state is bounded, and each JSONL
audit entry carries a `PublicAgentAuditEntryId`. Invalid config directories are
removed under a durable `side_cleanup`; one invalid audit line clears that
agent's entire audit corpus. Scans and searches no longer silently skip corrupt
configs or lines. Writes are atomic/durable, removals are fatal while pending,
and valid version-2 agents remain untouched.

The public-agent epoch, invalid-config `side_cleanup`, and normal DELETE are
cross-store durable operations, not side-directory removals. Their immutable
plan freezes the target public IDs,
`conversations.extra.public_agent_id` rows, plugin bindings, and channel
sessions. One main transaction explicitly deletes those sessions before their
public-service conversations, clears `channel_plugins.public_agent_id`, and
proves no main structured reference remains; only then is the config/audit
directory removed and its parent flushed. Normal DELETE removes the in-memory
roster entry last and returns success only after the journal clears. Any
failure remains resumable and the target stays unavailable; empty-roster
reconcile shortcuts and warn-only filesystem removal are not cleanup
authorities. The epoch applies this to the whole pre-v2 roster before installing
its empty sentinel. Public-agent conversation creation, channel/plugin binding,
and every other public-agent-reference writer hold the analogous shared
lifecycle permit and check the durable pending-public-owner tombstone. Epoch,
invalid-config cleanup, and DELETE hold the exclusive permit from preflight
through final main-reference proof, side removal, in-memory roster removal, and
journal clear, so no writer can recreate a binding from the frozen inventory.

Preview history is a durable referenced entity store rather than an opaque
timestamp cache. `PreviewSnapshotId` is added to the canonical registry with
the pinned prefix `psnap`; the Rust DTO and TypeScript boundary use that type.
`preview-history/` moves to contract version 2 with a sibling durable reset
marker. A missing/lower contract deletes all old timestamp/random snapshot
indexes and content files, then writes version 2; it never renames or rewrites
an old snapshot. New saves mint `psnap_<canonical-uuidv7>`, sync the content
before atomically installing a synced index, and derive paths only from a
validated newtype. Every read proves an exact index/file bijection, typed ID,
regular no-follow file, size, target metadata, and optional typed
`conversation_id`; an invalid target directory is cleared as one private unit
under `side_cleanup`, never returned as an empty successful history. The
optional conversation ID must also resolve in the proven final main dataset;
a dangling target clears that private unit through the same durable plan. The
40-hex target hash remains a content-addressed directory key, not an entity ID.

Both epoch sentinels/markers participate in offline backup proof and factory
reset arbitration. Main-dataset hard cut may satisfy them by deleting the whole
stores, but it still installs current empty contract sentinels before services
start.

### 9. Embedded execution receipts

The embedded `nomi_delegate` receipt intentionally exposes the same
`execution_id` field and `exec` namespace as a platform execution. Its current
`Uuid::now_v7().simple()` body is non-canonical. It changes to the normal
hyphenated `Uuid::now_v7()` representation.

The agent-layer dependency graph remains unchanged. A focused test validates
the entire returned string, UUID version, variant, lowercase hyphenated form,
and prefix. Test fixtures that use values such as `exec_test` are replaced with
canonical fixtures when they exercise the public receipt contract.

Provider-issued `call_` and `toolu_` IDs remain opaque external protocol IDs
and are unchanged.

### 10. Frontend behavior

`ui/src/common/types/ids.ts` and `ipcBridge.ts` remain strict and add the
registered `PreviewSnapshotId`. There is no legacy parser, fallback brand cast,
catch-and-drop list item, or error-message suppression. Nested
`conversation.extra`, artifact payload, Workshop, public-agent, and preview
adapters parse the same exact type declared by the backend reference registry.

Frontend tests continue to prove that a legacy `last_learn.id`, remote-agent
reference, artifact cron reference, or preview snapshot ID throws. Backend and
end-to-end fixtures prove that production APIs emit only canonical IDs, so the
strict frontend tests are detectors while backend tests establish the
user-visible repair.

## Error handling and observability

The companion store no longer converts every file-open/migration error into a
temporary in-memory store.

- SQLite busy/locked and Windows sharing/lock violations receive bounded
  startup retries with backoff.
- "Confirmed corrupt" means the SQLite primary result code is
  `SQLITE_CORRUPT` (11), including an extended corrupt code, or
  `SQLITE_NOTADB` (26), or an opened database returns anything other than the
  single value `ok` from `PRAGMA quick_check`. `BUSY`, `LOCKED`, `CANTOPEN`,
  `IOERR`, `PERM`, `READONLY`, disk-full, and ordinary filesystem errors are
  never classified as corruption.
- A confirmed corrupt companion DB, under the exclusive application data-dir
  lock and before any live pool exists, first freezes/arms the companion plan,
  then follows its member-by-member sidecar-first family-removal and staged
  no-replace v7 installation phases. No row recovery is attempted, and the
  main/side/bridge/skill predicates complete before the marker is cleared.
- Permission errors, an unsupported future version, or an unremovable stale
  side store fail companion initialization; if a pending marker already
  exists, it remains. They are not presented as a successful empty in-memory
  companion.
- A malformed row discovered after startup returns an internal-invariant error
  with table and field context, but never includes arbitrary user content.
- Import validation errors are client errors and identify the archive entry and
  field without coercing the value.

One structured boot log records reset source version, target version, phases,
and counts of removed records/files. It never logs memory content, prompts,
audit details, or other user data.

## Cross-platform behavior

All three desktop packages call the same backend service and store code. The
default roots differ only by OS:

- Windows: `%LOCALAPPDATA%\NomiFun\Nomi`;
- macOS: `~/Library/Application Support/NomiFun/Nomi`;
- Linux: `$XDG_DATA_HOME/NomiFun/Nomi` with the normal platform fallback when
  that variable is unset.

The reset runs while the application holds its existing exclusive data-dir
server lock and before companion files have live handles. Every destructive
main or companion epoch uses the same closed-family transition plus staged,
standalone target installation on every OS; SQLite transactions build the
private target, never rewrite the historical canonical source. A non-destructive
side cleanup leaves its proven current DB byte-identical.
Windows error codes 5, 32, and 33 receive the same bounded startup retry policy
used elsewhere in the repository. Unix/macOS paths use the identical state
machine without a platform-specific data contract. The filesystem helper uses
no-follow metadata on every platform, treats Windows reparse points as links,
and exposes two non-interchangeable primitives:

- `atomic_replace` installs a synced sibling over an existing mutable
  config/profile/index/sequence/sentinel. Windows uses
  `MoveFileExW(MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH)`; Linux uses
  same-directory rename followed by parent `fsync`; macOS uses same-directory
  rename followed by parent `File::sync_all()`/`F_FULLFSYNC`.
- `atomic_install_absent` publishes a synced marker phase, import destination,
  staged fresh DB, or completed journal only when the target is absent. It uses
  a platform no-replace rename/move primitive (Windows write-through without
  `REPLACE_EXISTING`; Linux `renameat2(RENAME_NOREPLACE)`; macOS
  `renamex_np(RENAME_EXCL)`), never a racy exists-then-rename sequence. If the
  required primitive is unavailable on a supported runtime, bootstrap fails
  before mutation.

Both call the `durable_entry_change` mapping for the changed parents; moves
cover both sides. Windows directory handles opened with backup semantics are
used for identity, reparse-point, and sharing checks, not passed to an invented
directory-flush API. Its documented durability edge is the write-through move
plus the recoverable tombstone/phase protocol above, based on
[Microsoft's `MoveFileExW` contract](https://learn.microsoft.com/en-us/windows/win32/api/winbase/nf-winbase-movefileexw).
Linux syncs files and parent directories with its required `fsync` sequence.
macOS uses `File::sync_all()`/`F_FULLFSYNC` for both temporary files and changed
parent directories, never plain `fsync`, and first verifies that the volume
supports [`RENAME_EXCL`](https://developer.apple.com/documentation/foundation/urlresourcevalues/volumesupportsexclusiverenaming).
A missing platform durability or atomicity primitive is an initialization
error, not permission to clear a phase early.

## Test strategy

Implementation follows red-green-refactor. The original symptom is captured by
a failing regression before production changes.

### Canonical registry and generation

- Iterate the generated entity registry and prove unique canonical prefixes,
  typed mint/parse round trips, lowercase hyphenated UUIDv7 bodies, RFC 9562
  version-7 bits, IETF variant bits, and a 48-bit Unix-millisecond timestamp.
- Iterate the main reference registry and prove that every fixed column maps to
  one exact parser/explicit union and every domain-owned JSON column maps to an
  exact discriminator/pointer/cardinality contract. Fail schema review for an
  unregistered new durable ID slot or domain JSON column.
- For every registered main slot, reject a canonical UUIDv7 under an unknown
  prefix and under every wrong registered prefix; prove a self-consistent
  wrong-domain FK graph still fails the value scan.
- Exercise every registered `client_preferences` exact/key-pattern contract,
  including all provider/model keys, channel companion, MCP config, speech, and
  GUID agent-selection unions. For every channel platform, exercise canonical
  custom `AgentId`, registered builtin and extension keys, unique builtin
  resolution without an ID, wrong source/type/backend, arbitrary natural keys,
  and forbidden `custom_agent_id`. A mixed upsert/delete batch with one invalid
  ID must make zero changes.
- Exercise the exact non-preference JSON matrix for knowledge source
  `credentialRef`, conversation model/execution-pool/failover providers, all
  four conversation/terminal IDMM bypass-provider paths, and session MCP
  snapshot IDs. Prove each registered pointer rejects a wrong-domain canonical
  UUID while connector scope, provider/tool payload, model names, and user text
  remain opaque in their declared arms.
- Reject unknown portable entity types and every type/prefix mismatch; prove
  clone IDs are minted through the resolved registered type.
- Add a source-policy test that forbids registered entity literals through the
  raw operation-key helper and forbids compact/simple UUID formatting in a
  NomiFun entity receipt. The only compact-prefix exceptions are named external
  provider protocol adapters such as OpenAI `call_` and Anthropic `toolu_`.

### Companion store and API

- Run the bootstrap coordinator over a handcrafted version-6 file containing a
  legacy `plr_<16-character-short-id>`; assert that subsequent service open
  returns no learn runs and observes version 7.
- Start with the complete identity-bearing companion closure absent and assert
  staged `fresh_create` produces v7 while preserving orphan workspaces/models.
  With DB absent beside canonical profiles/figures or any main reference,
  evolved skill, import journal, legacy/shared entry, assert `hard_cut` instead:
  canonical assets survive and the cross-store closure becomes clean.
- Exercise `missing_with_residue` with no DB family, orphan WAL, WAL+SHM, and a
  bounded no-super orphan rollback journal, plus each retained-side residue
  class. Crash before `family_transition_started` and before/after every
  orphan-sidecar removal, `source_family_cleared`, staged-target sync,
  no-replace install, and `store_committed`; after transition starts, recovery
  must use the member lattice rather than demand the now-impossible original
  bitmap/proof. Every retry either keeps the marker pending or reaches the exact
  pre-minted empty v7 installation, never stalls for an openable source DB or
  lets SQLite create beside stale hot state.
- Seed every ID-bearing companion table and nested ID field with legacy values;
  assert that the store resets all tables as one unit and removes the v6
  quarantine ledger.
- Seed a populated current-v7 DB with canonical UUIDv7 owners whose profiles are
  absent, or with a thread/window whose main conversation owner markers disagree;
  assert the offline closure proof classifies `hard_cut` before arming.
- Add an unknown table, index, trigger, view, and virtual-table shadow object,
  and separately alter `table_xinfo`/FK/CHECK/index structure; assert exact
  manifest proof hard-cuts and the rebuilt `sqlite_schema` has no residue.
- Attempt raw SQL inserts with a retired short ID, UUIDv4, compact UUID,
  uppercase UUID, wrong prefix, and invalid variant for each fixed ID column;
  assert that SQLite rejects them.
- Bypass the normal writer to poison a row after v7 startup; assert that the
  mapper returns a controlled invariant error and no HTTP/WebSocket serializer
  receives it.
- After service startup, tamper independently with config, profile, figure
  index, event JSONL, sequence, and a skill projection. Assert every API/list/
  worker/export path returns recovery-required, closes visibility, and never
  defaults, skips, or emits a partial payload.
- Exercise companion list, detail, status, learn history, run-now, digest,
  suggestion, active-thread, and learn-finished WebSocket paths with canonical
  fixtures.
- Before `family_transition_started`, replace each
  pre-v7/schema/value/owner/skill-trigger source with a different file or a
  populated valid v7 installation. Assert the private-probe trigger/family/hash
  reproof records no transition phase, performs zero rename/unlink, and never
  erases the replacement. Then crash before/after that phase and after every
  rollback-journal/SHM/WAL/main removal or Windows tombstone; recovery must
  accept only each member's plan-defined original/tombstone/absent state and
  finish forward, while a changed/reappearing member fails with zero further
  deletion. Separately make a `side_cleanup` trigger disappear before its
  final probe and assert no side file changes. Crash after staged-target
  commit/install but before `store_committed`; accept it only when the empty
  target metadata carries the exact pre-minted installation ID and plan hash.
  Prove a second process cannot acquire the data-dir reset guard.
- Inject failure at marker-directory create, temp-file sync, phase rename,
  parent sync, DB commit, each shared unlink, each workspace move, each figure
  index/image/profile update, `side_clean`, and marker removal. Every pre-v7
  restart completes to one empty v7 store, and an existing phase whose data
  predicate is false causes the predicate to be repaired before advancement.
- Leave an unarmed marker directory and assert that it authorizes no deletion;
  leave a later phase without `armed` and assert fail-closed behavior.
- Run `side_cleanup` beside a populated valid v7 database for each invalid
  profile, figure, config, sequence, event, and unknown-shared-entry case.
  Assert valid rows/config/events remain byte-identical, invalid config resets,
  sequence reconstructs, and one invalid event clears the entire event corpus.
- Seed canonical provider/preset/MCP/knowledge IDs in profile/shared snapshots
  whose main rows are absent; assert optional snapshots are cleared as whole
  typed units under `side_cleanup`, while an owner-envelope violation classifies
  `hard_cut` and no replacement ID is invented.
- Arm `hard_cut` for an independent pre-v7 store trigger while retaining a
  canonical profile with a dangling optional catalog/figure binding. Assert the
  same hard-cut plan performs the typed optional-field rewrite and completes;
  it neither attempts to change mode nor waits forever. In `dataset_detach`,
  assert every main-dataset field is cleared even when its old target resolves.
- Create a valid populated v7 DB with a missing/mismatched evolved-skill body;
  assert it takes `hard_cut`, clears all companion business tables and only the
  three companion-evolution skill roots, and never touches another user skill.
- Reject `side_cleanup` when its locked DB proof no longer matches the armed
  mode, with the marker retained and zero writes.
- Reject `user_version > 7` without changing schema or files.
- Prove lock retry does not register an in-memory store as the live store.
- Prove every destructive trigger recreates the DB family only after handles
  close, exact source-family evidence is re-proven, and
  `family_transition_started` is durable; include hot rollback, orphan journal,
  and WAL-plus-journal ambiguous fixtures.
- Prove `BUSY`, `LOCKED`, `CANTOPEN`, `IOERR`, `PERM`, `READONLY`, and disk-full
  paths never enter corruption reset.

### Import, side stores, and lifecycle

- Reject companion bundle v1 before any write.
- Reject a v2 bundle with one invalid memory, learn run, owner, profile, state
  pointer, or event ID and assert all pre-existing managed rows/files are
  unchanged and no import journal remains.
- Reject duplicate/path-escaping/symlink/non-regular entries and every
  entry-count, per-entry, or expanded-byte budget violation before domain
  writes.
- Assert active near-duplicates and fully equal same-ID records are idempotent,
  while a difference in any normalized field or event-file hash conflicts.
- Export/import a canonical v2 bundle and assert every ID parses through its
  registered newtype.
- Crash after `validated`, immediately after SQL commit, during file install,
  after `files_installed`, after pending-to-completed rename, after commit-row
  deletion, and during completed-journal GC; recovery either discards a
  pre-commit plan or completes the exact committed plan forward without
  exposing a partial roster/event set or ambiguously replaying success.
- Hold a companion read, background mutation, and WebSocket publication at the
  import barrier and assert none observes state between SQL commit and journal
  finalization.
- Crash at every phase of companion delete, companion-thread create,
  companion-channel bind, unbound-channel create, and every skill
  create/edit/accept/reject/gift/archive/delete projection. Recovery must finish
  or apply only the declared compensation; assert no hidden conversation,
  ownerless row, executable rejected skill, duplicate thread, or lost workspace.
- Hold shared permits in each conversation/channel/preference/token/knowledge/
  memory/skill/bridge writer while delete requests its exclusive permit, then
  race each writer after the pending-owner tombstone is installed. Assert the
  delete inventory starts only after the first group drains, the second group
  is rejected, the exclusive permit lasts through profile/journal removal, and
  the final owner-reference scan stays empty. Repeat the equivalent race for a
  public-agent delete and public conversation/plugin binding.
- Leave a valid `.ops-v7` crash state whose main rows are temporarily
  inconsistent. With a matching current main installation, assert operation
  recovery precedes the v3 scan and preserves the dataset. With a retired main
  lineage, assert the main plan freezes and compensates/consumes the operation
  after `main_committed` without replaying it into the new DB. Invalid/missing
  journals or outbox rows fail closed.
- Repeat the pending-operation boot with a current main but a pre-v7, missing-
  with-residue, confirmed-corrupt, schema-invalid, and value-invalid companion
  store. Assert the authorized companion hard cut supersedes the operation,
  removes its typed main/bridge footprint, and consumes local coordination
  residue without attempting a commit-row read from the bad DB or following an
  untrusted external path.
- Resume each pending-marker phase and verify idempotent cleanup.
- Preserve canonical profiles, provable figures, every regular workspace, and
  the model cache while deleting invalid entries and legacy layouts.
- Seed companion threads plus main companion-session conversations, channel
  sessions/plugins/preferences, access tokens, and knowledge bindings for both
  retained and discarded profiles. After companion `hard_cut`, assert all old
  companion conversations/sessions and their non-FK audit rows are gone,
  references for discarded owners are gone, valid identity-level bindings for
  retained owners remain, and the preserved workspace was neither deleted nor
  reattached. Crash at every store/side/main/skill phase and re-prove the same
  final cross-store predicate.
- Exercise corrupt/duplicate/dangling/unindexed/symlink figure inventories,
  dangling retained-profile bindings, `pet/nomi/workspace`, ancestor reparse
  points, and sequence/name reuse; assert no untrusted ID becomes a delete path
  and no orphan workspace attaches to a new profile.
- Trigger a retired main-database lineage reset and assert that
  `storage-generation` rotates, conversation workspace roots are moved to the
  exact orphan destinations, companion dataset references are detached, and
  all remaining coupled metadata is removed before services start.
- For both main hard-cut triggers, assert the armed plan freezes the readable
  source owner, exact source-family identities/fingerprints, pre-minted target
  `UserId`, target staging basename, and plan hash. Replace the canonical path
  after quarantine with a fully valid current database carrying another owner
  or plan hash and assert the coordinator performs zero write/delete against
  it. Crash after target DB commit/install but before `main_committed` and
  accept recovery only for the exact target owner plus
  `installed_by_plan_hash`; a missing/wrong provenance row or partial canonical
  family leaves the backup and marker pending.
- Present a current main-v2 database whose fixed columns and every registered
  JSON path are canonical; assert version 3 is stamped with all rows
  byte-identical. Poison, one at a time, `creation_tasks.node_id`,
  `result_asset_ids[*]`, discriminated execution `actor_id`,
  `conversations.extra.remote_agent_id`/companion/public/MCP references, and
  artifact `payload.cron_job_id`; also poison conversation/channel/cron
  `AgentReference` fields and inject each retired `custom_agent_id` alias.
  Poison text-message writeback `written/failures[*].kb_id` and tool/ACP-tool
  `turn_id`, knowledge-source `credentialRef`, conversation model/pool/failover
  providers, each conversation/terminal IDMM bypass provider, and session MCP
  snapshot IDs independently; seed the retired flat phase-1 IDMM shape as a
  separate structural case. Assert each triggers a dataset hard cut with no
  value conversion/defaulting, while ID-looking strings inside raw tool
  input/output, connector scope, model names, and user text remain opaque and
  do not trigger it.
- Beside a recognized current main lineage, inject each unknown/altered schema
  object kind (especially a trigger) and structural `table_xinfo`/FK/CHECK/index
  mismatch; assert the exact current-contract proof triggers the same hard cut,
  while an unknown migration checksum still fails without mutation.
- Combine a pre-v2 Workshop or public-agent dataset, and separately a pending
  companion/runtime operation, with an unknown `AFTER DELETE`/`AFTER UPDATE`
  trigger whose body would mutate an unrelated sentinel row. Assert
  `main_schema_write_gate_v3` detects it before operation replay, companion main
  detach, or scoped-epoch SQL; the sentinel and every old row remain unchanged,
  and the first authorized logical main mutation is the armed closed-family
  hard-cut transition.
  Inject the trigger again between the private gate and `BEGIN IMMEDIATE` and
  prove the locked repeat rolls back before DML. Enumerate the exact allowed v2
  baseline and complete-v3-metadata manifests; reject every partial metadata
  table/guard variant while tolerating only the declared old sentinel/data
  values for the later scoped epochs.
- Put malicious triggers on the migration/default/installation-owner tables so
  the ordinary initializer would fire them. Assert the gate path never calls
  that initializer, never creates a missing DB, and installs its authorizer
  before preparing schema-dependent SQL. Repeat gate failure with missing SHM,
  hot WAL, and legal hot rollback input: no trigger or business row changes;
  any SQLite recovery/lock sidecar delta is closed, privately re-probed, and
  frozen as the hard-cut plan's new physical source family.
- Through create, public PATCH, and internal `update_extra`, attempt an invalid
  or wrong-prefix nested ID and assert zero writes. Poison a post-v3 row and
  assert get/list/WebSocket/export returns a controlled invariant error rather
  than skipping or serializing it. Specifically prove knowledge-source and
  failover readers no longer `.ok()` an invalid stored reference into `None`,
  and terminal IDMM/model/session-MCP readers never default past one. Restart
  with that poisoned current-v3 database and assert the every-boot full scan
  takes `current_contract_violation` hard cut instead of trusting the version
  stamp.
- Present a future migration, unknown checksum, and partial numeric-schema
  fingerprint and assert that none arms a marker or changes any file.
- Crash before `armed`, before/after `main_family_transition_started`, after
  each journal/SHM/WAL/main rename, during clean-DB create,
  after `main_committed`, during workspace salvage, companion detach, each side
  removal/parent flush, after generation install, and after each recovery-backup
  family unlink. Assert side stores are untouched before `main_committed`,
  workspace data is never deleted, phase predicates are reverified, every retry
  uses the generation, destinations, existence bitmap, and backup basename fixed
  in the original armed plan, and successful completion leaves no retired DB,
  WAL, SHM, rollback journal, or recovery copy. After transition starts,
  recovery validates the source/destination lattice without trying to re-prove
  the old logical trigger from the partially moved canonical family. At every
  split, source-original/destination-absent must move and
  source-absent/destination-exact must resume; both-present, both-absent, or a
  changed member must fail with no additional move.
- Run an explicitly requested full factory reset and assert that
  `public-agents/`, preview history, evolved skills, subordinate markers, and
  planned backup artifacts are deleted; inject a failure at every phase and
  assert reset remains pending. Coexist it with main/companion/import markers
  and assert factory mode supersedes them. Reject truncated, malformed,
  unknown-version, noncanonical-operation-ID, hash-mismatched, and root-mismatched
  requests with zero deletion. Change configuration/environment after request
  creation and prove the armed plan still uses the exact frozen data/work roots;
  then verify `work_dir` is recomputed after deleting its persisted override in
  the same boot without inheriting the prior exported value.
- Run companion hard cut, normal bridge disable/change, memory
  save/update/archive/delete, and full factory reset against a valid external
  bridge. Assert the mirror ledger/file/index remain bijective under the shared
  root lock and hash-CAS. A malformed config with an independently complete,
  validated ledger may still prove ownership and clean its exact root. A
  reparse-point root, a root whose lock cannot be acquired, or a changed,
  partial, or hash-mismatched index must retain the marker and perform zero
  deletion regardless of ledger completeness. Config/ledger/pending-plan root
  or hash conflicts likewise remain pending unless the exact legal change plan
  explains them. Only fixtures where every durable source loses the path may
  take the explicitly logged unreachable-detach path, without touching
  unrelated memories.
- Start an already-current main-v2 install with legacy public-agent config/audit
  IDs and live public-service bindings. Assert its independent v2 epoch durably
  removes the exact main sessions/conversations/plugin bindings before clearing
  the side roster and installing the sentinel. Seed pre-v2 Workshop rows in the
  same otherwise canonical main and assert both scoped epochs finish before the
  v3 scan, unrelated main rows remain byte-identical, and no broad dataset reset
  or storage-generation rotation occurs.
- Start an already-current main-v2 install with timestamp/random preview
  snapshots and assert the independent v2 preview epoch clears them with the
  main DB byte-identical. Then poison one current config, audit line, preview
  index ID/file mapping, dangling preview conversation target, symlink, or
  sequence file and prove deterministic durable side cleanup with no
  warn-and-skip behavior or unrelated main-row mutation.
- Delete a public agent with live plugin/session/conversation references and
  crash after every main/side phase; assert main detach completes before profile
  removal, no public-service conversation or binding survives, and DELETE never
  reports success early.
- Reject offline backup for any lifecycle/operation/import marker, commit/outbox
  row, missing/noncanonical generation, or unproved companion/skill graph, and
  prove the read-only command changes no source. For main and companion
  separately, snapshot uncheckpointed WAL with missing SHM and a legal hot
  rollback fixture; assert source DB/WAL/SHM/journal bitmap and hashes remain
  identical and only a consistent standalone DB is packaged. Reject an illegal
  super-journal reference. Crash at every backup probe/staging phase and assert
  no bundle becomes publishable, the source is unchanged, and valid private
  residue is recovered by the probe intent state machine. Restore across
  Windows/macOS/Linux logical roots, rebase every
  managed absolute path, clear bridge config/ledger, include evolved-skill
  projections, and reject any unmapped external path before installation.

### Workshop and embedded execution

- For every declared Workshop ID slot, reject retired short IDs, UUIDv4,
  compact UUIDs, uppercase UUIDs, wrong prefixes, and dangling references.
- Prove invalid document and archive writes leave existing DB rows and files
  unchanged.
- Import a canonical Workshop v2 archive and assert complete node/edge/asset
  remapping and typed origin fields.
- Assert embedded delegation emits an `exec_<canonical-uuidv7>` accepted by the
  execution-ID grammar.
- Assert preview saves emit `psnap_<canonical-uuidv7>` and that the DTO,
  filesystem lookup, backend response, and TypeScript parser share one type.
- Assert provider `call_` and `toolu_` generation is unchanged.

### Frontend and platform matrix

- Keep the adapter rejection test for a legacy companion learn-run ID.
- Keep equivalent rejection tests for nested remote-agent/artifact-cron and
  preview-snapshot IDs, text knowledge-writeback KB IDs, and tool-call turn IDs.
  Feed real canonical backend fixtures through companion, conversation,
  Workshop, public-agent, and preview adapters; poison each post-v3 message path
  and assert get/list/WebSocket/export returns one controlled backend invariant
  error before the strict frontend adapter receives it.
- Run focused Rust and Bun tests, workspace formatting/lint/typecheck, and the
  affected package suites locally.
- Run native CI jobs on `windows-latest`, `macos-latest`, and `ubuntu-latest`.
- On every OS, exercise same-filesystem sibling-temp/no-replace installation,
  file-and-parent durability, DB/WAL/SHM/rollback-journal family
  classification, no-follow path checks, and interrupted-marker recovery. Run
  each SQLite preflight against a closed source with hot WAL, missing SHM, a
  valid single-DB hot rollback journal, orphan sidecars, and an already complete
  family; assert only the private probe may gain/change/delete copied
  SHM/WAL/journal state, canonical fingerprints/bitmap remain byte-identical,
  and no hot state is separated from its DB. A malformed, escaping, or
  super-journal-bearing rollback journal must fail before arming with zero
  source mutation. Fault after intent temp write/install, building-root create,
  every member copy/sync, ready publication, SQLite open/recovery/close, ready-
  to-GC rename, each GC unlink, root removal, and intent removal. Every reachable
  partial allowlisted probe state must clean idempotently and retry; malformed
  intent, extra entry, link/reparse point, or changed root identity must fail
  closed without source mutation. On Windows, additionally exercise
  replacement primitives, sharing violations, and lock retry. On macOS, fault
  `sync_all`/`F_FULLFSYNC` for each
  temporary file and changed parent directory and assert the phase stays
  pending; on macOS and Linux, exercise symlink/ancestor-reparse rejection and
  crash recovery across directory renames.

## Acceptance criteria

- Upgrading any pre-v7 companion store produces one newly initialized v7
  shared domain and cannot surface the reported
  `Invalid companion-learn-run id` error.
- No retired companion row, quarantine entry, pre-v7 raw event, or v1 bundle
  is retained or converted.
- Every new persisted NomiFun entity ID matches its registered prefix and
  application-canonical lowercase hyphenated RFC 9562 UUIDv7.
- Every main fixed/structured JSON ID slot is checked against its exact
  registered type or explicit discriminator union; generic or wrong-domain
  prefixes cannot pass merely because their UUID body is valid.
- No bootstrap SQL writes a recognized current main until its exact
  schema/trigger manifest passes both private preflight and an in-transaction
  locked repeat; unknown SQL can never run before a required hard cut.
- Invalid IDs are rejected before writes and before wire serialization; list
  paths never hide a poisoned row by skipping it.
- Canonical current companion profiles, provable canonical figures, every
  regular workspace, and the model cache survive the companion-only hard cut.
- A companion hard cut also removes old companion conversations/sessions and
  external evolved-skill bodies, and leaves no main reference to a discarded
  companion owner.
- Companion/public-agent deletes, thread/channel creation, skill projection,
  import, and external-memory mirroring are durable cross-store operations;
  crash recovery cannot publish an ownerless row, dangling reference, duplicate
  thread, or executable rejected skill.
- A retired main-database hard cut rotates the dataset generation, orphans
  conversation workspace roots without deleting them, detaches companion
  main-dataset references while preserving approved companion assets, and
  removes every other coupled metadata store plus every recovery-only copy of
  the retired DB family.
- Windows, macOS, and Linux follow one state machine and pass their native CI
  matrix.
- Current-main users independently start fresh for pre-v2 public-agent and
  preview-history stores; all new preview snapshot IDs are registered canonical
  UUIDv7 IDs, and side cleanup cannot leave dangling main references.
- Frontend parsing remains strict; no historical compatibility path is added.
