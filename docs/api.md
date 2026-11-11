# API reference

The service speaks typed JSON, API version 2. The same requests work through the
local socket, the one-shot CLI and the Python client.

## Calling the API

A request:

```json
{"version":2,"idempotency_key":"k-1","operation":{"type":"propose", "...":"..."}}
```

- `version` is `2`. Version `1` is still accepted for the original text operations
  (`head`, `read`, `list`, `propose`, `review`, `approve`, `reject`, `apply`,
  `edit` with `put` changes, `history`, `diff`); everything else answers
  `unsupported_version` under it.
- `idempotency_key` is required, nonempty, on every mutation: `edit`, `propose`,
  `approve`, `reject`, `withdraw`, `apply`, `rebase_proposal`, `import_commit`.
- Unknown fields are rejected. Identity is never part of a request.

A response holds exactly one of `result` and `error`:

```json
{"version":2,"result":{"...":"..."}}
{"version":2,"error":{"code":"not_found","message":"not found"}}
```

The response echoes the request's version. The `code` is stable; the `message` is
for people.

### Transports

**Socket service.** `mnemosyne content-serve --store DIR --credentials FILE --socket
PATH` listens on a Unix socket (`0600`, parent directory `0700`). Each frame is a
4-byte big-endian length followed by JSON. The request frame wraps the request with
a token:

```json
{"token":"OPAQUE_BEARER","request":{"version":2,"operation":{"type":"head"}}}
```

Requests are at most 128 MiB, responses at most 256 MiB. A connection serves up to
16 requests; each frame must arrive within two seconds. Requests run one at a time.
Credentials are reread on every request.

**One-shot CLI.** `mnemosyne content` reads one request from stdin and prints one
response. The token comes from `MNEMOSYNE_CONTENT_TOKEN`. Use `--socket PATH` to
talk to a running service, or `--store DIR --credentials FILE` to open the store
directly. Exit status is 0 on success, 1 on error.

## Content

| Operation | Fields | Result |
| --- | --- | --- |
| `head` | none | current revision ID; `"r0"` when empty |
| `read` | `path`, `revision?` | `{revision_id, path, content}`; text files only (`not_text` otherwise) |
| `read_binary` | `path`, `revision?`, `original?` (false), `encoding?` (`array` or `base64`) | `{revision_id, path, entry, bytes, bytes_base64?}` |
| `list` | `scope` | visible paths below `scope`, folders included |
| `stat` | `path`, `revision?` | kind, mime, words, characters, tokens, created and changed stamps, `seen_from`, colour |
| `read_many` | `paths` (1–64), `max_bytes?` (default 262,144, up to 8 MiB) | `{revision_id, items, used_bytes}` |

`entry` is `{file_id, kind, blob, original_blob, meta?}`; `kind` is `text`,
`binary` or `folder`. `original: true` returns the first bytes stored for that file.
With `encoding: "base64"`, `bytes` is empty and `bytes_base64` holds the data.

`read_many` reads up to 64 paths at one head revision under one byte budget, spent in
request order. Each item is `{path, ok, kind?, content?, truncated?, error?}`. A
binary returns its cached extracted text (`kind: "extracted"`). Item errors:
`not_found`, `invalid_path`, `not_text`, `extraction_pending`, `max_bytes`.

`stat` counts words, characters and `cl100k_base` tokens; for a binary it counts the
extracted text, or answers `null` when there is none.

## Knowledge reads (Markdown)

| Operation | Fields | Result |
| --- | --- | --- |
| `properties` | `path`, `revision?` | frontmatter as `properties`, with source ranges |
| `outline` | `path`, `revision?` | `preamble` and `headings` (`section_id`, `level`, `title`, `source`) |
| `read_section` | `path`, `revision?`, one of `section_id` or `section` | `{source, section_id, content}` |

Section IDs are `s0` (the text before the first heading), then `s1`, `s2`, … in
document order. `section` takes an ID or an exact heading title (the first match
wins). A section runs from its heading to the next heading of the same or higher
level. Every `source` carries `path`, `revision_id`, byte offsets and line numbers.
Bad selectors are `invalid_section`; no match is `section_not_found`.

Frontmatter starts with `---` on the first line and must be a mapping with unique
string keys (`invalid_frontmatter` otherwise). Limits: 2 MiB text, 64 KiB
frontmatter, 4096 headings.

## Changes and proposals

