# Search, documents and the link graph

Everything here reads the published head of the caller's tenant and filters by the
caller's permissions before counting, ranking or building snippets. Pending and
approved-but-unapplied proposals are never searchable. A hidden file never changes
a visible score or count.

## Full-text search

```json
{"version":2,"operation":{"type":"search","query":"поиск engines","scope":"handbook",
 "extensions":["md","pdf"],"limit":20}}
```

- The query is plain words. Every word must match. There is no boolean, phrase or
  wildcard syntax.
- Russian and English are stemmed (Snowball), text is lowercased, and `ё` matches
  `е`. Mixed-language text and queries work.
- `scope` limits results to a folder (default `""`, the whole tenant).
- `extensions` filters by file suffix, without the dot, case-insensitive.
- `fields` is `["body","name"]` by default. With `name`, a word in the file name
  also matches, with a fixed boost.
- `typos` (default `true`) forgives one edit in words of 5 or more characters.
  Exact matches always rank above fuzzy ones; fuzzy hits carry `fuzzy: true`.
- `limit` is 1–100 (default 20).

The result:

```json
{"revision_id":"r…","snapshot_id":"…","total":42,"next_offset":20,
 "index":{"revision_id":"r…","status":"current"},
 "hits":[{"file_id":"f…","path":"handbook/returns.md","revision_id":"r…",
          "score":3.1,"snippet":"…","fuzzy":false,
          "section":{"section_id":"s2","title":"Returns"}}]}
```

- `total` counts every visible match before paging.
- For the next page send the same request with `snapshot_id` and `offset` set to
  `next_offset`. If anything changed (head, policy, query), the answer is
  `stale_search`; start again at offset 0. `next_offset: null` is the last page.
- `section` names the Markdown heading the hit falls under.
- Snippets are plain text. Show them as text, never as HTML.
- `index.status` is `current`, `incremental` or `rebuilt`. The index is a cache and
  rebuilds itself; if it cannot, the answer is `search_unavailable`, never stale hits.

Query limits: at most 4096 bytes, each word at most 80 bytes (`invalid_search`).

## Find by name

`find` is the quick-open bar: files and folders whose name or path words start with
the query words.

```json
{"version":2,"operation":{"type":"find","query":"warr","limit":10}}
```

```json
{"revision_id":"r…","hits":[{"path":"notes/Warranty.md","file_id":"f…","kind":"text",
  "title":"Warranty","colour":"cyan","seen_from":"junior","matched":"name",
  "changed_at":"2026-01-01T12:00:00.000Z","fuzzy":false}]}
```

Name matches rank before path matches, then the most recently changed. The extension
counts as a name word, so `pdf` finds every PDF. `limit` is 1–50.

## Document extraction

PDF, DOCX and PPTX files stored with `put_binary` become searchable text. Extraction
runs locally in a bounded subprocess; no network, office suite or model is needed.

| Format | Location of each segment |
| --- | --- |
| PDF | `{"page": N}` |
| DOCX | `{"paragraph": N, "section": "…"}` (paragraphs inside tables included) |
| PPTX | `{"slide": N, "section": "<slide title or empty>"}` |

Read the text with `extracted_text`:

```json
{"version":2,"operation":{"type":"extracted_text","path":"contracts/lease.pdf"}}
```

```json
{"revision_id":"r…","path":"contracts/lease.pdf","file_id":"f…","source_blob":"…",
 "segment_count":12,"extraction":{"source_blob":"…","extractor":"…","version":"…",
 "status":"succeeded","segments":[{"text":"…","location":{"page":1}}],"limits":[]}}
```

`extraction_status` returns the same record without segments and never runs the
extractor. `retry_extraction` runs it again (needs `edit`).

### Statuses

| Status | Meaning |
| --- | --- |
| `pending` | not extracted yet |
| `succeeded` | all text extracted |
| `partial` | useful text, with omissions listed in `limits` |
| `unsupported` | format or content cannot be extracted; reason in `limits` |
| `failed` | the file could not be parsed |

Only `succeeded` and `partial` text is searchable. A search hit on a document adds
`source: {blob, extractor, version, segment, location}`.

There is **no OCR**. Image-only PDF pages and image-only slides are listed in
`limits` and make the record `partial`; a document with no text at all (a scan) is
`unsupported` with `image_only_or_empty_ocr_deferred`. Images (`png`, `jpg`, `jpeg`, `gif`,
`webp`, `svg`) answer `unsupported` with `limits: ["image"]` at once; their bytes
stay readable and findable by name. PPTX speaker notes, charts, SmartArt and
embedded objects are omitted and say so (`notes_omitted`,
`embedded_content_omitted`).

### Limits

