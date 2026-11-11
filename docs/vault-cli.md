# Vault CLI

Besides the governed content service, the `mnemosyne` binary works directly on a
plain folder of Markdown files (a vault). Its commands are modelled on and behave
like Obsidian's CLI commands: the same `command key=value` syntax, flags and output
formats.

The vault CLI edits files in place. It has no tenants, ranks, proposals or history;
use the content service for those.

## Usage

```sh
mnemosyne [vault=<name-or-path>] <command> [key=value ...] [flag ...]
```

- `vault=` takes a directory path or the name of a vault in the desktop app's vault
  registry. Without it, `MNEMOSYNE_VAULT` names the vault.
- Options are `key=value`; a bare word is a flag (`total`, `counts`, `verbose`).
- `file=<name>` resolves a note by name like a wikilink; `path=<path>` is the exact
  path from the vault root.
- Many listing commands take `format=json`, `tsv`, `csv`, `yaml`, `md` or `tree`
  (listed per command below).
- `--copy` also copies the output to the clipboard (macOS).
- `mnemosyne help` lists the commands; `mnemosyne help <command>` shows one.

```sh
mnemosyne vault=/path/to/notes search query="quarterly review" limit=5 format=json
mnemosyne vault=/path/to/notes property:set file=Plan name=status value=done
mnemosyne vault=/path/to/notes backlinks file=Warranty counts
```

## Files and folders

| Command | Does | Options |
| --- | --- | --- |
| `vault` | show vault info | `info=name\|path\|files\|folders\|size` |
| `vaults` | list registered vaults | `total`, `verbose` |
| `files` | list files | `folder=`, `ext=`, `total` |
| `folders` | list folders | `folder=`, `total` |
| `folder` | show folder info | `path=` (required), `info=files\|folders\|size` |
| `file` | show file info | `file=`, `path=` |
| `read` | print a file | `file=`, `path=` |
| `create` | create a file | `name=`, `path=`, `content=`, `template=`, `overwrite`, `open`, `newtab` |
| `append` | append to a file | `file=`, `path=`, `content=` (required), `inline` |
| `prepend` | prepend to a file | `file=`, `path=`, `content=` (required), `inline` |
| `rename` | rename a file | `file=`, `path=`, `name=` (required) |
| `move` | move or rename a file | `file=`, `path=`, `to=` (required) |
| `delete` | delete a file (to the system trash) | `file=`, `path=`, `permanent` |
| `wordcount` | count words and characters | `file=`, `path=`, `words`, `characters` |

## Search

| Command | Does | Options |
| --- | --- | --- |
| `search` | search the vault for text | `query=` (required), `path=`, `limit=`, `total`, `case`, `format=text\|json` |
| `search:context` | search with matching lines | `query=` (required), `path=`, `limit=`, `case`, `format=text\|json` |
| `index` | rebuild the search index | none |
| `watch` | keep the index updated as files change | none |

## Links

| Command | Does | Options |
| --- | --- | --- |
| `links` | outgoing links of a file | `file=`, `path=`, `total` |
| `backlinks` | links to a file | `file=`, `path=`, `counts`, `total`, `format=json\|tsv\|csv` |
| `unresolved` | links that point nowhere | `total`, `counts`, `verbose`, `format=json\|tsv\|csv` |
| `orphans` | files nothing links to | `total`, `all` |
| `deadends` | files that link to nothing | `total`, `all` |

## Metadata

| Command | Does | Options |
| --- | --- | --- |
| `properties` | list frontmatter properties | `file=`, `path=`, `name=`, `total`, `counts`, `sort=count`, `format=yaml\|json\|tsv`, `active` |
| `property:read` | read one property | `name=` (required), `file=`, `path=` |
| `property:set` | set one property | `name=`, `value=` (required), `type=text\|list\|number\|checkbox\|date\|datetime`, `file=`, `path=` |
| `property:remove` | remove one property | `name=` (required), `file=`, `path=` |
| `tags` | list tags | `file=`, `path=`, `total`, `counts`, `sort=count`, `format=json\|tsv\|csv`, `active` |
| `tag` | one tag's info | `name=` (required), `total`, `verbose` |
| `aliases` | list aliases | `file=`, `path=`, `total`, `verbose`, `active` |
| `outline` | headings of a file | `file=`, `path=`, `format=tree\|md\|json`, `total` |

## Tasks

| Command | Does | Options |
| --- | --- | --- |
| `tasks` | list Markdown tasks | `file=`, `path=`, `total`, `done`, `todo`, `status="<char>"`, `verbose`, `format=json\|tsv\|csv`, `active`, `daily` |
| `task` | show or update one task | `ref=<path:line>`, `file=`, `path=`, `line=`, `toggle`, `done`, `todo`, `status="<char>"`, `daily` |

## Bookmarks and workspace

| Command | Does | Options |
| --- | --- | --- |
| `bookmarks` | list bookmarks | `total`, `verbose`, `format=json\|tsv\|csv` |
| `bookmark` | add a bookmark | `file=`, `subpath=`, `folder=`, `search=`, `url=`, `title=` |
| `open` | open a file in the workspace | `file=`, `path=`, `newtab` |
| `recents` | recently opened files | `total` |
| `tabs` | open tabs | `ids` |
| `version` | print the compatible application version | none |

## Running as a service

`serve` keeps a vault open and answers commands over a Unix socket, which avoids
reopening the vault on every call:

```sh
mnemosyne vault=/path/to/notes serve socket=/private/run/vault.sock
MNEMOSYNE_SOCKET=/private/run/vault.sock mnemosyne search query=plan
```

When `MNEMOSYNE_SOCKET` is set, every command is relayed to that service. `Ctrl-C`
stops it.