| Operation | Fields | Result |
| --- | --- | --- |
| `edit` | `base_revision`, `changes`, `message?`, `origin?` | the new revision (humans only) |
| `propose` | `base_revision`, `changes`, `reason?`, `origin?` | the proposal, `pending` |
| `review` | `proposal_id` | the full proposal with diff and digest |
| `approve` | `proposal_id`, `digest`, `reason?` | the proposal, `approved` (humans only) |
| `reject` | `proposal_id`, `digest`, `reason?` | the proposal, `rejected` (humans only) |
| `apply` | `proposal_id`, `digest` | the new revision |
| `withdraw` | `proposal_id`, `digest` | the proposal, `withdrawn` (proposer or its human) |
| `rebase_proposal` | `proposal_id`, `digest`, `keep: "fitting"` | a new pending proposal; the old one `superseded` |
| `list_proposals` | `states?`, `limit?` (1–100, default 20), `offset?` | `{proposals, next_offset}` |
| `deciders` | `proposal_id` | `{proposal_id, deciders, fallback}` |

`message` and `reason` are at most 2048 bytes with no control characters except
newline (`invalid_message`). `origin` labels assistant-drafted work and is never an
authorization input:

```json
{"source":"assistant","model":"model-name","chat_id":"chat-1"}
```

`source` must be `assistant`; `model` is at most 128 bytes, `chat_id` 256
(`invalid_origin`).

A proposal is:

```json
{"id":"p…","tenant_id":"acme","proposer":{"tenant_id":"acme","actor_id":"assistant","kind":"agent"},
 "base_revision":"r…","changes":[…],"diff":[…],"digest":"…","state":"pending",
 "approver":null,"decisions":[],"revision_id":null,"at":"2026-01-01T12:00:00.000Z",
 "reason":"…","origin":null,"supersedes":null}
```

`decisions` lists every `{actor, approved, policy_digest, at, reason}`.
`list_proposals` returns compact summaries (`id`, `state`, `digest`,
`base_revision`, `revision_id`, `proposer`, `at`, `reason`, `origin`,
`supersedes`, the first eight visible `paths`, `path_count`), only for proposals
the caller may fully read.

A revision is:

```json
{"id":"r…","parent":"r…","actor":{…},"proposal_id":"p…","changes":[…],
 "at":"…","message":"…","origin":null,"kind":"proposal"}
```

### Change types

| `type` | Fields | Effect |
| --- | --- | --- |
| `put` | `path`, `content` | create or replace UTF-8 text |
| `put_binary` | `path`, `bytes` (array of 0–255) | create or replace bytes |
| `create_folder` | `path` | create an explicit folder |
| `delete` | `path` | delete a file, or a folder and everything in it |
| `rename` | `path`, `to` | move a file or folder, keeping file identities |
| `restore` | `path`, `revision_id` | republish a file as it was in that revision |
| `set_meta` | `path`, `seen_from?`, `colour?` | set visibility and colour; `edit` only |

A request has 1 to 100 changes on distinct paths; each file is at most 16 MiB. A
destination that already exists fails. Restoring onto a different file, or
duplicating a file that still lives elsewhere, is `identity_conflict`. Folders
cannot be restored.

### Bases and stale writes

`edit`, `propose`, `approve` and `apply` compare the base with head path by path.
They fail with `stale_base` only when a path they touch changed since the base
(by file identity across renames, by name, or through a containing folder).
Otherwise they publish on the current head. On `stale_base`, reread and prepare a
new change, or use `rebase_proposal`.

`rebase_proposal` supports `put` changes to existing text files only
(`rebase_unsupported`). It fails with `not_stale` when nothing changed, and
`nothing_fits` when no changed paragraph still applies.

## History

| Operation | Fields | Result |
| --- | --- | --- |
| `history` | `path` | revisions that touched this file, following renames |
| `diff` | `revision_id` | the revision's file diffs |
| `log` | `scope?`, `limit?` (1–100, default 20), `offset?` | newest-first entries with paths, line counts, `hidden_paths` |
| `render` | `revision_id` or `proposal_id`, `path?`, `offset?` | the change as paragraphs and word spans |

A file diff carries `path`, `previous_path?`, `file_id`, `kind`, `deleted`,
`before`, `after`, `before_blob`, `after_blob`, `meta_before?`, `meta_after?`,
`added_lines?` and `removed_lines?`.

`render` aligns text by paragraph and marks words `same`, `added` or `removed`.
Unchanged runs collapse to one entry with a line count. It pages 12 changed
paragraphs per file (`next_offset`); without `path` it renders the first 24
files and summarises the rest. `summary.rewritten` is true when most of a file
changed. For a stale pending or approved proposal it adds `stale: true`,
`head_revision`, and per paragraph `fits` and `conflict`.

## Search and find

