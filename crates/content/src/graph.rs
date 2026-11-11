//! Derived link graph of a tenant's head revision. A private cache under `graph-v1/`,
//! reconciled under the authority lock and never authoritative. It holds no
//! authorization decision: callers filter nodes, edges and candidates per request.
use crate::auth::{Permission, TenantPolicy, TrustedIdentity};
use crate::links::{extract_links, resolve_with, BasenameIndex, Link, Resolution, GRAMMAR_VERSION};
use crate::outline_text;
use crate::ranks::ColourSource;
use crate::{err, storage, Entry, EntryKind, Result, Service, State};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

/// 2: nodes carry `title` and `snippet`, texts their heading titles.
const SCHEMA: u32 = 2;
/// Texts above the extraction bound have no links; their blobs are not read.
const MAX_TEXT_BYTES: u64 = 2 * 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EdgeKind {
    Contains,
    Links,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IndexStatus {
    Current,
    Incremental,
    Rebuilt,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct GraphIndex {
    pub schema: u32,
    /// `links::GRAMMAR_VERSION` the cached `files` links were extracted with.
    pub grammar: u32,
    pub revision_id: String,
    pub nodes: Vec<IndexNode>,
    pub edges: Vec<IndexEdge>,
    pub unresolved: Vec<IndexUnresolved>,
    /// Extracted links of every text file by path, with the blob they were parsed
    /// from. A blob is content-addressed, so its links are reused wherever it recurs.
    pub files: BTreeMap<String, IndexFile>,
    /// The distinct case-folded words of every node's path (so of every basename),
    /// for `find`'s one forgiven typo. An index stored without it is
    /// rebuilt.
    pub words: crate::fuzzy::TrigramIndex,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct IndexNode {
    pub path: String,
    /// Empty for an implicit parent folder that has no entry of its own.
    pub file_id: String,
    pub kind: EntryKind,
    pub out_links: u32,
    pub in_links: u32,
    /// Folders: direct children.
    pub members: u32,
    /// the text's own title, else the basename without its extension.
    pub title: String,
    /// The first paragraph as plain text; `""` for folders, binaries and texts over
    /// the bound.
    pub snippet: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct IndexEdge {
    pub from: String,
    pub to: String,
    pub kind: EdgeKind,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct IndexUnresolved {
    pub from: String,
    pub target: String,
    pub candidates: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct IndexFile {
    pub blob: String,
    pub links: Vec<Link>,
    /// Heading titles in outline order: heading `i` is section `s{i+1}`.
    pub headings: Vec<String>,
    /// The title the text gives itself (frontmatter or first H1), kept apart from the
    /// path so a blob that recurs under another name keeps it and the basename
    /// fallback follows the path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    pub snippet: String,
}

/// Parsed indexes kept per tenant (final review I1): `find` asks for the index on
/// every keystroke, and parsing the whole file each time cost tens of milliseconds on a
/// large tenant. An entry is trusted only while the file's metadata (length,
/// modification time, inode) is the one it was read with, and a reconcile that
/// rewrites the file replaces it. Like the file it holds no authorization decision.
pub(crate) struct GraphCache {
    entries: Mutex<HashMap<String, Cached>>,
    parses: AtomicU64,
}

struct Cached {
    stamp: Stamp,
    index: Arc<GraphIndex>,
    used: Instant,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Stamp {
    len: u64,
    modified: Option<SystemTime>,
    inode: u64,
}

impl Stamp {
    fn of(meta: &fs::Metadata) -> Self {
        #[cfg(unix)]
        let inode = std::os::unix::fs::MetadataExt::ino(meta);
        #[cfg(not(unix))]
        let inode = 0;
        Self {
            len: meta.len(),
            modified: meta.modified().ok(),
            inode,
        }
    }
}

/// At most this many tenants are kept, and none unused for longer than `CACHE_IDLE`.
const CACHE_TENANTS: usize = 16;
const CACHE_IDLE: Duration = Duration::from_secs(10 * 60);

impl GraphCache {
    pub(crate) fn new() -> Self {
        Self {
            entries: Mutex::new(HashMap::new()),
            parses: AtomicU64::new(0),
        }
    }

    fn entries(&self) -> std::sync::MutexGuard<'_, HashMap<String, Cached>> {
        self.entries.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// The stored index of `tenant_id`, parsed only when the file's metadata differs
    /// from the cached entry's. A missing, unreadable, corrupt, other-schema or
    /// other-grammar file is absent. The stamp is taken from the open handle, so it
    /// always describes the bytes read: the file is replaced only by rename.
    fn load(&self, root: &Path, tenant_id: &str) -> Option<Arc<GraphIndex>> {
        let file = index_file(root, tenant_id);
        let current = fs::metadata(&file).ok().map(|m| Stamp::of(&m));
        {
            let mut entries = self.entries();
            match entries.get_mut(tenant_id) {
                Some(c) if Some(c.stamp) == current => {
                    c.used = Instant::now();
                    return Some(c.index.clone());
                }
                Some(_) => {
                    entries.remove(tenant_id);
                }
                None => {}
            }
        }
        let mut handle = File::open(&file).ok()?;
        let stamp = Stamp::of(&handle.metadata().ok()?);
        let mut bytes = Vec::with_capacity(stamp.len as usize);
        std::io::Read::read_to_end(&mut handle, &mut bytes).ok()?;
        drop(handle);
        self.parses.fetch_add(1, Ordering::Relaxed);
        let index = serde_json::from_slice::<GraphIndex>(&bytes)
            .ok()
            .filter(|i| i.schema == SCHEMA && i.grammar == GRAMMAR_VERSION)?;
        let index = Arc::new(index);
        self.insert(tenant_id, stamp, index.clone());
        Some(index)
    }

    /// Records `index` as what the file now holds, right after this process wrote it.
    fn store(&self, root: &Path, tenant_id: &str, index: Arc<GraphIndex>) {
        match fs::metadata(index_file(root, tenant_id)) {
            Ok(meta) => self.insert(tenant_id, Stamp::of(&meta), index),
            Err(_) => {
                self.entries().remove(tenant_id);
            }
        }
    }

    fn insert(&self, tenant_id: &str, stamp: Stamp, index: Arc<GraphIndex>) {
        let now = Instant::now();
        let mut entries = self.entries();
        entries.retain(|_, c| now.duration_since(c.used) < CACHE_IDLE);
        entries.insert(
            tenant_id.to_owned(),
            Cached {
                stamp,
                index,
                used: now,
            },
        );
        while entries.len() > CACHE_TENANTS {
            let oldest = entries
                .iter()
                .min_by_key(|(_, c)| c.used)
                .map(|(t, _)| t.clone())
                .expect("over the cap, so not empty");
            entries.remove(&oldest);
        }
    }
}

fn index_file(root: &Path, tenant_id: &str) -> PathBuf {
    root.join("graph-v1")
        .join(format!("{:x}.json", Sha256::digest(tenant_id.as_bytes())))
}

fn unavailable() -> crate::ContentError {
    err("graph_unavailable")
}

/// The tenant's graph at head: `current` when the stored index already describes head,
/// `incremental` when only texts whose blob changed were parsed again, `rebuilt` when
/// no usable index was stored. The caller holds the authority lock; the stored index
/// comes from `cache`, parsed only when the file changed since it was last read.
pub(crate) fn reconcile(
    root: &Path,
    cache: &GraphCache,
    s: &State,
    tenant_id: &str,
) -> Result<(Arc<GraphIndex>, IndexStatus)> {
    let revision = s
        .tenants
        .get(tenant_id)
        .map(|t| t.head.as_str())
        .filter(|h| !h.is_empty())
        .unwrap_or("r0");
    let files = crate::ranks::head_files(s, tenant_id);
    let mut texts = BTreeMap::new();
    for (path, entry) in files.iter() {
        if entry.kind == EntryKind::Text {
            let blob = entry.blob.as_deref().ok_or_else(unavailable)?;
            texts.insert(path.as_str(), blob);
        }
    }
    let file = index_file(root, tenant_id);
    let stored = cache.load(root, tenant_id);
    if let Some(index) = &stored {
        let same_texts = index.files.len() == texts.len()
            && index
                .files
                .iter()
                .all(|(p, f)| texts.get(p.as_str()) == Some(&f.blob.as_str()));
        if index.revision_id == revision && same_texts {
            return Ok((stored.unwrap(), IndexStatus::Current));
        }
    }
    let stored_present = stored.is_some();
    // Everything derived from a text depends on its blob alone, so a blob already
    // parsed (here or under another path) is not read again.
    let mut known: HashMap<String, IndexFile> = stored
        .iter()
        .flat_map(|i| i.files.values())
        .map(|f| (f.blob.clone(), f.clone()))
        .collect();
    let mut parsed = BTreeMap::new();
    for (path, blob) in texts {
        let file = match known.get(blob) {
            Some(file) => file.clone(),
            None => {
                let file = parse(root, blob)?;
                known.insert(blob.to_string(), file.clone());
                file
            }
        };
        parsed.insert(path.to_string(), file);
    }
    let index = Arc::new(build(revision, &files, parsed));
    save(&file, &index).map_err(|_| unavailable())?;
    cache.store(root, tenant_id, index.clone());
    let status = if stored_present {
        IndexStatus::Incremental
    } else {
        IndexStatus::Rebuilt
    };
    Ok((index, status))
}

/// Links, heading titles, own title and snippet of one text blob. A text over the
/// bound is not read: it has none of them.
fn parse(root: &Path, blob: &str) -> Result<IndexFile> {
    let mut file = IndexFile {
        blob: blob.to_string(),
        links: vec![],
        headings: vec![],
        title: None,
        snippet: String::new(),
    };
    if storage::blob_len(root, blob).map_err(|_| unavailable())? > MAX_TEXT_BYTES {
        return Ok(file);
    }
    let bytes = storage::read_blob(root, blob).map_err(|_| unavailable())?;
    let text = String::from_utf8(bytes).map_err(|_| unavailable())?;
    file.links = extract_links(&text);
    file.headings = outline_text::heading_titles(&text);
    file.title = outline_text::content_title(&text);
    file.snippet = outline_text::snippet_of(&text);
    Ok(file)
}

fn build(
    revision: &str,
    files: &BTreeMap<String, Entry>,
    parsed: BTreeMap<String, IndexFile>,
) -> GraphIndex {
    let mut nodes: BTreeMap<&str, IndexNode> = BTreeMap::new();
    let node = |path: &str, file_id: &str, kind: EntryKind| {
        let file = parsed.get(path);
        IndexNode {
            path: path.into(),
            file_id: file_id.into(),
            kind,
            out_links: 0,
            in_links: 0,
            members: 0,
            title: file
                .and_then(|f| f.title.clone())
                .unwrap_or_else(|| outline_text::basename_title(path)),
            snippet: file.map(|f| f.snippet.clone()).unwrap_or_default(),
        }
    };
    for (path, entry) in files {
        nodes.insert(path, node(path, &entry.file_id, entry.kind.clone()));
    }
    for path in files.keys() {
        for (i, _) in path.match_indices('/') {
            let parent = &path[..i];
            nodes
                .entry(parent)
                .or_insert_with(|| node(parent, "", EntryKind::Folder));
        }
    }
    let mut edges = vec![];
    let children: Vec<(&str, &str)> = nodes
        .keys()
        .filter_map(|p| p.rsplit_once('/').map(|(parent, _)| (parent, *p)))
        .collect();
    for (parent, child) in children {
        nodes.get_mut(parent).unwrap().members += 1;
        edges.push(IndexEdge {
            from: parent.into(),
            to: child.into(),
            kind: EdgeKind::Contains,
        });
    }
    let by_basename = BasenameIndex::new(files);
    let mut unresolved = vec![];
    for (from, file) in &parsed {
        let mut targets = BTreeSet::new();
        let mut missing = BTreeSet::new();
        for link in &file.links {
            match resolve_with(from, link, files, &by_basename) {
                Resolution::Resolved { path } => {
                    if targets.insert(path.clone()) {
                        edges.push(IndexEdge {
                            from: from.clone(),
                            to: path,
                            kind: EdgeKind::Links,
                        });
                    }
                }
                Resolution::Unresolved { target, candidates } => {
                    if missing.insert(target.clone()) {
                        unresolved.push(IndexUnresolved {
                            from: from.clone(),
                            target,
                            candidates,
                        });
                    }
                }
            }
        }
    }
    for edge in edges.iter().filter(|e| e.kind == EdgeKind::Links) {
        nodes.get_mut(edge.from.as_str()).unwrap().out_links += 1;
        nodes.get_mut(edge.to.as_str()).unwrap().in_links += 1;
    }
    edges.sort_by(|a, b| (&a.from, &a.to, a.kind).cmp(&(&b.from, &b.to, b.kind)));
    let words = crate::fuzzy::TrigramIndex::build(path_words(nodes.keys().copied()));
    GraphIndex {
        schema: SCHEMA,
        grammar: GRAMMAR_VERSION,
        revision_id: revision.into(),
        nodes: nodes.into_values().collect(),
        edges,
        unresolved,
        files: parsed,
        words,
    }
}

/// The distinct case-folded words of `paths`.
pub(crate) fn path_words<'a>(paths: impl IntoIterator<Item = &'a str>) -> Vec<String> {
    paths
        .into_iter()
        .flat_map(crate::find::words)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

/// Temp file + rename; the authority lock serializes every writer.
fn save(file: &Path, index: &GraphIndex) -> std::io::Result<()> {
    let dir = file.parent().expect("graph cache path has a parent");
    fs::create_dir_all(dir)?;
    let name = file.file_name().expect("graph cache path has a name");
    let temp = dir.join(format!(".{}.next", name.to_string_lossy()));
    let mut out = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(&temp)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        out.set_permissions(fs::Permissions::from_mode(0o600))?;
    }
    out.write_all(&serde_json::to_vec(index).map_err(std::io::Error::other)?)?;
    out.sync_all()?;
    fs::rename(&temp, file)?;
    File::open(dir).and_then(|d| d.sync_all())
}

/// A node the caller may read, with the index's full degrees: a reader learns how many
/// links a file has even when some of them lead to files it cannot see.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct GraphNode {
    pub path: String,
    /// Empty for an implicit parent folder that has no entry of its own.
    pub file_id: String,
    pub kind: EntryKind,
    pub title: String,
    pub seen_from: String,
    pub colour: String,
    pub colour_source: ColourSource,
    pub in_links: u32,
    pub out_links: u32,
    pub members: u32,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct GraphEdge {
    pub from: String,
    pub to: String,
    pub kind: EdgeKind,
}

/// A link that names no file; `candidates` are only the namesakes the caller may read,
/// and the link stays unresolved even when a single one remains.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct GraphUnresolved {
    pub from: String,
    pub target: String,
    pub candidates: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct IndexInfo {
    pub revision_id: String,
    pub status: IndexStatus,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct GraphResult {
    pub revision_id: String,
    pub index: IndexInfo,
    pub nodes: Vec<GraphNode>,
    pub edges: Vec<GraphEdge>,
    pub unresolved: Vec<GraphUnresolved>,
    /// The offset of the next page; `null` without `limit` and on the last page.
    #[serde(default)]
    pub next_offset: Option<usize>,
}

/// A node of a neighbourhood: what the caller may read, with its distance from the
/// centre (the centre is hop 0).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct NeighbourNode {
    pub path: String,
    /// Empty for an implicit parent folder that has no entry of its own.
    pub file_id: String,
    pub title: String,
    pub kind: EntryKind,
    pub seen_from: String,
    pub colour: String,
    pub colour_source: ColourSource,
    pub snippet: String,
    pub hop: u8,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct NeighbourhoodResult {
    pub revision_id: String,
    pub index: IndexInfo,
    pub centre: String,
    pub nodes: Vec<NeighbourNode>,
    pub edges: Vec<GraphEdge>,
    /// The centre's unresolved links and above-rank targets, exactly as `links`.
    pub unresolved: Vec<GraphUnresolved>,
    pub above_rank: Vec<AboveRank>,
    pub truncated: bool,
}

/// Bounds of `graph` paging and `neighbourhood`.
const MAX_GRAPH_PAGE: usize = 2000;
const MAX_NEIGHBOURS: usize = 64;

fn invalid_request(message: &str) -> crate::ContentError {
    crate::ContentError::new("invalid_request", message)
}

/// A link target the caller may not read, named because its name is already in the
/// text of a file the caller reads.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AboveRank {
    pub target: String,
    pub seen_from: String,
}

/// A wikilink of the file to a heading of a readable target: `heading` as written,
/// `section_id` the target's first heading with exactly that title, else the first
/// equal to it case-folded, else `null`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Anchor {
    pub target: String,
    pub heading: String,
    pub section_id: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LinksResult {
    pub path: String,
    pub revision_id: String,
    pub out: Vec<String>,
    pub r#in: Vec<String>,
    pub unresolved: Vec<GraphUnresolved>,
    pub above_rank: Vec<AboveRank>,
    /// The title of the file itself and of every path in `out` and `in`.
    pub titles: BTreeMap<String, String>,
    pub anchors: Vec<Anchor>,
}

/// `section_id` of `heading` among a text's heading titles.
fn section_of(headings: &[String], heading: &str) -> Option<String> {
    let folded = heading.to_lowercase();
    headings
        .iter()
        .position(|h| h == heading)
        .or_else(|| headings.iter().position(|h| h.to_lowercase() == folded))
        .map(|i| format!("s{}", i + 1))
}

/// The index nodes `who` may read at head. A node with an entry needs the rank check
/// and every name its identity has held (as `read` and `list` do). An implicit parent
/// folder has no meta of its own, so it needs the rank check and at least one such
/// readable descendant: otherwise its name exists only through hidden files.
fn visible<'a>(s: &State, who: &TrustedIdentity, index: &'a GraphIndex) -> HashSet<&'a str> {
    visible_among(s, who, index.nodes.iter().map(|n| n.path.as_str()))
}

/// `visible` over any head paths: those with an entry, plus the implicit parent folders
/// of the readable ones (each a prefix of a returned path).
pub(crate) fn visible_among<'a>(
    s: &State,
    who: &TrustedIdentity,
    paths: impl IntoIterator<Item = &'a str>,
) -> HashSet<&'a str> {
    let files = crate::ranks::head_files(s, &who.tenant_id);
    let readable = |p: &str| crate::ranks::check_at(s, who, Permission::Read, p, &files).is_ok();
    let mut out: HashSet<&str> = paths
        .into_iter()
        .filter(|p| {
            files
                .get(*p)
                .is_some_and(|e| readable(p) && crate::authorize_identity(s, who, e, p).is_ok())
        })
        .collect();
    let mut implicit = HashSet::new();
    for path in &out {
        for (i, _) in path.match_indices('/') {
            let parent = &path[..i];
            if !files.contains_key(parent) {
                implicit.insert(parent);
            }
        }
    }
    out.extend(implicit.into_iter().filter(|p| readable(p)));
    out
}

/// The link targets of `path` hidden from `who` by rank, named by the link text as
/// written in the file: a resolved path (a basename wikilink's folder) is not in the
/// text. A target hidden only by its identity's earlier names is left out, since its
/// seen_from would not say who can see it. Agents get none unless the tenant allows it
/// for a delegated agent, which then gets its human's list.
fn above_rank(
    s: &State,
    who: &TrustedIdentity,
    index: &GraphIndex,
    files: &BTreeMap<String, Entry>,
    path: &str,
) -> Vec<AboveRank> {
    let (Some(viewer), Some(file)) = (
        crate::ranks::above_rank_viewer(s, who),
        index.files.get(path),
    ) else {
        return vec![];
    };
    let tp = crate::ranks::tenant_policy(s, &who.tenant_id);
    let by_basename = BasenameIndex::new(files);
    let mut above: BTreeMap<String, String> = BTreeMap::new();
    for link in &file.links {
        if let Resolution::Resolved { path: to } = resolve_with(path, link, files, &by_basename) {
            if crate::ranks::check_at(s, &viewer, Permission::Read, &to, files).is_err() {
                above
                    .entry(link.target.clone())
                    .or_insert_with(|| crate::ranks::effective_seen_from(files, &tp, &to));
            }
        }
    }
    above
        .into_iter()
        .map(|(target, seen_from)| AboveRank { target, seen_from })
        .collect()
}

fn unresolved_for(u: &IndexUnresolved, readable: &HashSet<&str>) -> GraphUnresolved {
    GraphUnresolved {
        from: u.from.clone(),
        target: u.target.clone(),
        candidates: u
            .candidates
            .iter()
            .filter(|c| readable.contains(c.as_str()))
            .cloned()
            .collect(),
    }
}

/// `path` is `scope` or below it; the empty scope is the whole tenant.
pub(crate) fn within(path: &str, scope: &str) -> bool {
    scope.is_empty()
        || path
            .strip_prefix(scope)
            .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
}

impl Service {
    /// The link graph at head, limited to `scope` (a path and its descendants; `""` is
    /// the whole tenant) and to what the caller may read. A scope that is absent or
    /// hidden answers `not_found` alike.
    ///
    /// With `limit` (1–2,000) the answer is one page: the nodes `offset .. offset+limit`
    /// in path byte order, the edges whose `from` is on the page (and whose `to` is
    /// shown) and the unresolved links whose `from` is on the page, so the pages of one
    /// revision hold each exactly once. Without it, the whole scope.
    pub fn graph(
        &self,
        who: &TrustedIdentity,
        scope: &str,
        offset: usize,
        limit: Option<usize>,
    ) -> Result<GraphResult> {
        if limit.is_some_and(|l| !(1..=MAX_GRAPH_PAGE).contains(&l)) {
            return Err(invalid_request("limit must be between 1 and 2000"));
        }
        if !scope.is_empty() {
            crate::valid_path(scope)?;
        }
        self.warm_graph_index(&who.tenant_id);
        self.transaction_for(who, false, vec![], false, None, None, |s| {
            // Gives a non-member its grant answer (`forbidden`); `visible` below decides
            // for members, including implicit folders and earlier names.
            crate::ranks::check(s, who, Permission::Read, scope)?;
            let (index, status) = reconcile(&self.root, &self.graph_cache, s, &who.tenant_id)?;
            let readable = visible(s, who, &index);
            if !scope.is_empty() && !readable.contains(scope) {
                return Err(err("not_found"));
            }
            let shown: HashSet<&str> = readable
                .iter()
                .copied()
                .filter(|p| within(p, scope))
                .collect();
            let files = crate::ranks::head_files(s, &who.tenant_id);
            let tp = crate::ranks::tenant_policy(s, &who.tenant_id);
            let mut defaults: HashMap<&str, (String, ColourSource)> = HashMap::new();
            // The index keeps its nodes in path byte order.
            let mut listed: Vec<&IndexNode> = index
                .nodes
                .iter()
                .filter(|n| shown.contains(n.path.as_str()))
                .collect();
            let mut next_offset = None;
            if let Some(limit) = limit {
                let total = listed.len();
                let end = offset.saturating_add(limit).min(total);
                listed = listed
                    .get(offset..end)
                    .map(<[_]>::to_vec)
                    .unwrap_or_default();
                next_offset = (end < total).then_some(end);
            }
            let on_page: HashSet<&str> = listed.iter().map(|n| n.path.as_str()).collect();
            let nodes = listed
                .into_iter()
                .map(|n| {
                    let (colour, colour_source) =
                        colour(&files, &tp, &who.tenant_id, &n.path, &mut defaults);
                    GraphNode {
                        path: n.path.clone(),
                        file_id: n.file_id.clone(),
                        kind: n.kind.clone(),
                        title: n.title.clone(),
                        seen_from: crate::ranks::effective_seen_from(&files, &tp, &n.path),
                        colour,
                        colour_source,
                        in_links: n.in_links,
                        out_links: n.out_links,
                        members: n.members,
                    }
                })
                .collect();
            let edges = index
                .edges
                .iter()
                .filter(|e| on_page.contains(e.from.as_str()) && shown.contains(e.to.as_str()))
                .map(|e| GraphEdge {
                    from: e.from.clone(),
                    to: e.to.clone(),
                    kind: e.kind,
                })
                .collect();
            let unresolved = index
                .unresolved
                .iter()
                .filter(|u| on_page.contains(u.from.as_str()))
                .map(|u| unresolved_for(u, &readable))
                .collect();
            Ok(GraphResult {
                revision_id: index.revision_id.clone(),
                index: IndexInfo {
                    revision_id: index.revision_id.clone(),
                    status,
                },
                nodes,
                edges,
                unresolved,
                next_offset,
            })
        })
    }

    /// The links of one readable path at head: readable targets and sources, targets
    /// hidden by rank under `above_rank` with their `seen_from`; sources it may not read,
    /// and targets hidden only by their identity's earlier names, are left out.
    pub fn links(&self, who: &TrustedIdentity, path: &str) -> Result<LinksResult> {
        crate::valid_path(path)?;
        self.warm_graph_index(&who.tenant_id);
        self.transaction_for(who, false, vec![], false, None, None, |s| {
            // Gives a non-member its grant answer (`forbidden`); `visible` below decides
            // for members, including implicit folders and earlier names.
            crate::ranks::check(s, who, Permission::Read, path)?;
            let (index, _) = reconcile(&self.root, &self.graph_cache, s, &who.tenant_id)?;
            let readable = visible(s, who, &index);
            if !readable.contains(path) {
                return Err(err("not_found"));
            }
            let files = crate::ranks::head_files(s, &who.tenant_id);
            let (mut out, mut sources) = (vec![], vec![]);
            for e in index.edges.iter().filter(|e| e.kind == EdgeKind::Links) {
                if e.from == path && readable.contains(e.to.as_str()) {
                    out.push(e.to.clone());
                }
                if e.to == path && readable.contains(e.from.as_str()) {
                    sources.push(e.from.clone());
                }
            }
            sources.sort();
            let mut anchors: Vec<Anchor> = vec![];
            let mut anchored: HashSet<(String, String)> = HashSet::new();
            if let Some(file) = index.files.get(path) {
                let by_basename = BasenameIndex::new(&files);
                for link in &file.links {
                    if let (Some(heading), Resolution::Resolved { path: to }) = (
                        &link.heading,
                        resolve_with(path, link, &files, &by_basename),
                    ) {
                        if readable.contains(to.as_str())
                            && anchored.insert((to.clone(), heading.clone()))
                        {
                            let headings =
                                index.files.get(&to).map_or(&[][..], |f| &f.headings[..]);
                            anchors.push(Anchor {
                                section_id: section_of(headings, heading),
                                target: to.clone(),
                                heading: heading.clone(),
                            });
                        }
                    }
                }
            }
            let above_rank = above_rank(s, who, &index, &files, path);
            let title_of: HashMap<&str, &str> = index
                .nodes
                .iter()
                .map(|n| (n.path.as_str(), n.title.as_str()))
                .collect();
            let titles = std::iter::once(path)
                .chain(out.iter().map(String::as_str))
                .chain(sources.iter().map(String::as_str))
                .filter_map(|p| title_of.get(p).map(|t| (p.to_string(), t.to_string())))
                .collect();
            Ok(LinksResult {
                path: path.into(),
                revision_id: index.revision_id.clone(),
                out,
                r#in: sources,
                unresolved: index
                    .unresolved
                    .iter()
                    .filter(|u| u.from == path)
                    .map(|u| unresolved_for(u, &readable))
                    .collect(),
                above_rank,
                titles,
                anchors,
            })
        })
    }

    /// The visible subgraph around one file or folder: from a file its
    /// `links` in both directions, from a folder its members at hop 1 and their links
    /// at hop 2. Breadth-first over readable nodes only, so a hidden node is neither
    /// returned nor walked through; within a hop `in_links` descending, then path; at
    /// most `limit` nodes besides the centre, `truncated` when the cut dropped one.
    pub fn neighbourhood(
        &self,
        who: &TrustedIdentity,
        path: &str,
        depth: u8,
        limit: usize,
    ) -> Result<NeighbourhoodResult> {
        crate::valid_path(path)?;
        if !(1..=2).contains(&depth) {
            return Err(invalid_request("depth must be 1 or 2"));
        }
        if !(1..=MAX_NEIGHBOURS).contains(&limit) {
            return Err(invalid_request("limit must be between 1 and 64"));
        }
        self.warm_graph_index(&who.tenant_id);
        self.transaction_for(who, false, vec![], false, None, None, |s| {
            // Gives a non-member its grant answer (`forbidden`); `visible` below decides
            // for members, including implicit folders and earlier names.
            crate::ranks::check(s, who, Permission::Read, path)?;
            let (index, status) = reconcile(&self.root, &self.graph_cache, s, &who.tenant_id)?;
            let readable = visible(s, who, &index);
            if !readable.contains(path) {
                return Err(err("not_found"));
            }
            let by_path: HashMap<&str, &IndexNode> =
                index.nodes.iter().map(|n| (n.path.as_str(), n)).collect();
            // Adjacency over readable ends only.
            let mut linked: HashMap<&str, Vec<&str>> = HashMap::new();
            let mut members: HashMap<&str, Vec<&str>> = HashMap::new();
            for e in &index.edges {
                let (from, to) = (e.from.as_str(), e.to.as_str());
                if !readable.contains(from) || !readable.contains(to) {
                    continue;
                }
                match e.kind {
                    EdgeKind::Links => {
                        linked.entry(from).or_default().push(to);
                        linked.entry(to).or_default().push(from);
                    }
                    EdgeKind::Contains => members.entry(from).or_default().push(to),
                }
            }
            let centre = by_path[path];
            let mut hop_of: HashMap<&str, u8> = HashMap::from([(path, 0)]);
            let mut order: Vec<&str> = vec![path];
            let mut frontier: Vec<&str> = vec![path];
            let mut truncated = false;
            for hop in 1..=depth {
                let mut next: Vec<&str> = vec![];
                let mut queued: HashSet<&str> = HashSet::new();
                for from in &frontier {
                    let step = if hop == 1 && centre.kind == EntryKind::Folder {
                        members.get(from)
                    } else {
                        linked.get(from)
                    };
                    for to in step.into_iter().flatten() {
                        if !hop_of.contains_key(to) && queued.insert(to) {
                            next.push(to);
                        }
                    }
                }
                next.sort_by(|a, b| {
                    by_path[b]
                        .in_links
                        .cmp(&by_path[a].in_links)
                        .then_with(|| a.as_bytes().cmp(b.as_bytes()))
                });
                let room = limit + 1 - order.len();
                if next.len() > room {
                    next.truncate(room);
                    truncated = true;
                }
                for p in &next {
                    hop_of.insert(p, hop);
                    order.push(p);
                }
                if truncated || next.is_empty() {
                    break;
                }
                frontier = next;
            }
            let files = crate::ranks::head_files(s, &who.tenant_id);
            let tp = crate::ranks::tenant_policy(s, &who.tenant_id);
            let mut defaults: HashMap<&str, (String, ColourSource)> = HashMap::new();
            let nodes = order
                .iter()
                .map(|p| {
                    let n = by_path[p];
                    let (colour, colour_source) =
                        colour(&files, &tp, &who.tenant_id, &n.path, &mut defaults);
                    NeighbourNode {
                        path: n.path.clone(),
                        file_id: n.file_id.clone(),
                        title: n.title.clone(),
                        kind: n.kind.clone(),
                        seen_from: crate::ranks::effective_seen_from(&files, &tp, &n.path),
                        colour,
                        colour_source,
                        snippet: n.snippet.clone(),
                        hop: hop_of[p],
                    }
                })
                .collect();
            let edges = index
                .edges
                .iter()
                .filter(|e| {
                    hop_of.contains_key(e.from.as_str()) && hop_of.contains_key(e.to.as_str())
                })
                .map(|e| GraphEdge {
                    from: e.from.clone(),
                    to: e.to.clone(),
                    kind: e.kind,
                })
                .collect();
            Ok(NeighbourhoodResult {
                revision_id: index.revision_id.clone(),
                index: IndexInfo {
                    revision_id: index.revision_id.clone(),
                    status,
                },
                centre: path.into(),
                nodes,
                edges,
                unresolved: index
                    .unresolved
                    .iter()
                    .filter(|u| u.from == path)
                    .map(|u| unresolved_for(u, &readable))
                    .collect(),
                above_rank: above_rank(s, who, &index, &files, path),
                truncated,
            })
        })
    }

    /// Test hook: reconciles the caller's tenant graph index and returns
    /// `{"status", "index"}`, unfiltered, so only a root administrator may call it.
    #[doc(hidden)]
    pub fn graph_index(&self, who: &TrustedIdentity) -> Result<serde_json::Value> {
        self.transaction_for(who, false, vec![], false, None, None, |s| {
            crate::ranks::check(s, who, Permission::Admin, "")?;
            let (index, status) = reconcile(&self.root, &self.graph_cache, s, &who.tenant_id)?;
            Ok(serde_json::json!({ "status": status, "index": &*index }))
        })
    }

    /// Test hook: how many times this service has parsed a stored graph index file.
    #[doc(hidden)]
    pub fn graph_index_parses(&self) -> u64 {
        self.graph_cache.parses.load(Ordering::Relaxed)
    }

    /// Reads and parses the tenant's stored index into the cache before the authority
    /// lock is taken, so that under the lock a request only compares the file's
    /// metadata. The file is replaced by rename, so it reads whole without the lock.
    pub(crate) fn warm_graph_index(&self, tenant_id: &str) {
        self.graph_cache.load(&self.root, tenant_id);
    }
}

/// `ranks::effective_colour`, with the default of each top-level segment computed once:
/// that default scans every top-level folder.
pub(crate) fn colour<'a>(
    files: &BTreeMap<String, Entry>,
    tp: &TenantPolicy,
    tenant_id: &str,
    path: &'a str,
    defaults: &mut HashMap<&'a str, (String, ColourSource)>,
) -> (String, ColourSource) {
    let explicit = std::iter::successors(Some(path), |p| p.rsplit_once('/').map(|(a, _)| a))
        .any(|p| files.get(p).is_some_and(|e| e.meta.colour.is_some()));
    if explicit {
        return crate::ranks::effective_colour(files, tp, tenant_id, path);
    }
    let top = path.split('/').next().unwrap_or(path);
    defaults
        .entry(top)
        .or_insert_with(|| crate::ranks::effective_colour(files, tp, tenant_id, top))
        .clone()
}
