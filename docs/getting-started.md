# Getting started

This walk-through builds Mnemosyne, creates a store with one tenant, starts the
local service and runs a first round trip: an agent proposes a page, a human
approves it, the agent publishes it.

You need Rust (stable, with Cargo), Python 3.10 or newer, and macOS or Linux. The
service listens on a Unix socket.

## 1. Build

```sh
cargo build --release
```

The binary is `target/release/mnemosyne`. The examples below call it `mnemosyne`.

## 2. Write a policy and a credential file

Keep both files in a private directory. Anyone who can read the credential file
can act as any identity in it.

`policy.json` gives Alice, a human, the right to read and approve, and an agent the
right to read and propose:

```json
{"grants":[
  {"tenant_id":"acme","actor_id":"alice","permission":"read","scope":""},
  {"tenant_id":"acme","actor_id":"alice","permission":"approve","scope":""},
  {"tenant_id":"acme","actor_id":"alice","permission":"admin","scope":""},
  {"tenant_id":"acme","actor_id":"assistant","permission":"read","scope":""},
  {"tenant_id":"acme","actor_id":"assistant","permission":"propose","scope":""}
]}
```

`credentials.json` maps tokens to those identities:

```json
{"credentials":[
  {"token":"replace-with-a-long-random-secret-1","identity":{"tenant_id":"acme","actor_id":"alice","kind":"human"}},
  {"token":"replace-with-a-long-random-secret-2","identity":{"tenant_id":"acme","actor_id":"assistant","kind":"agent"}}
]}
```

The agent here acts on its own grants. If it set `on_behalf_of: "alice"`, Alice
would also need `propose`, and she could not approve its proposals herself
(`self_approval_denied`); a second human would decide.

## 3. Initialise the store

```sh
mnemosyne content-init --store ./store --policy policy.json
```

This runs once per store. To add a grant later, use `content-grant`:

```sh
mnemosyne content-grant --store ./store --tenant acme --actor bob \
  --permission read --scope ""
```

## 4. Start the service

The socket's parent directory must exist with mode `0700`.

```sh
mkdir -m 700 run
mnemosyne content-serve --store ./store --credentials credentials.json \
  --socket run/content.sock
```

The service prints `Content listening: run/content.sock` to stderr. `Ctrl-C`
finishes the current request and removes the socket. Only one service may hold a
store at a time.

Check it from another shell with the one-shot client:

```sh
echo '{"version":2,"operation":{"type":"head"}}' \
  | MNEMOSYNE_CONTENT_TOKEN=replace-with-a-long-random-secret-1 \
    mnemosyne content --socket run/content.sock
```

```json
{"version":2,"result":"r0"}
```

`r0` means the tenant is empty.

## 5. Propose, approve, apply with Python

Install the client (standard library only):

```sh
python3 -m pip install ./clients/python
```

```python
from mnemosyne_client import Client

SOCKET = "run/content.sock"
agent = Client(SOCKET, "replace-with-a-long-random-secret-2")
alice = Client(SOCKET, "replace-with-a-long-random-secret-1")

# The agent proposes a new page against the current head.
head = agent.call({"type": "head"})
proposal = agent.call(
    {"type": "propose", "base_revision": head,
     "changes": [{"type": "put", "path": "handbook/welcome.md",
                  "content": "# Welcome\n\nStart here.\n"}],
     "reason": "Add a welcome page"},
    idempotency_key="welcome-propose-1")
print(proposal["state"])            # pending

# Alice reviews the exact diff and approves that digest.
review = alice.call({"type": "review", "proposal_id": proposal["id"]})
for change in review["diff"]:
    print(change["path"], change["before"], "->", change["after"])
alice.call({"type": "approve", "proposal_id": review["id"],
            "digest": review["digest"], "reason": "Looks right"},
           idempotency_key="welcome-approve-1")

# The agent publishes the approved proposal as a new revision.
revision = agent.call({"type": "apply", "proposal_id": proposal["id"],
                       "digest": proposal["digest"]},
                      idempotency_key="welcome-apply-1")
print(revision["id"], revision["kind"])  # r..., proposal

print(alice.call({"type": "read", "path": "handbook/welcome.md"})["content"])
```

If the agent tried `{"type": "edit", ...}` instead, it would get
`DomainError` with code `human_required`. If it tried `apply` before Alice
approved, it would get `approval_required`.

## Next

- [concepts.md](concepts.md) explains tenants, actors and the approval flow.
- [permissions.md](permissions.md) adds ranks and hidden folders.
- [api.md](api.md) lists every operation.
- [python-client.md](python-client.md) covers errors, retries and imports.
