# Concepts

Mnemosyne is a versioned knowledge base that a team and its AI agents share.
People publish. Agents suggest. Ranks decide who sees what.

This page explains the moving parts. The exact operations are in [api.md](api.md).

## Store and tenants

A **store** is one private directory on the host. It holds every tenant's content,
history, proposals and policy in SQLite metadata plus content-addressed blobs.
Nothing outside Mnemosyne should edit it.

A **tenant** is one knowledge base inside the store, named by `tenant_id`. Every
request runs inside exactly one tenant. Tenants never see each other.

Paths inside a tenant are relative, `/`-separated and at most 1024 bytes. They may
not start with `/`, contain `.` or `..` segments, empty segments, backslashes or
control characters. The empty path `""` is the tenant root.

## Actors and credentials

An **actor** is whoever makes a request. It is identified by the host, never by the
request body. The operator writes a credential file that maps opaque bearer tokens
to identities:

```json
{"credentials":[
  {"token":"HUMAN_TOKEN","identity":{"tenant_id":"acme","actor_id":"alice","kind":"human"}},
  {"token":"AGENT_TOKEN","identity":{"tenant_id":"acme","actor_id":"assistant","kind":"agent","on_behalf_of":"alice"}}
]}
```

- `kind` is `human` or `agent`.
- `on_behalf_of` is optional and only valid for agents. It names the human who
  started the agent's work. A delegated agent can do only what **both** it and
  that human may do on the path.
- An agent without `on_behalf_of` acts on its own grants alone.

The service rereads the credential file before every request, so removing a token
takes effect at once.

## Grants and ranks

Permissions are `read`, `propose`, `edit`, `approve` and `admin`. An actor gets
them in two ways, and the two add up:

- **Grants** in the policy give one permission to one actor on a scope (a path and
  everything below it, or `""` for the whole tenant).
- **Ranks** come from the tenant's role ladder (`worker`, `junior`, `middle`,
  `senior`, `administrator` by default). A member's role decides what it may read
  and whether it may propose or publish.

Paths can be hidden from lower ranks with `seen_from`. A hidden path answers
exactly like a path that does not exist. See [permissions.md](permissions.md).

## Revisions

Every published change is an immutable **revision**. A revision can touch many
files at once, and it publishes all of them or none.

A revision records:

- `id` and `parent` (opaque strings; `r0` is the empty tenant),
- the `actor` who published it, and the `proposal_id` it came from, if any,
- `at` (UTC, millisecond precision), an optional `message` and `origin`,
- `kind`: `edit`, `proposal`, `restore`, `rename`, `meta`, `import` or `new_version`,
- `changes`: one diff per file, with added and removed line counts for text.

Files keep a stable `file_id` across renames, so history follows a file when it
moves. Old versions can be read, diffed and restored; a restore is a new revision.
Text and binary files are both versioned. The first bytes stored for a file stay
readable as its original (`read_binary` with `original: true`).

Change types are `put`, `put_binary`, `create_folder`, `delete`, `rename`,
`restore` and `set_meta`. One request carries 1 to 100 changes on distinct paths.

## Proposals and approval

Humans with `edit` can publish directly with `edit`. Agents never can: an agent
calling `edit` gets `human_required`. An agent instead creates a **proposal**,
an exact changeset against a base revision, and a human decides it.

```text
agent   propose  ──►  pending
human   review   (sees the exact diff and its digest)
human   approve  ──►  approved        or  reject  ──►  rejected
agent   apply    ──►  applied (a new revision)
```

Rules that keep this safe:

- The **digest** binds the exact changes and base. `approve` and `apply` must send
  the digest the reviewer saw; anything else is `proposal_mismatch`. To change the
  content, make a new proposal.
- Only a human can approve or reject. The proposer and the proposer's
  `on_behalf_of` human cannot decide their own proposal (`self_approval_denied`).
- `apply` before approval is `approval_required`. Permissions are checked again at
  apply time, for the caller, the proposer and the approver.
- A policy change in the tenant returns every approved, unapplied proposal to
  `pending`. It must be approved again.
- The proposer (or its `on_behalf_of` human) can `withdraw` a pending or approved
  proposal.

### Stale proposals

A proposal is **stale** when a path it touches changed after its base. A head that
moved on other files does not make it stale; it is approved and applied on today's
head. A stale proposal can still be rejected or withdrawn, but `approve` and
`apply` answer `stale_base`.

`rebase_proposal` with `keep: "fitting"` carries a stale proposal onto the current
head. It keeps the changed paragraphs that still apply cleanly, keeps head's text
everywhere else, and creates a new pending proposal. The old one becomes
`superseded`. The new proposal needs a fresh decision.

Proposal states: `pending`, `approved`, `rejected`, `applied`, `withdrawn`,
`superseded`.

## Idempotency

Every mutation carries an `idempotency_key`. Sending the same request with the
same key again returns the original result and writes nothing. That makes a retry
after a lost response safe. Reusing a key for a different request is
`idempotency_conflict`. Keys are bound to the caller's identity.

## Derived indexes

Search, extraction, the link graph and semantic vectors live in disposable caches
inside the store. They are rebuilt automatically when missing or out of date and
never decide who may see what. Backups leave them out.