- Input 16 MiB; PDF up to 1,000 pages.
- DOCX and PPTX: 2,048 ZIP entries, 32 MiB inflated in total, 8 MiB per XML part,
  128 nesting levels. DOCTYPE and non-UTF-8 XML are `unsupported`.
- Output 2 MiB of text and 10,000 segments.
- 10 seconds wall time and 8 seconds CPU per file; on Linux also 512 MiB of
  address space.

Text extracted from documents does not carry links into the graph.

## Semantic and hybrid search (optional)

Keyword search needs nothing extra. Semantic and hybrid search use a local embedding
model and never call a remote service.

```json
{"version":2,"operation":{"type":"search","query":"how to recover lost data",
 "mode":"hybrid","min_score":0.0}}
```

- `mode` is `keyword` (default), `semantic` or `hybrid`.
- `semantic` ranks by cosine similarity. `min_score` is a cosine floor in
  [-1, 1] (default 0); keyword mode rejects it.
- `hybrid` fuses keyword and semantic ranks by reciprocal rank fusion
  (`1/(60+rank)`).
- Hits add `ranking` (method, keyword and semantic rank and score) and, for semantic
  matches, `chunk: {byte_start, byte_end, model_fingerprint, dimensions}`. The
  result adds `retrieval: {mode, min_score, model_fingerprint, dimensions}`.
- Semantic retrieval matches the body only; `typos` and `fields` apply to the
  keyword half.

Without a model, or if the model fails, semantic and hybrid answer
`semantic_unavailable`. Keyword search keeps working.

### Installing a model

The supported setup is the `multilingual-e5-small` ONNX model with ONNX Runtime
1.22.0 on CPU. The installer downloads the pinned files, checks their SHA-256
hashes and writes a `config.json`. It is the only step that uses the network. It
supports macOS arm64; on other platforms supply a compatible runtime library
yourself.

```sh
python3 tools/install_semantic_model.py --destination /private/models \
  --accept-download-and-licenses
```

Point the service at the config before starting it:

```sh
export MNEMOSYNE_EMBEDDING_CONFIG=/private/models/config.json
mnemosyne content-serve --store ./store --credentials credentials.json --socket run/content.sock
```

`config.json` fields:

| Field | Default | Range |
| --- | --- | --- |
| `model_dir` | required | holds `model.onnx` and `tokenizer.json` |
| `runtime_library` | required | path to the ONNX Runtime library |
| `worker` | the running binary | override also via `MNEMOSYNE_EMBEDDING_WORKER` |
| `timeout_ms` | 30000 | 100–300000 |
| `threads` | 2 | 1–16 |
| `batch_size` | 8 | 1–32 |
| `chunk_chars` | 1000 | 64–1500 |

Vectors are cached per model fingerprint, so switching models never mixes vector
spaces. Each query starts a fresh worker process; expect about a second or more per
semantic query and several hundred MB of memory while it runs.

## Link graph

Links come from Markdown links, images and `[[wikilinks]]` (`[[target]]`,
`[[target|alias]]`, `[[target#heading]]`). Relative paths resolve from the linking
file's folder. A wikilink resolves by file name and only when exactly one file has
that name; otherwise it is unresolved and lists its `candidates`. Links inside code
blocks, external URLs and texts over 2 MiB are ignored.

### graph

```json
{"version":2,"operation":{"type":"graph","scope":"","limit":500}}
```

Returns `nodes` (path, file_id, title, kind, `seen_from`, colour, `in_links`,
`out_links`, `members`), `edges` (`links` between files, `contains` from a folder
to its children), `unresolved` and `next_offset`. Without `limit` you get the whole
scope; with it, pages of 1–2000 nodes in path order. If `revision_id` changes
between pages, start again.

### links

```json
{"path":"index.md","revision_id":"r…","out":["notes/Warranty.md"],"in":["plan.md"],
 "unresolved":[],"above_rank":[{"target":"Budget","seen_from":"senior"}],
 "titles":{"index.md":"Home","notes/Warranty.md":"Warranty","plan.md":"Plan"},
 "anchors":[{"target":"notes/Warranty.md","heading":"Terms","section_id":"s2"}]}
```

`above_rank` names links you can see in the text whose target you may not open, and
the rank that can. `anchors` resolves `[[target#heading]]` to a section you can pass
to `read_section`.

### neighbourhood

The visible subgraph around a file or folder in one call, for an agent that wants
context:

```json
{"version":2,"operation":{"type":"neighbourhood","path":"index.md","depth":2,"limit":32}}
```

Nodes carry `title`, `snippet` (the first paragraph, up to 240 characters) and
`hop`. From a file the walk follows links both ways; from a folder it takes its
members, then their links. Hidden files are never returned or walked through.
`truncated: true` means `limit` cut the result.

Titles come from frontmatter `title`, else the first level-1 heading, else the file
name.
