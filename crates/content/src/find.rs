//! `find`: files and folders at head by name and path, for the ⌘K bar. Every query
//! token (case-folded Unicode words) must be the prefix of a word of the basename (a
//! `name` hit) or else of the whole path (a `path` hit). Name hits rank before path
//! hits, then the newest `changed.at`, then the path, over every candidate before the
//! `limit` cut. No index: the head manifest is scanned, only what the caller may read is
//! matched, and one metadata-only walk of the revision chain stamps every candidate.
//! Hits carry the graph index's titles, so `find` reconciles that index; when it cannot,
//! the hits carry basename titles instead and `find` still answers.
//!
//! With `typos` a token of at least 5 characters also matches a word
//! within one edit of a prefix; such hits rank after every exact one (name-exact,
//! path-exact, name-fuzzy, path-fuzzy) and say `fuzzy: true`. The words one edit away
//! come from the graph index's trigram index (bounded candidates, never a scan); when
//! the index cannot reconcile, from a trigram index of the readable paths.
use crate::auth::{Permission, TrustedIdentity};
use crate::ranks::ColourSource;
use crate::{err, EntryKind, Result, Service};
use serde::{Deserialize, Serialize};
use std::cmp::Reverse;
use std::collections::{BTreeMap, HashMap, HashSet};

pub const DEFAULT_LIMIT: usize = 20;
pub const MAX_LIMIT: usize = 50;
const MAX_QUERY_BYTES: usize = 4096;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FindHit {
    pub path: String,
    /// Empty for an implicit parent folder that has no entry of its own.
    pub file_id: String,
    pub kind: EntryKind,
    /// The graph index's title.
    pub title: String,
    pub colour: String,
    pub seen_from: String,
    /// `"name"` or `"path"`.
    pub matched: String,
    /// When the file identity last changed (its `stat` `changed.at`); `null` for an
    /// implicit folder.
    pub changed_at: Option<String>,
    /// Some token matched only with an edit forgiven.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub fuzzy: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FindResult {
    pub revision_id: String,
    pub hits: Vec<FindHit>,
}

/// Case-folded Unicode words: runs of alphanumerics.
pub(crate) fn words(text: &str) -> Vec<String> {
    text.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(str::to_owned)
        .collect()
}

/// Every token prefixes a word of `words`, or (with `near`, one set per token) is one
/// of the words that token reaches with an edit.
fn prefixes_all(tokens: &[String], near: Option<&[HashSet<String>]>, words: &[String]) -> bool {
    tokens.iter().enumerate().all(|(i, t)| {
        words.iter().any(|w| {
            w.starts_with(t.as_str()) || near.is_some_and(|near| near[i].contains(w.as_str()))
        })
    })
}

/// 0 name-exact, 1 path-exact, 2 name-fuzzy, 3 path-fuzzy. `near` holds one set per
/// token: the words it reaches only with an edit (empty when none is forgiven).
fn rank(tokens: &[String], near: &[HashSet<String>], path: &str) -> Option<u8> {
    let name = words(path.rsplit('/').next().unwrap_or(path));
    let whole = words(path);
    if prefixes_all(tokens, None, &name) {
        Some(0)
    } else if prefixes_all(tokens, None, &whole) {
        Some(1)
    } else if near.iter().all(HashSet::is_empty) {
        None
    } else if prefixes_all(tokens, Some(near), &name) {
        Some(2)
    } else if prefixes_all(tokens, Some(near), &whole) {
        Some(3)
    } else {
        None
    }
}

/// Each pass that lacks manifests asks for twice as many per candidate as the last.
const MAX_PASSES: u32 = 32;

/// What one candidate's stamp walk has reached.
enum Walk {
    /// Still looking, older revisions next.
    Open,
    /// Waits for this many unloaded revisions (and their parents) to be hydrated.
    Waiting(usize),
}

