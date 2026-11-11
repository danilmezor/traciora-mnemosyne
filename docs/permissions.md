# Permissions

Every operation is checked against the caller's tenant, its grants, its rank and the
path's visibility. Read checks cover content, history, diffs, search hits, counts,
snippets, original documents and extracted text alike.

## Permissions

| Permission | Allows |
| --- | --- |
| `read` | read files, history, diffs, search, graph |
| `propose` | create proposals; apply an approved proposal |
| `edit` | publish directly with `edit` (humans only) |
| `approve` | approve or reject proposals (humans only) |
| `admin` | read and replace the tenant policy (`policy_get`, `policy_set`) |

An actor's effective permission on a path is the union of its **grants** and its
**rank**. Neither ever removes the other.

## Grants

A grant gives one permission to one actor on one scope:

```json
{"tenant_id":"acme","actor_id":"alice","permission":"edit","scope":"handbook"}
```

The scope `""` is the whole tenant. Any other scope covers that path and everything
below it at `/` boundaries (`handbook` covers `handbook/a.md`, not `handbook2`).
Grants are independent: `edit` does not imply `read`.

One exception: a **human** with `admin` on the tenant root also gets `read` and
`approve` on every path, so someone can always decide a proposal. It does not give
`edit` or `propose`.

Grants come from the policy file at `content-init`, from `content-grant`, or from
`policy_set` by a tenant admin.

## The rank ladder

Each tenant has a section in the policy with its roles, members and rungs. When
none is stored, this default applies:

```json
{"roles":["worker","junior","middle","senior","administrator"],
 "members":{"alice":{"role":"senior","kind":"human"}},
 "rungs":{"propose_from":"middle","publish_from":"senior","visibility_from":"senior"},
 "root_seen_from":"junior"}
```

- `roles` is ordered lowest first; 1 to 16 roles, names 1 to 64 characters.
- `members` maps an `actor_id` to its `role` and `kind`. A credential whose `kind`
  differs from its membership fails every request with `policy_mismatch`.
- `rungs` set the lowest role that may propose, publish (`edit` and `approve`) and
  change visibility.
- `root_seen_from` is the lowest role that sees the tenant root.
- Optional: `colour_seed` (a number for default folder colours) and
  `agent_sees_above_rank` (see below).

For a member whose role is at or above a path's `seen_from`:

- `read` always;
- `propose` when the role reaches `propose_from` and the member is human;
- `edit` and `approve` when the role reaches `publish_from` and the member is human.

An agent member gets only `read` from its rank. An agent's `propose` comes from a
grant. `admin` comes only from a grant.

## Visibility

Every path has an effective `seen_from`: its own, else its nearest ancestor's,
else `root_seen_from`. Set it with a `set_meta` change in an `edit`:

```json
{"type":"set_meta","path":"board","seen_from":"senior","colour":"cyan"}
```

- Raising `seen_from` needs a role at or above `visibility_from`, or an `admin`
  grant covering the path. Only such an admin may set it above their own role
  (`visibility_above_own_rank`).
- No entry may rank below its folder (`visibility_below_folder`), also after a
  rename or restore.
- `colour` needs `edit`. Colours: `blue`, `magenta`, `green`, `lavender`,
  `orange`, `cyan`, `other`.
- `set_meta` is never allowed in a proposal (`set_meta_requires_edit`).
- A role named by a `seen_from` at head cannot be removed from the ladder; re-rank
  those paths first.

## Non-disclosure

A path hidden by rank answers exactly like a path that does not exist:

- `read`, `read_binary`, `diff`, `stat`, `links`, `log`, `graph` scopes and the
  rest answer `not_found`, the same code and message as for an absent path.
- Hidden paths are never listed, counted, searched, exported, linked or proposed on.
- `log` reports how many in-scope changes are hidden (`hidden_paths`) but never
  their names. `graph` shows a node's full link counts but only edges you can see.
- The tenant root answers `forbidden` when hidden.
- A caller who can read a path but sits below a rung gets `forbidden`.

The past is as hidden as it was then and as it is now. A historical read needs
`read` on the path both at head and in the revision being read. Raising a folder
hides its history; lowering it later never reveals old bytes. A file moved out of a
hidden folder stays hidden from those who could not read its old name; copy its
content to a new file to publish it.

`delete` or `rename` of a folder holding something the caller cannot read answers
`forbidden` and writes nothing.

Proposal and revision IDs a caller cannot see answer `not_found`, so IDs cannot be
probed.

## What agents can and cannot do

Agents can:

- read, search and navigate what their grants, rank and (if delegated) their human
  allow;
- `propose` changes, `withdraw` their own proposals, `rebase_proposal` them, and
  `apply` them once a human has approved;
- import a directory as a proposal (`import_commit` with `mode: "propose"`).

Agents cannot:

- `edit`, `set_meta`, `retry_extraction` or commit an import in `edit` mode
  (`human_required`);
- `approve` or `reject` anything (`human_required`);
- read or change the policy (`human_required`);
- assert an identity in a request; identity comes only from the credential file.

A delegated agent (`on_behalf_of`) needs every permission for itself **and** for its
human. The proposer's human cannot approve the proposal (`self_approval_denied`).

`links` and `neighbourhood` list `above_rank`: link targets in a visible file that
the caller may not open, with the rank that can. Agents get an empty list unless the
tenant sets `agent_sees_above_rank: true`; then a delegated agent gets what its
human would get. An agent alone never gets any.

## Inspecting permissions

- `me` returns your actor, kind, role, the ladder and rungs.
- `permissions {path}` returns the effective `seen_from`, where it is inherited
  from, the colour and a `can` map (`read`, `propose`, `edit`, `approve`, `admin`).
  The path need not exist, so a UI can check before creating a file.
- `deciders {proposal_id}` lists the humans who may decide a proposal. `fallback:
  true` means only root admins can (or nobody can).

## Changing the policy

`policy_get` and `policy_set` need a human with `admin` on the tenant root, not
acting for anyone. `policy_set` replaces the tenant's grants and rank section and
must carry the `expected_digest` from `policy_get`; a stale digest is
`policy_conflict`. A policy for another tenant is `forbidden`.

Any policy change returns that tenant's approved, unapplied proposals to `pending`.
