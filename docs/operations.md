# Operations

Operator commands act on the store directly. They are not part of the tenant API,
because they see every tenant. Run them as the OS user that owns the store, and keep
the store, backups and credential files in private directories.

Every command prints one JSON response, like the API: `{"version":2,"result":…}` or
`{"version":2,"error":{"code","message"}}`, with exit status 0 or 1.

## Setup commands

| Command | Does |
| --- | --- |
| `content-init --store DIR --policy FILE` | create the store and its first policy (once) |
| `content-grant --store DIR --tenant T --actor A --permission P --scope S [--while-serving true]` | add one grant |
| `content-serve --store DIR --credentials FILE --socket PATH` | run the local service |
| `content --socket PATH` or `content --store DIR --credentials FILE` | send one request from stdin |

`content-grant` answers `{tenant_id, actor_id, permission, scope, added,
policy_digest}`; `added: false` means the grant already existed. While a service is
running it answers `store_in_use` unless you pass `--while-serving true`; the running
service then honours the grant on its next request. Adding a grant returns that
tenant's approved, unapplied proposals to `pending`. An uninitialised store is
`not_initialized`.

`content-serve` holds a lock on the store; a second service is refused with
`store_in_use`. The lock is released when the process dies. The socket's parent
directory must exist with mode `0700`, and the socket path must not exist. After a
crash, check and remove a stale socket file before restarting.

## Policy administration

A tenant's admin manages grants and ranks through the API, not the CLI:

```json
{"version":2,"operation":{"type":"policy_get"}}
```

```json
{"version":2,"operation":{"type":"policy_set","expected_digest":"DIGEST_FROM_POLICY_GET",
 "policy":{"grants":[{"tenant_id":"acme","actor_id":"alice","permission":"admin","scope":""}],
           "tenants":{"acme":{"roles":["worker","junior","middle","senior","administrator"],
             "members":{"alice":{"role":"administrator","kind":"human"}},
             "rungs":{"propose_from":"middle","publish_from":"senior","visibility_from":"senior"},
             "root_seen_from":"junior"}}}}}
```

`policy_set` replaces the whole tenant policy, so send every grant you want to keep,
including your own `admin`. Leaving out `tenants` resets the rank section to the
default. The digest guards against concurrent changes (`policy_conflict`). The
caller must be a human with `admin` at the tenant root. See
[permissions.md](permissions.md).

Credentials are not part of the policy. Edit the credential file to add or revoke
tokens; the service rereads it on every request.

## Backup and restore

```sh
mnemosyne content-backup --store ./store --backup /backups/nightly
mnemosyne content-restore --backup /backups/nightly --store ./store-restored
```

A backup is a directory with `manifest.json`, `authority.sqlite3` and
`blobs/<sha256>`. It covers every tenant: content, history, proposals, decisions,
policy and idempotency results. The result reports `blob_count`, `blob_bytes` and
`database_sha256`.

- The destination must be absent or empty, and must not be inside the source.
- Both commands stage next to the destination and publish with one rename, so you
  get a complete result or none. An interrupted run may leave a
  `.mnemosyne-stage-*` directory; remove it once nothing is running.
- Restore checks every hash, size and history chain before publishing.
- Backups are **not encrypted**. Hashes detect damage, not tampering.
- Left out: search, extraction, graph and vector caches (they rebuild), model files,
  credential files, sockets and locks. Provide credentials again after a restore.
- Backup holds the store lock while it runs; requests wait.

## Retention and garbage collection

History is kept forever until you prune it. Pruning is always explicit, one tenant
at a time, in two steps.

Plan:

```sh
mnemosyne content-retention --store ./store --tenant acme --keep-revisions 50
```

The dry run reports the plan without changing anything: `plan_digest`,
`revisions_before`, `retained_revisions`, `pruned_revision_ids`,
`pinned_revision_ids`, `checkpoint_revision_ids`, `retained_blob_bytes`,
`orphan_blob_count`, `orphan_blob_bytes`, `expired_replay_count`.

Apply exactly that plan:

```sh
mnemosyne content-retention --store ./store --tenant acme --keep-revisions 50 \
  --mode apply --expected-plan PLAN_DIGEST
```

If anything changed since the dry run, apply answers `retention_plan_conflict`;
run a new dry run. Apply reports `deleted_blob_count` and `deleted_blob_bytes`.

What is kept:

- the newest N revisions, plus every revision a proposal, an applied proposal or a
  restore points at (so more than N may stay);
- every proposal, review and decision;
- current files, policy and file lineage;
- every idempotency result, unless you pass `--expire-pruned-edit-replays true`
  on both the dry run and apply. That drops the stored results of direct edits
  whose revisions are pruned; retrying such a key answers `replay_expired`.

Reading, diffing or restoring a pruned revision answers `history_pruned`. `history`
stops at the oldest kept revision.

Garbage collection runs as part of apply. It removes blobs that no tenant still
needs, across the whole store, after checking every tenant. An interrupted apply is
safe: rerun the dry run and apply. Nothing prunes automatically, and the SQLite file
does not shrink on its own. Take a backup first if you want to keep what you prune.

## Import and export

### Export

```sh
MNEMOSYNE_CONTENT_TOKEN=… mnemosyne content-export --socket run/content.sock \
  --destination /exports/handbook --scope handbook
```

Writes the files the caller may read, as of one revision, into two trees:
`current/` (today's bytes) and `original/` (each file's first stored bytes). The
destination must not exist; the export is staged and published with one rename.
Paths and bytes are kept exactly. The result is `{revision_id, entries,
destination}`.

Underneath, `export_snapshot` returns pages of up to 32 entries and 32 MiB. Later
pages send the first page's `revision` and `expected_policy_digest`; a policy change
mid-export answers `export_policy_changed`.

### Import

For a one-off import from the command line, preview a batch:

```sh
MNEMOSYNE_CONTENT_TOKEN=… mnemosyne content-import-preview --socket run/content.sock \
  --source /data/handbook --base-revision r0 --offset 0 --batch-size 32 \
  --conflict-policy error
```

Then commit the exact preview you reviewed:

```json
{"version":2,"idempotency_key":"import-1","operation":{"type":"import_commit",
 "preview":PREVIEW_RESULT,"mode":"edit","message":"Imported the handbook"}}
```

- `mode: "edit"` publishes a revision (humans). `mode: "propose"` creates a
  proposal for a human to approve (agents).
- Each batch publishes atomically; a directory does not. Continue with the preview's
  `next_offset` and the new head as base.
- The base must equal the head; there is no silent rebase.
- Existing paths need `--conflict-policy skip` or `overwrite`. An all-skipped batch
  answers `skipped: true`.
- UTF-8 files become text; PDF, DOCX and non-UTF-8 files become binaries. Symlinks,
  special files and unsafe paths are rejected. File modes and timestamps are not
  kept.

For large or restartable imports use the Python `ImportSession`
([python-client.md](python-client.md#import-sessions)). It captures the source once
and resumes from a durable cursor.

Import is a one-time copy, not a sync. Mnemosyne never watches a directory.