| Operation | Fields | Result |
| --- | --- | --- |
| `search` | `query`, `mode?`, `scope?`, `extensions?`, `fields?`, `typos?`, `min_score?`, `limit?`, `offset?`, `snapshot_id?` | ranked hits with snippets |
| `find` | `query`, `scope?`, `limit?` (1–50, default 20), `typos?` | files and folders by name and path prefix |

See [search.md](search.md) for modes, ranking, paging and hit fields.

## Graph

| Operation | Fields | Result |
| --- | --- | --- |
| `graph` | `scope?`, `offset?`, `limit?` (1–2000) | nodes, `links` and `contains` edges, unresolved links |
| `links` | `path` | `out`, `in`, `unresolved`, `above_rank`, `titles`, `anchors` |
| `neighbourhood` | `path`, `depth?` (1–2, default 1), `limit?` (1–64, default 32) | the visible subgraph around a file or folder |

See [search.md](search.md#link-graph).

## Document extraction

| Operation | Fields | Result |
| --- | --- | --- |
| `extraction_status` | `path` | the cached status, or `pending`; no text |
| `extracted_text` | `path` | extracts if needed, returns segments |
| `retry_extraction` | `path` | extracts again; needs `read` and `edit` |

All three return `{revision_id, path, file_id, source_blob, segment_count,
extraction}` for the current version of a binary file (`not_binary` otherwise).
See [search.md](search.md#document-extraction).

## Import and export

| Operation | Fields | Result |
| --- | --- | --- |
| `import_preview` | `base_revision`, `files`, `offset?`, `next_offset?`, `conflict_policy?` | outcomes, exact changes and a digest; writes nothing |
| `import_commit` | `preview`, `mode` (`edit` or `propose`), `message?`, `origin?` | a revision, a proposal, or `{revision_id, next_offset, skipped: true}` |
| `export_snapshot` | `scope?`, `revision?`, `offset?`, `limit?`, `expected_policy_digest?`, `encoding?` | a page of files with current and original bytes |

`files` items are `{path, bytes, directory}`, at most 32 entries and 16 MiB per
batch. `conflict_policy` is `error` (default), `skip` or `overwrite`. Outcomes are
`create`, `overwrite`, `skip` or `conflict`; a preview with any `conflict` cannot
be committed (`import_conflict`). The base must be the head. See
[operations.md](operations.md#import-and-export).

## Identity and policy

| Operation | Fields | Result |
| --- | --- | --- |
| `me` | none | `{actor_id, kind, role, roles, rungs, root_seen_from}` |
| `permissions` | `path` | `{path, seen_from, inherited_from, colour, colour_source, can}` |
| `policy_get` | none | `{policy, digest}` for the caller's tenant |
| `policy_set` | `policy`, `expected_digest` | `{initialized: true}` |

See [permissions.md](permissions.md).

## Error codes

Common codes. Others are specific to one operation and named on its page.

| Code | Meaning |
| --- | --- |
| `unauthenticated` | missing or unknown token |
| `forbidden` | no permission, or a hidden tenant root |
| `not_found` | absent, or hidden from the caller |
| `human_required` | an agent called a human-only operation |
| `policy_mismatch` | the credential's kind differs from its membership |
| `invalid_request` | malformed request, missing key, value out of range |
| `invalid_path` | a path breaks the path rules |
| `unsupported_version` | the operation or field needs version 2 |
| `idempotency_conflict` | a key reused with a different request |
| `replay_expired` | the stored result of this key was pruned by retention |
| `stale_base` | a touched path changed after the base |
| `conflict`, `path_conflict`, `already_exists`, `identity_conflict` | the change collides with what is at head |
| `proposal_mismatch` | the digest is not the proposal's |
| `approval_required` | `apply` before approval |
| `invalid_review_state` | the proposal's state does not allow this |
| `self_approval_denied` | the proposer or its human tried to decide |
| `not_proposer` | `withdraw` by someone other than the proposer or its human |
| `policy_conflict` | `policy_set` with a stale digest |
| `invalid_policy` | a policy breaks the ladder or grant rules |
| `visibility_above_own_rank`, `visibility_below_folder`, `set_meta_requires_edit` | `set_meta` rules |
| `invalid_message`, `invalid_origin` | message, reason or origin out of bounds |
| `not_text`, `not_binary` | wrong file kind for the operation |
| `content_limit`, `changes_limit` | a change set exceeds size limits |
| `history_pruned` | the revision was removed by retention |
| `invalid_search`, `stale_search` | bad query; changed search snapshot |
| `search_unavailable`, `semantic_unavailable`, `graph_unavailable` | a derived index or model cannot be used |
| `response_too_large` | the response exceeds 256 MiB |
| `storage_error` | the store could not be read or written |
