//! Ranked retrieval over the current published revision. The authority lock covers
//! authorization, cache reconciliation and ranking; the index never grants access.
use crate::extraction::{self, ExtractionStatus, SourceLocation};
use crate::knowledge::HitSection;
use crate::{
    auth::{Permission, TrustedIdentity},
    authorize_identity, err, fingerprint, storage, ContentError, EntryKind, Result, Service,
};
use mnemosyne_index::semantic::{sync_semantic, SemanticConfig};
use mnemosyne_index::{sync_search_fields, ManagedDocument, ManagedQuery};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet, HashMap};

fn default_limit() -> usize {
    20
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SearchMode {
    #[default]
    Keyword,
    Semantic,
    Hybrid,
}
/// What a keyword query is matched against: the text (`body`, keyword scoring) and the
/// file name (`name`, a fixed boost of 2.0 per name hit).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SearchField {
    Body,
    Name,
}
impl SearchField {
    pub fn all() -> Vec<SearchField> {
        vec![SearchField::Body, SearchField::Name]
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SearchRequest {
    pub query: String,
    #[serde(default)]
    pub mode: SearchMode,
    /// Cosine floor for semantic candidates; defaults to zero. Not a probability.
    #[serde(default)]
    pub min_score: Option<f32>,
    #[serde(default)]
    pub scope: String,
    #[serde(default)]
    pub extensions: Vec<String>,
    #[serde(default = "default_limit")]
    pub limit: usize,
    #[serde(default)]
    pub offset: usize,
    /// Pin later pages to the same authority, policy and query snapshot.
    #[serde(default)]
    pub snapshot_id: Option<String>,
    /// Keyword fields; defaults to both. Semantic retrieval reads the body only.
    #[serde(default = "SearchField::all")]
    pub fields: Vec<SearchField>,
    /// The keyword half forgives one edit in words of 5+ characters and ranks exact
    /// matches first; part of the snapshot.
    #[serde(default = "yes")]
    pub typos: bool,
}
fn yes() -> bool {
    true
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SearchHit {
    pub file_id: String,
    pub path: String,
    pub revision_id: String,
    pub score: f32,
    /// Plain text, never HTML. Renderers must escape it as ordinary source text.
    pub snippet: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<SearchSource>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chunk: Option<SearchChunk>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ranking: Option<SearchRanking>,
    /// The deepest heading of a text whose range holds the hit's offset;
    /// absent in the preamble, on extracted text and beyond the outline bounds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub section: Option<HitSection>,
    /// The keyword half matched only with an edit forgiven.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub fuzzy: bool,
}
/// Byte offsets into the UTF-8 original text or the referenced extracted segment.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SearchChunk {
    pub byte_start: usize,
    pub byte_end: usize,
    pub model_fingerprint: String,
    pub dimensions: usize,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SearchRanking {
    pub method: String,
    pub keyword_rank: Option<usize>,
    pub keyword_score: Option<f32>,
    pub semantic_rank: Option<usize>,
    pub semantic_score: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rrf_k: Option<usize>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SearchSource {
    pub blob: String,
    pub extractor: String,
    pub version: String,
    pub segment: u32,
    pub location: SourceLocation,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SearchIndexStatus {
    pub revision_id: String,
    pub status: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SearchResult {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retrieval: Option<SearchRetrieval>,
    pub revision_id: String,
    pub snapshot_id: String,
    pub total: usize,
    pub hits: Vec<SearchHit>,
    pub next_offset: Option<usize>,
    pub index: SearchIndexStatus,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SearchRetrieval {
    pub mode: SearchMode,
    pub min_score: f32,
    pub model_fingerprint: String,
    pub dimensions: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rrf_k: Option<usize>,
}
impl Service {
    pub fn search(&self, who: &TrustedIdentity, request: &SearchRequest) -> Result<SearchResult> {
        if !crate::auth::valid_path(&request.scope, true) {
            return Err(err("invalid_path"));
        }
        if request.query.len() > 4096
            || !request.query.chars().any(char::is_alphanumeric)
            || request
                .query
                .to_lowercase()
                .split(|c: char| !c.is_alphanumeric())
                .any(|token| token.len() > 80)
            || request
                .min_score
                .is_some_and(|s| !s.is_finite() || !(-1.0..=1.0).contains(&s))
            || (request.mode == SearchMode::Keyword && request.min_score.is_some())
            || !(1..=100).contains(&request.limit)
            || request.fields.is_empty()
            || request.extensions.len() > 32
            || request.extensions.iter().any(|extension| {
                extension.is_empty()
                    || extension.len() > 32
                    || !extension
                        .chars()
                        .all(|c| c.is_alphanumeric() || c == '_' || c == '-')
            })
        {
            return Err(err("invalid_search"));
        }
        let extensions: BTreeSet<_> = request
            .extensions
            .iter()
            .map(|e| e.to_lowercase())
            .collect();
        let fields: BTreeSet<SearchField> = request.fields.iter().copied().collect();
        self.transaction_for(who, false, vec![], false, None, None, |state| {
            crate::ranks::check(state, who, Permission::Read, &request.scope)?;
            let tenant = state.tenants.get(&who.tenant_id);
            let revision = tenant
                .map(|t| t.head.as_str())
                .filter(|h| !h.is_empty())
                .unwrap_or("r0");
            // Include only this tenant's policy. A policy or published-content change
            // invalidates pagination, including a change that grants new visibility.
            let grants: Vec<_> = state
                .policy
                .grants
                .iter()
                .filter(|g| g.tenant_id == who.tenant_id)
                .collect();
            // Roles, membership, rungs and root visibility also decide what is visible.
            let ranks = state.policy.tenants.get(&who.tenant_id);
            let files = tenant
                .and_then(|t| t.revisions.get(revision))
                .map(|s| &s.files);
            let mut documents = vec![];
            let mut allowed = BTreeSet::new();
            let mut sources = BTreeMap::new();
            let mut extracted = BTreeMap::new();
            if let Some(files) = files {
                for (path, entry) in files {
                    let in_scope = request.scope.is_empty()
                        || path == &request.scope
                        || path
                            .strip_prefix(&request.scope)
                            .is_some_and(|s| s.starts_with('/'));
                    let extension_matches = extensions.is_empty()
                        || path
                            .rsplit('/')
                            .next()
                            .and_then(|name| name.rsplit_once('.'))
                            .is_some_and(|(_, ext)| extensions.contains(&ext.to_lowercase()));
                    // Query visibility is independent of the persistent manifest.
                    // Only visible binary sources may invoke a parser on this request.
                    let visible = in_scope
                        && extension_matches
                        && authorize_identity(state, who, entry, path).is_ok();
                    if entry.kind == EntryKind::Text {
                        let blob = entry.blob.clone().ok_or_else(|| err("corrupt_blob"))?;
                        documents.push(ManagedDocument {
                            path: path.clone(),
                            file_id: entry.file_id.clone(),
                            blob,
                        });
                        if visible {
                            allowed.insert(path.clone());
                        }
                        sources.insert(path.clone(), (path.clone(), None));
                    } else if entry.kind == EntryKind::Binary {
                        let Some(format) = extraction::format(path) else {
                            continue;
                        };
                        let blob = entry.blob.as_deref().ok_or_else(|| err("corrupt_blob"))?;
                        // Retain all current, already extracted sources independently
                        // of who is querying. Cache validation never parses a denied
                        // source; a missing/invalid cache omits its derived documents.
                        let record = if visible {
                            extraction::extract(&self.root, blob, format, false)?
                        } else {
                            let Some(record) = extraction::cached(&self.root, blob, format)? else {
                                continue;
                            };
                            record
                        };
                        if !matches!(
                            record.status,
                            ExtractionStatus::Succeeded | ExtractionStatus::Partial
                        ) {
                            continue;
                        }
                        for (i, segment) in record.segments.iter().enumerate() {
                            // Control characters are forbidden in real paths: no uploaded
                            // file can collide with this private segment-key namespace.
                            let key = format!("\0{path}\0{i}");
                            let hash = format!("{:x}", Sha256::digest(segment.text.as_bytes()));
                            extracted.insert(hash.clone(), segment.text.clone());
                            documents.push(ManagedDocument {
                                path: key.clone(),
                                file_id: entry.file_id.clone(),
                                blob: hash,
                            });
                            if visible {
                                allowed.insert(key.clone());
                            }
                            sources.insert(
                                key,
                                (
                                    path.clone(),
                                    Some(SearchSource {
                                        blob: blob.into(),
                                        extractor: record.extractor.clone(),
                                        version: record.version.clone(),
                                        segment: (i + 1) as u32,
                                        location: segment.location.clone(),
                                    }),
                                ),
                            );
                        }
                    }
                }
            }
            // Derived output can change after retry without changing authority head.
            // Bind pagination to exact currently searchable segment content as well.
            let manifest: Vec<_> = documents.iter().map(|d| (&d.path, &d.blob)).collect();
            // Host configuration is never accepted from operation JSON. Keyword
            // searches do not inspect or initialize any embedding provider.
            let semantic_config = if request.mode != SearchMode::Keyword {
                Some(SemanticConfig::from_env().map_err(|_| err("semantic_unavailable"))?)
            } else {
                None
            };
            let model_fingerprint = semantic_config
                .as_ref()
                .map(|c| c.fingerprint())
                .transpose()
                .map_err(|_| err("semantic_unavailable"))?;
            let snapshot_id = fingerprint(&(
                revision,
                who,
                grants,
                ranks,
                &request.query,
                &request.mode,
                &request.min_score,
                &model_fingerprint,
                &request.scope,
                &extensions,
                manifest,
                &sources,
                &fields,
                &request.typos,
            ))?;
            if request
                .snapshot_id
                .as_ref()
                .is_some_and(|id| id != &snapshot_id)
            {
                return Err(err("stale_search"));
            }
            let load = |blob: &str| {
                if let Some(text) = extracted.get(blob) {
                    return Ok(text.clone());
                }
                storage::read_blob(&self.root, blob)
                    .and_then(|bytes| String::from_utf8(bytes).map_err(crate::io))
                    .map_err(|e| e.to_string())
            };
            let keyword = if request.mode != SearchMode::Semantic {
                let result = sync_search_fields(
                    &self.root,
                    &who.tenant_id,
                    revision,
                    &documents,
                    &allowed,
                    ManagedQuery {
                        text: &request.query,
                        body: fields.contains(&SearchField::Body),
                        name: fields.contains(&SearchField::Name),
                        typos: request.typos,
                    },
                    load,
                )
                .map_err(|_| ContentError::new("search_unavailable", "Search index unavailable"))?;
                if result.indexed_revision != revision
                    || result
                        .hits
                        .iter()
                        .any(|h| !allowed.contains(&h.path) || !h.score.is_finite())
                {
                    return Err(err("search_unavailable"));
                }
                Some(result)
            } else {
                None
            };
            let semantic = if let Some(config) = semantic_config.as_ref() {
                // Only authorized current candidates enter vector lookup, embedding,
                // scoring or snippets, including full historical identity lineage.
                let visible: Vec<_> = documents
                    .iter()
                    .filter(|d| allowed.contains(&d.path))
                    .cloned()
                    .collect();
                let mut result = sync_semantic(
                    &self.root,
                    &who.tenant_id,
                    revision,
                    &visible,
                    &allowed,
                    &request.query,
                    config,
                    load,
                )
                .map_err(|_| err("semantic_unavailable"))?;
                if result.indexed_revision != revision
                    || Some(&result.model_fingerprint) != model_fingerprint.as_ref()
                    || result
                        .hits
                        .iter()
                        .any(|h| !allowed.contains(&h.path) || !h.score.is_finite())
                {
                    return Err(err("semantic_unavailable"));
                }
                result
                    .hits
                    .retain(|h| h.score >= request.min_score.unwrap_or(0.0));
                Some(result)
            } else {
                None
            };
            let mut ranked = BTreeMap::<String, RankedHit>::new();
            if let Some(result) = &keyword {
                for (i, hit) in result.hits.iter().enumerate() {
                    ranked.insert(
                        hit.path.clone(),
                        RankedHit {
                            path: hit.path.clone(),
                            score: hit.score,
                            snippet: hit.snippet.clone(),
                            chunk: None,
                            keyword_offset: hit.byte_start,
                            fuzzy: hit.fuzzy,
                            ranking: SearchRanking {
                                method: "reciprocal_rank_fusion".into(),
                                keyword_rank: Some(i + 1),
                                keyword_score: Some(hit.score),
                                semantic_rank: None,
                                semantic_score: None,
                                rrf_k: Some(60),
                            },
                        },
                    );
                }
            }
            if let Some(result) = &semantic {
                for (i, hit) in result.hits.iter().enumerate() {
                    let ranked_hit = ranked.entry(hit.path.clone()).or_insert_with(|| RankedHit {
                        path: hit.path.clone(),
                        score: hit.score,
                        snippet: hit.snippet.clone(),
                        chunk: None,
                        keyword_offset: None,
                        fuzzy: false,
                        ranking: SearchRanking {
                            method: "cosine".into(),
                            keyword_rank: None,
                            keyword_score: None,
                            semantic_rank: None,
                            semantic_score: None,
                            rrf_k: None,
                        },
                    });
                    // A semantic snippet always resolves to the returned chunk offsets.
                    ranked_hit.snippet = hit.snippet.clone();
                    ranked_hit.chunk = Some(SearchChunk {
                        byte_start: hit.byte_start,
                        byte_end: hit.byte_end,
                        model_fingerprint: result.model_fingerprint.clone(),
                        dimensions: result.dimensions,
                    });
                    ranked_hit.ranking.semantic_rank = Some(i + 1);
                    ranked_hit.ranking.semantic_score = Some(hit.score);
                }
            }
            let hits = rank_hits(request.mode, ranked.into_values().collect());
            let update = semantic
                .as_ref()
                .map(|r| r.update.clone())
                .or_else(|| keyword.as_ref().map(|r| r.update.clone()))
                .unwrap();
            let total = hits.len();
            // At most one outline per distinct text path on the page, at head.
            let mut outlines: HashMap<String, Option<crate::knowledge::OutlineResult>> =
                HashMap::new();
            let hits = hits
                .into_iter()
                .skip(request.offset)
                .take(request.limit)
                .map(|hit| {
                    let (path, source) = sources
                        .get(&hit.path)
                        .ok_or_else(|| err("search_unavailable"))?;
                    let entry = files
                        .and_then(|f| f.get(path))
                        .ok_or_else(|| err("search_unavailable"))?;
                    // Fail closed if a backend ever returns a path outside the authorized set.
                    if !allowed.contains(&hit.path) {
                        return Err(err("search_unavailable"));
                    }
                    // A semantic (or hybrid) hit's offset is its chunk's; a keyword hit's
                    // the first matching word's. Extracted text has `source` instead.
                    let offset = hit
                        .chunk
                        .as_ref()
                        .map(|c| c.byte_start)
                        .or(hit.keyword_offset);
                    let section = match (source, offset, &entry.blob) {
                        (None, Some(offset), Some(blob)) => outlines
                            .entry(path.clone())
                            .or_insert_with(|| {
                                let content = load(blob).ok()?;
                                crate::knowledge::outline_of(&crate::ReadResult {
                                    revision_id: revision.into(),
                                    path: path.clone(),
                                    content,
                                })
                                .ok()
                            })
                            .as_ref()
                            .and_then(|o| crate::knowledge::section_at(o, offset)),
                        _ => None,
                    };
                    Ok(SearchHit {
                        file_id: entry.file_id.clone(),
                        path: path.clone(),
                        source: source.clone(),
                        revision_id: revision.into(),
                        score: hit.score,
                        snippet: hit.snippet,
                        chunk: hit.chunk,
                        ranking: (request.mode != SearchMode::Keyword).then_some(hit.ranking),
                        section,
                        fuzzy: hit.fuzzy,
                    })
                })
                .collect::<Result<Vec<_>>>()?;
            let end = request.offset.saturating_add(hits.len());
            Ok(SearchResult {
                retrieval: semantic.as_ref().map(|r| SearchRetrieval {
                    mode: request.mode,
                    min_score: request.min_score.unwrap_or(0.0),
                    model_fingerprint: r.model_fingerprint.clone(),
                    dimensions: r.dimensions,
                    rrf_k: (request.mode == SearchMode::Hybrid).then_some(60),
                }),
                revision_id: revision.into(),
                snapshot_id,
                total,
                next_offset: (end < total).then_some(end),
                hits,
                index: SearchIndexStatus {
                    revision_id: revision.into(),
                    status: update,
                },
            })
        })
    }
}

struct RankedHit {
    path: String,
    score: f32,
    snippet: String,
    chunk: Option<SearchChunk>,
    /// The keyword half's offset of the first matching word.
    keyword_offset: Option<usize>,
    /// The keyword half matched only with an edit forgiven.
    fuzzy: bool,
    ranking: SearchRanking,
}
fn rrf(keyword_rank: Option<usize>, semantic_rank: Option<usize>) -> f32 {
    [keyword_rank, semantic_rank]
        .into_iter()
        .flatten()
        .map(|rank| 1.0 / (60.0 + rank as f32))
        .sum()
}

fn rank_hits(mode: SearchMode, mut hits: Vec<RankedHit>) -> Vec<RankedHit> {
    if mode == SearchMode::Hybrid {
        for hit in &mut hits {
            hit.score = rrf(hit.ranking.keyword_rank, hit.ranking.semantic_rank);
            hit.ranking.method = "reciprocal_rank_fusion".into();
            hit.ranking.rrf_k = Some(60);
        }
    }
    // A keyword search ranks every exact document first; hybrid fusion
    // already takes the keyword half's exact-first ranks.
    let grouped = mode == SearchMode::Keyword;
    hits.sort_by(|a, b| {
        (grouped && a.fuzzy)
            .cmp(&(grouped && b.fuzzy))
            .then_with(|| b.score.total_cmp(&a.score))
            .then_with(|| a.path.cmp(&b.path))
    });
    hits
}

#[cfg(test)]
mod tests {
    use super::*;
    fn candidate(
        path: &str,
        raw_score: f32,
        keyword: Option<usize>,
        semantic: Option<usize>,
    ) -> RankedHit {
        RankedHit {
            path: path.into(),
            score: raw_score,
            snippet: path.into(),
            chunk: None,
            keyword_offset: None,
            fuzzy: false,
            ranking: SearchRanking {
                method: "pending".into(),
                keyword_rank: keyword,
                keyword_score: keyword.map(|_| raw_score),
                semantic_rank: semantic,
                semantic_score: semantic.map(|_| raw_score),
                rrf_k: None,
            },
        }
    }
    #[test]
    fn fusion_discards_incomparable_score_scales_and_breaks_ties_by_path() {
        let run = |scale| {
            rank_hits(
                SearchMode::Hybrid,
                vec![
                    candidate("z-semantic", 0.99, None, Some(1)),
                    candidate("a-keyword", scale, Some(1), None),
                    candidate("both", 0.01, Some(2), Some(2)),
                ],
            )
        };
        let first = run(1_000_000.0);
        let rescaled = run(0.00001);
        assert_eq!(
            first
                .iter()
                .map(|h| (h.path.as_str(), h.score))
                .collect::<Vec<_>>(),
            rescaled
                .iter()
                .map(|h| (h.path.as_str(), h.score))
                .collect::<Vec<_>>()
        );
        assert_eq!(
            first.iter().map(|h| h.path.as_str()).collect::<Vec<_>>(),
            ["both", "a-keyword", "z-semantic"]
        );
        for hit in &first {
            assert_eq!(hit.ranking.method, "reciprocal_rank_fusion");
            assert_eq!(hit.ranking.rrf_k, Some(60));
        }
        assert_eq!(first[1].ranking.keyword_score, Some(1_000_000.0));
        assert_eq!(first[2].ranking.semantic_score, Some(0.99));
    }
    #[test]
    fn single_mode_scores_and_stable_order_are_preserved() {
        for mode in [SearchMode::Keyword, SearchMode::Semantic] {
            let hits = rank_hits(
                mode,
                vec![
                    candidate("z", 0.7, None, Some(1)),
                    candidate("a", 0.7, None, Some(2)),
                    candidate("top", 0.9, None, Some(3)),
                ],
            );
            assert_eq!(
                hits.iter()
                    .map(|h| (h.path.as_str(), h.score))
                    .collect::<Vec<_>>(),
                [("top", 0.9), ("a", 0.7), ("z", 0.7)]
            );
        }
    }
}