/// One walk of the chain from head, newest first, for every candidate identity at
/// once: each gets the `at` of the newest revision that touched it and that the caller
/// could read then (as `history` does). `Err` names the manifests the walk lacks: a
/// candidate whose next touching revision is not loaded asks for up to `budget` of them.
fn stamps(
    s: &crate::State,
    who: &TrustedIdentity,
    t: &crate::Tenant,
    ids: &[&str],
    hydrated: &HashSet<String>,
    budget: usize,
) -> std::result::Result<HashMap<String, Option<String>>, HashSet<String>> {
    let mut found: HashMap<String, Option<String>> = HashMap::new();
    let mut walking: HashMap<&str, Walk> = ids.iter().map(|id| (*id, Walk::Open)).collect();
    let mut needed = HashSet::new();
    let loaded = |id: &str| id == t.head || hydrated.contains(id);
    for id in crate::chain(t, &t.head) {
        // Stop as soon as every candidate is stamped or has asked for all it may.
        if walking
            .values()
            .all(|w| matches!(w, Walk::Waiting(n) if *n >= budget))
        {
            break;
        }
        let revision = &t.revisions[&id].revision;
        let mut touched: BTreeMap<&str, Vec<crate::FileDiff>> = BTreeMap::new();
        for c in &revision.changes {
            if let Some(fid) = c.file_id.as_deref().filter(|f| walking.contains_key(f)) {
                touched.entry(fid).or_default().push(c.clone());
            }
        }
        if touched.is_empty() {
            continue;
        }
        let manifests: Vec<&String> = std::iter::once(&revision.id)
            .chain(revision.parent.as_ref())
            .collect();
        let ready = manifests.iter().all(|m| loaded(m));
        for (fid, changes) in touched {
            let walk = walking
                .get_mut(fid)
                .expect("touched only walking identities");
            match walk {
                Walk::Waiting(n) => {
                    if !ready && *n < budget {
                        needed.extend(manifests.iter().map(|m| (*m).clone()));
                        *n += 1;
                    }
                }
                Walk::Open if !ready => {
                    needed.extend(manifests.iter().map(|m| (*m).clone()));
                    *walk = Walk::Waiting(1);
                }
                Walk::Open => {
                    let readable = changes
                        .iter()
                        .all(|c| crate::authorize_diff(s, who, c).is_ok())
                        && crate::readable_then(s, who, t, revision, &changes).is_ok();
                    if readable {
                        walking.remove(fid);
                        found.insert(fid.to_owned(), revision.at.clone());
                    }
                }
            }
        }
    }
    if !needed.is_empty() {
        needed.extend(hydrated.iter().cloned());
        return Err(needed);
    }
    // The chain is exhausted: no revision the caller could read touched these.
    for (fid, _) in walking {
        found.insert(fid.to_owned(), None);
    }
    Ok(found)
}

impl Service {
    /// At most `limit` (1–50) readable files and folders at or below `scope` (`""` is the
    /// whole tenant) whose name or path matches `query`. A scope that is absent or hidden
    /// answers `not_found` alike.
    pub fn find(
        &self,
        who: &TrustedIdentity,
        query: &str,
        scope: &str,
        limit: usize,
        typos: bool,
    ) -> Result<FindResult> {
        if !scope.is_empty() {
            crate::valid_path(scope)?;
        }
        let tokens = words(query);
        if query.len() > MAX_QUERY_BYTES || tokens.is_empty() {
            return Err(err("invalid_search"));
        }
        if !(1..=MAX_LIMIT).contains(&limit) {
            return Err(err("invalid_request"));
        }
        // Parsing the stored graph index (words, titles) happens here, outside the
        // authority lock; under it `find_over` only checks the file is unchanged.
        self.warm_graph_index(&who.tenant_id);
        // Revision-time visibility needs the manifests of the revisions that stamp the
        // candidates; a pass asks only for those it reaches, doubling its reach each time.
        let mut hydrated = HashSet::new();
        for pass in 0..MAX_PASSES {
            let budget = 1usize << pass.min(20);
            match self.find_over(who, &tokens, typos, scope, limit, &hydrated, budget)? {
                Ok(result) => return Ok(result),
                Err(needed) => hydrated = needed,
            }
        }
        Err(err("conflict"))
    }

