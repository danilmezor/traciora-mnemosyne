# Python client

`mnemosyne_client` talks to the local service over its Unix socket. It needs Python
3.10 or newer and nothing outside the standard library.

```sh
python3 -m pip install ./clients/python
```

Or, from a checkout, `PYTHONPATH=clients/python`.

## Calls

```python
from mnemosyne_client import Client, DomainError

client = Client("run/content.sock", token, timeout=120)

head = client.call({"type": "head"})
page = client.call({"type": "read", "path": "handbook/welcome.md"})
response = client.request({"type": "properties", "path": "handbook/welcome.md"})
print(response.version, response.result["properties"])
```

- `Client(socket_path, token, *, timeout=120.0, max_request_bytes=..., max_response_bytes=...)`.
  The token comes from the host's credential file. Store it like a password; it never
  appears in exception messages or reprs.
- `request(operation, *, idempotency_key=None, version=2, decode=None)` returns a
  `Response(version, result)`.
- `call(...)` takes the same arguments and returns only `result`.
- `decode` is an optional function that validates or converts the result.
- `operation` is any operation from [api.md](api.md), as a dict with a `type`.

Mutations need a key:

```python
proposal = client.call(
    {"type": "propose", "base_revision": head,
     "changes": [{"type": "put", "path": "notes/a.md", "content": "Hello\n"}]},
    idempotency_key="notes-a-1")
```

Each call opens one connection, sends one frame and reads one reply under a single
deadline. Concurrent calls use separate sockets. Frames are bounded at 128 MiB out
and 256 MiB in; you can lower both.

## Errors

| Exception | Meaning |
| --- | --- |
| `DomainError` | the service answered with an error; `code`, `message`, `version` |
| `TransportError` | I/O failed; `phase` and `may_have_been_sent` |
| `ProtocolError` | the reply was invalid or too large |
| `ClientError` | base class of all three |

```python
try:
    client.call({"type": "approve", "proposal_id": pid, "digest": digest},
                idempotency_key=key)
except DomainError as error:
    if error.code == "stale_base":
        ...  # reread and prepare a new proposal
```

## Retries

The client **never retries**, not even reads. To make a mutation safe to retry:

1. Save the exact operation and its idempotency key before sending.
2. If a `TransportError` or `ProtocolError` leaves the outcome unknown, resend the
   same operation with the same key. The service returns the original result and
   writes nothing new.
3. A different operation with an old key fails with `idempotency_conflict`.

`policy_set` does not use keys. After an unclear outcome, call `policy_get` and
compare before trying again.

Domain errors such as `stale_base`, `forbidden` or `replay_expired` need a decision
by your application. Never rebase onto a changed head silently.

## Import sessions

`ImportSession` imports a local directory in batches, durably. It copies the source
once into private hash-named files, keeps a cursor and receipts on disk, and can
resume after a crash without rereading the source.

```python
from mnemosyne_client import Client
from mnemosyne_client.import_session import ImportSession

client = Client("run/content.sock", human_token)
head = client.call({"type": "head"})

session = ImportSession.create(
    "/private/state/handbook.json",
    source="/data/handbook",
    base_revision=head,
    mode="edit",              # or "propose"
    batch_size=32,            # 1 to 32 entries per batch
    conflict_policy="error",  # or "skip", "overwrite"
    message="Imported the handbook")

while not session.complete:
    session.advance(client, expected_head=session.base_revision)
```

After a restart:

```python
session = ImportSession.open("/private/state/handbook.json")
while not session.complete:
    session.advance(client, expected_head=session.base_revision)
```

- `create(path, *, source, base_revision, mode="edit", batch_size=32,
  conflict_policy="error", message=None, origin=None)`. The state file's directory
  must be private (`0700`) and owned by you, and must sit outside the source tree.
  The captured copy lives next to it in `<name>.capture-<id>`.
- `advance(client, *, expected_head)` previews and commits one batch, atomically.
  `expected_head` must equal `session.base_revision`; if the head moved, it stops
  with an error instead of rebasing.
- An unfinished commit is replayed with its saved key before anything else.
- `message` and `origin` are sent with every commit: the revision's message in
  `edit` mode, the proposal's reason in `propose` mode.
- Properties: `complete`, `base_revision`, `file_paths`, `receipts`.

UTF-8 files become text; PDF, DOCX and other non-UTF-8 files become binaries. Each
file may be up to 16 MiB, each batch up to 32 entries and 16 MiB. There is no total
size limit. Symlinks and special files are rejected.

### Importing as an agent

With `mode="propose"`, each batch becomes a proposal and the session waits:

```python
proposal = session.advance(agent, expected_head=session.base_revision)
# A human finds it with list_proposals, reviews and approves it.
# The agent (or another proposer) applies it:
revision = agent.call({"type": "apply", "proposal_id": proposal["id"],
                       "digest": proposal["digest"]}, idempotency_key="import-apply-1")
session.acknowledge_applied(agent, expected_revision=revision["id"])
```

`acknowledge_applied` checks that the proposal is applied and is the current head
before the session moves to the next batch. The session never approves, applies or
rebases anything itself.

When the import is done, delete the state file and its capture directory. They hold
copies of the source bytes. Drive one session from one process at a time.
