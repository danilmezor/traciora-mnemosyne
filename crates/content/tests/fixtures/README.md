# Schema 1 migration fixture

`schema1-authority.json` is a content store written by the first schema with the
actual `content-init` and API-v1 `edit`, `propose` and `approve` operations. It
holds a human-created text file and an approved, unapplied agent proposal, with
durable operation replays and the approval audit. All content, identities and
tokens are synthetic. Never regenerate it with a newer binary: it must keep
representing the old schema.