    /// The page, or the revisions whose manifests the stamps need when not loaded.
    #[allow(clippy::too_many_arguments)]
    fn find_over(
        &self,
        who: &TrustedIdentity,
        tokens: &[String],
        typos: bool,
        scope: &str,
        limit: usize,
        hydrated: &HashSet<String>,
        budget: usize,
    ) -> Result<std::result::Result<FindResult, HashSet<String>>> {
        // One metadata-only read: revision records without text, manifests only for the
        // head and `hydrated`.
        self.transaction_metadata(who, hydrated.iter().cloned().collect(), |s| {
            // Gives a non-member its grant answer (`forbidden`); visibility below
            // decides for members, including implicit folders and earlier names.
            crate::ranks::check(s, who, Permission::Read, scope)?;
            let tenant = s.tenants.get(&who.tenant_id).filter(|t| !t.head.is_empty());
            let revision_id = tenant.map_or("r0", |t| t.head.as_str()).to_owned();
            let files = crate::ranks::head_files(s, &who.tenant_id);
            let readable = crate::graph::visible_among(s, who, files.keys().map(String::as_str));
            if !scope.is_empty() && !readable.contains(scope) {
                return Err(err("not_found"));
            }
            // The graph index, reconciled once: its words for typos, its titles for hits.
            // One that cannot be reconciled never fails ⌘K.
            let mut index: Option<Option<std::sync::Arc<crate::graph::GraphIndex>>> = None;
            let mut reconciled = || {
                crate::graph::reconcile(&self.root, &self.graph_cache, s, &who.tenant_id)
                    .ok()
                    .map(|(index, _)| index)
            };
            let forgiving = |t: &String| typos && t.chars().count() >= crate::fuzzy::MIN_TYPO_CHARS;
            let near: Vec<HashSet<String>> = if tokens.iter().any(forgiving) {
                let graph = index.get_or_insert_with(&mut reconciled);
                let fallback;
                let words = match graph {
                    Some(graph) => &graph.words,
                    None => {
                        fallback = crate::fuzzy::TrigramIndex::build(crate::graph::path_words(
                            readable.iter().copied(),
                        ));
                        &fallback
                    }
                };
                tokens
                    .iter()
                    .map(|t| match forgiving(t) {
                        true => words.near(t).into_iter().map(str::to_owned).collect(),
                        false => HashSet::new(),
                    })
                    .collect()
            } else {
                vec![HashSet::new(); tokens.len()]
            };
            let matched: Vec<(u8, &str)> = readable
                .iter()
                .filter(|p| crate::graph::within(p, scope))
                .filter_map(|p| rank(tokens, &near, p).map(|r| (r, *p)))
                .collect();
            // Every candidate is stamped before the cut, so the order is total over all
            // of them: name before path, newest change first, then the path.
            let ids: Vec<&str> = matched
                .iter()
                .filter_map(|(_, p)| files.get(*p).map(|e| e.file_id.as_str()))
                .filter(|id| !id.is_empty())
                .collect();
            let stamped = match tenant {
                Some(t) if !ids.is_empty() => match stamps(s, who, t, &ids, hydrated, budget) {
                    Ok(stamped) => stamped,
                    Err(needed) => return Ok(Err(needed)),
                },
                _ => HashMap::new(),
            };
            let stamp_of = |path: &str| -> Option<String> {
                files
                    .get(path)
                    .and_then(|e| stamped.get(&e.file_id).cloned().flatten())
            };
            let mut ranked: Vec<(u8, Option<String>, &str)> = matched
                .into_iter()
                .map(|(r, p)| (r, stamp_of(p), p))
                .collect();
            // `None` orders below any stamp, so newest first puts unstamped folders last.
            ranked.sort_by(|(ra, sa, pa), (rb, sb, pb)| {
                (ra, Reverse(sa), pa).cmp(&(rb, Reverse(sb), pb))
            });
            ranked.truncate(limit);
            // Titles come from the graph index, reconciled as `graph` does. An index
            // that cannot be reconciled (a missing text, a failed save) never fails
            // ⌘K: every hit falls back to its basename title from the manifest.
            let index = if ranked.is_empty() {
                None
            } else {
                index.unwrap_or_else(reconciled)
            };
            let titles: HashMap<&str, &str> = index
                .iter()
                .flat_map(|i| i.nodes.iter())
                .map(|n| (n.path.as_str(), n.title.as_str()))
                .collect();
            let tp = crate::ranks::tenant_policy(s, &who.tenant_id);
            let mut defaults: HashMap<&str, (String, ColourSource)> = HashMap::new();
            let hits = ranked
                .into_iter()
                .map(|(r, changed_at, path)| {
                    let entry = files.get(path);
                    let (colour, _) =
                        crate::graph::colour(&files, &tp, &who.tenant_id, path, &mut defaults);
                    FindHit {
                        path: path.to_owned(),
                        file_id: entry.map(|e| e.file_id.clone()).unwrap_or_default(),
                        kind: entry.map_or(EntryKind::Folder, |e| e.kind.clone()),
                        title: titles.get(path).map_or_else(
                            || crate::outline_text::basename_title(path),
                            |t| t.to_string(),
                        ),
                        colour,
                        seen_from: crate::ranks::effective_seen_from(&files, &tp, path),
                        matched: if r % 2 == 0 { "name" } else { "path" }.into(),
                        changed_at,
                        fuzzy: r >= 2,
                    }
                })
                .collect();
            Ok(Ok(FindResult { revision_id, hits }))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_prefix_the_basename_before_the_path() {
        let q = |s: &str| words(s);
        let rank =
            |tokens: &[String], path: &str| rank(tokens, &vec![HashSet::new(); tokens.len()], path);
        assert_eq!(rank(&q("war"), "Warranty.md"), Some(0));
        assert_eq!(rank(&q("war"), "services/warranty-claims.md"), Some(0));
        assert_eq!(rank(&q("war"), "software.md"), None);
        assert_eq!(rank(&q("serv war"), "services/warranty-claims.md"), Some(1));
        assert_eq!(rank(&q("ÉCH"), "notes/échéancier.md"), Some(0));
        assert_eq!(rank(&q("arranty"), "Warranty.md"), None);
        assert!(q(" -/. ").is_empty());
    }

    #[test]
    fn near_words_rank_after_exact_ones() {
        let tokens = words("warrnaty");
        let near = vec![HashSet::from(["warranty".to_string()])];
        assert_eq!(rank(&tokens, &near, "Warranty.md"), Some(2));
        assert_eq!(rank(&tokens, &near, "warranty/terms.md"), Some(3));
        assert_eq!(rank(&tokens, &near, "warrnaty.md"), Some(0));
        assert_eq!(rank(&tokens, &near, "terms.md"), None);
    }
}
