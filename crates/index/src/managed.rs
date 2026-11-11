//! Disposable managed-content index. Callers hold the authority lock and provide
//! the complete committed text manifest plus freshly authorized paths. The source
//! store is authoritative; index errors never produce stale results.
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
};
use tantivy::{
    collector::TopDocs,
    doc,
    query::{
        BooleanQuery, BoostQuery, ConstScoreQuery, FuzzyTermQuery, Occur, Query, TermQuery,
        TermSetQuery,
    },
    schema::{
        Field, IndexRecordOption, Schema, TextFieldIndexing, TextOptions, Value, STORED, STRING,
    },
    tokenizer::{
        Language, LowerCaser, RemoveLongFilter, SimpleTokenizer, Stemmer, TextAnalyzer, TokenStream,
    },
    Index, TantivyDocument, Term,
};

#[derive(Clone, Debug)]
pub struct ManagedDocument {
    pub path: String,
    pub file_id: String,
    pub blob: String,
}
#[derive(Debug)]
pub struct ManagedHit {
    pub path: String,
    pub score: f32,
    pub snippet: String,
    /// Byte offset in the indexed text of the first matching word (the snippet's
    /// anchor); `None` when no word of the text matches, as for a name-only hit.
    pub byte_start: Option<usize>,
    /// The document matches only with an edit forgiven in some query word.
    pub fuzzy: bool,
}
/// A keyword query and the fields it is matched against. A `body` hit is a document
/// whose text (or title) holds every query term; a `name` hit one whose source file
/// name (its basename without the extension) holds every query term. A name hit adds
/// `NAME_BOOST` to the body score, which is zero when body matching is off.
///
/// With `typos`, a body query word of at least `MIN_TYPO_CHARS` characters also matches
/// a term within one edit (Damerau–Levenshtein, transposition counted as one); every
/// document that matches all words exactly ranks above every one that needs an edit.
#[derive(Clone, Copy, Debug)]
pub struct ManagedQuery<'a> {
    pub text: &'a str,
    pub body: bool,
    pub name: bool,
    pub typos: bool,
}
pub const NAME_BOOST: f32 = 2.0;
/// The shortest query word (in characters) that may match with an edit.
pub const MIN_TYPO_CHARS: usize = 5;
#[derive(Debug)]
pub struct ManagedSearch {
    pub hits: Vec<ManagedHit>,
    pub indexed_revision: String,
    pub update: String,
}
#[derive(Serialize, Deserialize, PartialEq, Eq)]
struct Manifest {
    version: u32,
    revision: String,
    documents: BTreeMap<String, (String, String)>,
}
struct Fields {
    path: Field,
    body: Field,
    en: Field,
    ru: Field,
    en_title: Field,
    ru_title: Field,
    relevance: Field,
}
fn schema() -> (Schema, Fields) {
    let mut b = Schema::builder();
    let path = b.add_text_field("path", STRING | STORED);
    let body = b.add_text_field("body", STORED);
    let relevance = b.add_text_field("relevance_v3", STORED);
    let options = |name| {
        TextOptions::default().set_indexing_options(
            TextFieldIndexing::default()
                .set_tokenizer(name)
                .set_index_option(IndexRecordOption::WithFreqs),
        )
    };
    let en = b.add_text_field("en", options("managed_en"));
    let ru = b.add_text_field("ru", options("managed_ru"));
    let en_title = b.add_text_field("en_title", options("managed_en"));
    let ru_title = b.add_text_field("ru_title", options("managed_ru"));
    (
        b.build(),
        Fields {
            path,
            body,
            en,
            ru,
            en_title,
            ru_title,
            relevance,
        },
    )
}
fn analyzer(language: Language) -> TextAnalyzer {
    TextAnalyzer::builder(SimpleTokenizer::default())
        .filter(RemoveLongFilter::limit(81))
        .filter(LowerCaser)
        .filter(Stemmer::new(language))
        .build()
}
fn register(index: &Index) {
    index
        .tokenizers()
        .register("managed_en", analyzer(Language::English));
    index
        .tokenizers()
        .register("managed_ru", analyzer(Language::Russian));
}
fn normalize(s: &str) -> String {
    s.to_lowercase().replace('ё', "е")
}
fn tokens(text: &str, language: Language) -> Vec<String> {
    let mut a = analyzer(language);
    let mut stream = a.token_stream(text);
    let mut out = Vec::new();
    while stream.advance() {
        out.push(stream.token().text.clone());
    }
    out
}
fn failpoint(name: &str) {
    if std::env::var("MNEMOSYNE_INDEX_FAILPOINT").as_deref() == Ok(name) {
        std::process::exit(86);
    }
}
/// Synchronizes the committed manifest before querying. Missing/corrupt caches
/// are rebuilt automatically. The caller must serialize all access to `root`.
/// Scores use only matching document-local signals; allowed paths are part of the query
/// before collection, totals, snippets or pagination, never a post-limit filter.
pub fn sync_search(
    root: &Path,
    tenant: &str,
    revision: &str,
    documents: &[ManagedDocument],
    allowed: &BTreeSet<String>,
    query: &str,
    load: impl FnMut(&str) -> Result<String, String>,
) -> Result<ManagedSearch, String> {
    let query = ManagedQuery {
        text: query,
        body: true,
        name: false,
        typos: false,
    };
    sync_search_fields(root, tenant, revision, documents, allowed, query, load)
}
/// `sync_search` with the fields of `query`.
pub fn sync_search_fields(
    root: &Path,
    tenant: &str,
    revision: &str,
    documents: &[ManagedDocument],
    allowed: &BTreeSet<String>,
    query: ManagedQuery,
    mut load: impl FnMut(&str) -> Result<String, String>,
) -> Result<ManagedSearch, String> {
    let dir = root
        .join("search-v1")
        .join(format!("{:x}", Sha256::digest(tenant.as_bytes())));
    let manifest = Manifest {
        version: 3,
        revision: revision.to_string(),
        documents: documents
            .iter()
            .map(|d| (d.path.clone(), (d.file_id.clone(), d.blob.clone())))
            .collect(),
    };
    if documents.len() != manifest.documents.len() {
        return Err("duplicate managed index path".into());
    }
    // Opening/searching may reveal corrupt segments that opening metadata alone
    // cannot detect. Retry once from authoritative blobs, with no stale fallback.
    match run(&dir, &manifest, allowed, query, &mut load, false) {
        Ok(result) => Ok(result),
        Err(_) => run(&dir, &manifest, allowed, query, &mut load, true),
    }
}
fn run(
    dir: &Path,
    wanted: &Manifest,
    allowed: &BTreeSet<String>,
    managed: ManagedQuery,
    load: &mut impl FnMut(&str) -> Result<String, String>,
    rebuild: bool,
) -> Result<ManagedSearch, String> {
    let query = managed.text;
    let result = (|| -> Result<ManagedSearch, Box<dyn std::error::Error>> {
        if rebuild && dir.exists() {
            std::fs::remove_dir_all(dir)?;
        }
        std::fs::create_dir_all(dir)?;
        let (schema, f) = schema();
        let existed = dir.join("meta.json").exists();
        let index = if existed {
            Index::open_in_dir(dir)?
        } else {
            Index::create_in_dir(dir, schema.clone())?
        };
        if index.schema() != schema {
            return Err("managed index schema changed".into());
        }
        register(&index);
        let previous: Option<Manifest> = index
            .load_metas()?
            .payload
            .map(|p| serde_json::from_str(&p))
            .transpose()?;
        if previous
            .as_ref()
            .is_some_and(|m| m.version != wanted.version)
        {
            return Err("managed index version changed".into());
        }
        if existed && previous.is_none() {
            return Err("managed index has no committed manifest".into());
        }
        let update = if !existed {
            "rebuilt"
        } else if previous.as_ref() == Some(wanted) {
            "current"
        } else {
            "incremental"
        };
        if previous.as_ref() != Some(wanted) {
            let mut writer = index.writer_with_num_threads::<TantivyDocument>(1, 20_000_000)?;
            if let Some(old) = &previous {
                for path in old
                    .documents
                    .keys()
                    .filter(|p| !wanted.documents.contains_key(*p))
                {
                    writer.delete_term(Term::from_field_text(f.path, path));
                }
            }
            for (path, record) in &wanted.documents {
                if previous.as_ref().and_then(|m| m.documents.get(path)) == Some(record) {
                    continue;
                }
                let body = load(&record.1).map_err(std::io::Error::other)?;
                if format!("{:x}", Sha256::digest(body.as_bytes())) != record.1 {
                    return Err("managed index source hash mismatch".into());
                }
                let title = body
                    .lines()
                    .find_map(|line| {
                        line.strip_prefix('#')
                            .map(|s| s.trim_start_matches('#').trim())
                    })
                    .filter(|s| !s.is_empty())
                    .unwrap_or_else(|| {
                        // Managed extraction segments use a namespace impossible for
                        // canonical source paths. Only the source filename contributes
                        // fallback title terms, never the private segment identifier.
                        source_path(path)
                    });
                let normalized = normalize(&body);
                let title = normalize(title);
                let relevance = StoredRelevance::new(RelevanceStats::new(&body, path, &record.1))?;
                writer.delete_term(Term::from_field_text(f.path, path));
                writer.add_document(doc!(f.path=>path.as_str(), f.body=>body.as_str(), f.en=>normalized.as_str(), f.ru=>normalized.as_str(), f.en_title=>title.as_str(), f.ru_title=>title.as_str(), f.relevance=>serde_json::to_string(&relevance)?))?;
            }
            failpoint("before_commit");
            let mut commit = writer.prepare_commit()?;
            commit.set_payload(&serde_json::to_string(wanted)?);
            commit.commit()?;
            failpoint("after_commit");
            writer.wait_merging_threads()?;
        }
        let reader = index.reader()?;
        let searcher = reader.searcher();
        if searcher.num_docs() != wanted.documents.len() as u64 {
            return Err("managed index document count mismatch".into());
        }
        let filter = || {
            TermSetQuery::new(
                allowed
                    .iter()
                    .filter(|p| wanted.documents.contains_key(*p))
                    .map(|p| Term::from_field_text(f.path, p)),
            )
        };
        let mut plain = TextAnalyzer::builder(SimpleTokenizer::default())
            .filter(LowerCaser)
            .build();
        let normalized = normalize(query);
        let mut stream = plain.token_stream(&normalized);
        let mut clauses: Vec<(Occur, Box<dyn Query>)> = Vec::new();
        // The same words, each also matching a term one edit away when it is long enough.
        let mut near_clauses: Vec<(Occur, Box<dyn Query>)> = Vec::new();
        let mut forgiving_any = false;
        let mut raws = Vec::new();
        while stream.advance() {
            let raw = &stream.token().text;
            raws.push(raw.clone());
            let forgiving = managed.typos && raw.chars().count() >= MIN_TYPO_CHARS;
            forgiving_any |= forgiving;
            let mut alternatives: Vec<(Occur, Box<dyn Query>)> = Vec::new();
            let mut near: Vec<(Occur, Box<dyn Query>)> = Vec::new();
            for (language, field, title_field) in [
                (Language::English, f.en, f.en_title),
                (Language::Russian, f.ru, f.ru_title),
            ] {
                for term in tokens(raw, language) {
                    alternatives.push((
                        Occur::Should,
                        Box::new(TermQuery::new(
                            Term::from_field_text(field, &term),
                            IndexRecordOption::WithFreqs,
                        )),
                    ));
                    alternatives.push((
                        Occur::Should,
                        Box::new(BoostQuery::new(
                            Box::new(TermQuery::new(
                                Term::from_field_text(title_field, &term),
                                IndexRecordOption::WithFreqs,
                            )),
                            2.0,
                        )),
                    ));
                    if forgiving {
                        for field in [field, title_field] {
                            near.push((
                                Occur::Should,
                                Box::new(FuzzyTermQuery::new(
                                    Term::from_field_text(field, &term),
                                    1,
                                    true,
                                )),
                            ));
                        }
                    }
                }
            }
            near.extend(alternatives.iter().map(|(o, q)| (*o, q.box_clone())));
            near_clauses.push((Occur::Must, Box::new(BooleanQuery::new(near))));
            clauses.push((Occur::Must, Box::new(BooleanQuery::new(alternatives))));
        }
        if clauses.is_empty() {
            return Err("search query has no lexical tokens".into());
        }
        for clauses in [&mut clauses, &mut near_clauses] {
            clauses.push((
                Occur::Must,
                Box::new(ConstScoreQuery::new(Box::new(filter()), 0.0)),
            ));
        }
        // Defense in depth; never trust a cache as an authorization source.
        let stored = |addr,
                      near: bool|
         -> Result<
            (String, String, Option<usize>, f32),
            Box<dyn std::error::Error>,
        > {
            let doc: TantivyDocument = searcher.doc(addr)?;
            let path = doc
                .get_first(f.path)
                .and_then(|v| v.as_str())
                .ok_or("missing indexed path")?;
            if !allowed.contains(path) || !wanted.documents.contains_key(path) {
                return Err("index authorization invariant failed".into());
            }
            let body = doc
                .get_first(f.body)
                .and_then(|v| v.as_str())
                .ok_or("missing indexed body")?;
            if format!("{:x}", Sha256::digest(body.as_bytes())) != wanted.documents[path].1 {
                return Err("managed index stored body hash mismatch".into());
            }
            let stored_relevance: StoredRelevance = serde_json::from_str(
                doc.get_first(f.relevance)
                    .and_then(|v| v.as_str())
                    .ok_or("missing indexed relevance statistics")?,
            )?;
            let relevance = stored_relevance.validate()?;
            if relevance.blob != wanted.documents[path].1 || relevance.path != path {
                return Err("managed index relevance provenance mismatch".into());
            }
            let score = if managed.body {
                relevance.score(query)
            } else {
                0.0
            };
            let (snippet, byte_start) = snippet(body, query, near);
            Ok((path.to_string(), snippet, byte_start, score))
        };
        let named: BTreeSet<&str> = if managed.name {
            allowed
                .iter()
                .filter(|p| wanted.documents.contains_key(*p) && name_matches(p, &raws))
                .map(String::as_str)
                .collect()
        } else {
            BTreeSet::new()
        };
        let boost = |path: &str| {
            if named.contains(path) {
                NAME_BOOST
            } else {
                0.0
            }
        };
        let mut hits = Vec::new();
        let mut seen = BTreeSet::new();
        if managed.body {
            let ranked = searcher.search(
                &ConstScoreQuery::new(Box::new(BooleanQuery::new(clauses)), 1.0),
                &TopDocs::with_limit(allowed.len().max(1)).order_by_score(),
            )?;
            for (_, addr) in ranked {
                let (path, snippet, byte_start, score) = stored(addr, false)?;
                seen.insert(path.clone());
                hits.push(ManagedHit {
                    score: score + boost(&path),
                    path,
                    snippet,
                    byte_start,
                    fuzzy: false,
                });
            }
            // Documents that need an edit; a name hit matched every word exactly.
            if forgiving_any {
                let ranked = searcher.search(
                    &ConstScoreQuery::new(Box::new(BooleanQuery::new(near_clauses)), 1.0),
                    &TopDocs::with_limit(allowed.len().max(1)).order_by_score(),
                )?;
                for (_, addr) in ranked {
                    let path = searcher
                        .doc::<TantivyDocument>(addr)?
                        .get_first(f.path)
                        .and_then(|v| v.as_str())
                        .map(str::to_owned)
                        .ok_or("missing indexed path")?;
                    if seen.contains(&path) {
                        continue;
                    }
                    let fuzzy = !named.contains(path.as_str());
                    let (path, snippet, byte_start, score) = stored(addr, fuzzy)?;
                    seen.insert(path.clone());
                    hits.push(ManagedHit {
                        score: score + boost(&path),
                        path,
                        snippet,
                        byte_start,
                        fuzzy,
                    });
                }
            }
        }
        // Name hits without a body hit; of an extracted source, only its first segment.
        for path in named {
            if seen.contains(path) || !first_segment(path) {
                continue;
            }
            let found = searcher.search(
                &BooleanQuery::new(vec![
                    (
                        Occur::Must,
                        Box::new(TermQuery::new(
                            Term::from_field_text(f.path, path),
                            IndexRecordOption::Basic,
                        )) as Box<dyn Query>,
                    ),
                    (Occur::Must, Box::new(filter())),
                ]),
                &TopDocs::with_limit(1).order_by_score(),
            )?;
            let (_, addr) = found
                .first()
                .ok_or("managed index missing named document")?;
            let (path, snippet, byte_start, score) = stored(*addr, false)?;
            hits.push(ManagedHit {
                score: score + NAME_BOOST,
                path,
                snippet,
                byte_start,
                fuzzy: false,
            });
        }
        // Exact before fuzzy; the keyword score, then the path, within each group.
        hits.sort_by(|a, b| {
            a.fuzzy
                .cmp(&b.fuzzy)
                .then_with(|| b.score.total_cmp(&a.score))
                .then_with(|| a.path.cmp(&b.path))
        });
        Ok(ManagedSearch {
            hits,
            indexed_revision: wanted.revision.clone(),
            update: update.into(),
        })
    })();
    result.map_err(|e| e.to_string())
}
/// Every query term, stemmed as either language, is a word of the source file name
/// (its basename without the extension).
fn name_matches(path: &str, raws: &[String]) -> bool {
    let base = source_path(path).rsplit('/').next().unwrap_or("");
    let stem = match base.rsplit_once('.') {
        Some((stem, _)) if !stem.is_empty() => stem,
        _ => base,
    };
    let stem = normalize(stem);
    let words: Vec<BTreeSet<String>> = [Language::English, Language::Russian]
        .into_iter()
        .map(|l| tokens(&stem, l).into_iter().collect())
        .collect();
    !raws.is_empty()
        && raws.iter().all(|raw| {
            [Language::English, Language::Russian]
                .into_iter()
                .zip(&words)
                .any(|(l, words)| {
                    let terms = tokens(raw, l);
                    !terms.is_empty() && terms.iter().all(|t| words.contains(t))
                })
        })
}
/// A canonical path, or the first segment of an extracted source.
fn first_segment(path: &str) -> bool {
    !path.starts_with('\0') || path.rsplit('\0').next() == Some("0")
}
/// The snippet around the first matching word, and that word's byte offset. With
/// `near`, a text without one is anchored on the first word one edit from a query word
/// long enough to be forgiven.
fn snippet(body: &str, query: &str, near: bool) -> (String, Option<usize>) {
    let query_tokens: BTreeSet<String> = [Language::English, Language::Russian]
        .into_iter()
        .flat_map(|l| tokens(&normalize(query), l))
        .collect();
    let near_terms: Vec<Vec<char>> = if near {
        let mut plain = TextAnalyzer::builder(SimpleTokenizer::default())
            .filter(LowerCaser)
            .build();
        let normalized = normalize(query);
        let mut stream = plain.token_stream(&normalized);
        let mut raws = Vec::new();
        while stream.advance() {
            if stream.token().text.chars().count() >= MIN_TYPO_CHARS {
                raws.push(stream.token().text.clone());
            }
        }
        raws.iter()
            .flat_map(|raw| {
                [Language::English, Language::Russian]
                    .into_iter()
                    .flat_map(move |l| tokens(raw, l))
            })
            .map(|t| t.chars().collect())
            .collect()
    } else {
        vec![]
    };
    let mut near_start = None;
    let chars: Vec<char> = body.chars().collect();
    let mut anchor = 0;
    let mut byte_start = None;
    // Find a lexical match while retaining offsets in the original UTF-8 text.
    let mut a = TextAnalyzer::builder(SimpleTokenizer::default())
        .filter(LowerCaser)
        .build();
    let mut stream = a.token_stream(body);
    while stream.advance() {
        let token = stream.token();
        let stems: Vec<String> = [Language::English, Language::Russian]
            .into_iter()
            .flat_map(|l| tokens(&normalize(&token.text), l))
            .collect();
        if stems.iter().any(|t| query_tokens.contains(t)) {
            anchor = body[..token.offset_from].chars().count();
            byte_start = Some(token.offset_from);
            break;
        }
        if near_start.is_none()
            && stems.iter().any(|t| {
                let t: Vec<char> = t.chars().collect();
                near_terms
                    .iter()
                    .any(|q| crate::fuzzy::osa_within_one(q, &t))
            })
        {
            near_start = Some(token.offset_from);
        }
    }
    if let (None, Some(offset)) = (byte_start, near_start) {
        anchor = body[..offset].chars().count();
        byte_start = Some(offset);
    }
    let start = anchor.saturating_sub(60);
    let end = (start + 240).min(chars.len());
    let snippet = format!(
        "{}{}{}",
        if start > 0 { "…" } else { "" },
        chars[start..end].iter().collect::<String>(),
        if end < chars.len() { "…" } else { "" }
    );
    (snippet, byte_start)
}

// A fixed reference length avoids corpus statistics, so adding/editing denied
// content cannot alter a visible score. TF saturation rewards useful matches
// without allowing repeated keywords to grow relevance without bound.
fn source_path(path: &str) -> &str {
    path.strip_prefix('\0')
        .and_then(|p| p.split_once('\0').map(|(source, _)| source))
        .unwrap_or(path)
}
// These document-local signals are computed only when exact content/path changes.
// Versioned derived fields are rebuilt from authoritative blobs, never used as
// authorization data. Keeping raw counts preserves the original f32 arithmetic.
#[derive(Serialize, Deserialize)]
struct TokenCounts {
    len: usize,
    frequencies: BTreeMap<String, usize>,
}
impl TokenCounts {
    fn new(text: &str, language: Language) -> Self {
        let tokens = tokens(text, language);
        let mut frequencies = BTreeMap::new();
        for token in &tokens {
            *frequencies.entry(token.clone()).or_default() += 1;
        }
        Self {
            len: tokens.len(),
            frequencies,
        }
    }
    fn count(&self, term: &str) -> usize {
        self.frequencies.get(term).copied().unwrap_or(0)
    }
}
#[derive(Serialize, Deserialize)]
struct RelevanceStats {
    blob: String,
    path: String,
    en: TokenCounts,
    ru: TokenCounts,
    en_title: TokenCounts,
    ru_title: TokenCounts,
}
#[derive(Serialize, Deserialize)]
struct StoredRelevance {
    statistics: RelevanceStats,
    checksum: String,
}
impl StoredRelevance {
    fn new(statistics: RelevanceStats) -> Result<Self, serde_json::Error> {
        let checksum = format!("{:x}", Sha256::digest(serde_json::to_vec(&statistics)?));
        Ok(Self {
            statistics,
            checksum,
        })
    }
    fn validate(&self) -> Result<&RelevanceStats, Box<dyn std::error::Error>> {
        let checksum = format!(
            "{:x}",
            Sha256::digest(serde_json::to_vec(&self.statistics)?)
        );
        if checksum != self.checksum {
            return Err("managed index relevance checksum mismatch".into());
        }
        for counts in [
            &self.statistics.en,
            &self.statistics.ru,
            &self.statistics.en_title,
            &self.statistics.ru_title,
        ] {
            if counts
                .frequencies
                .values()
                .try_fold(0usize, |sum, count| sum.checked_add(*count))
                != Some(counts.len)
            {
                return Err("managed index relevance length mismatch".into());
            }
        }
        Ok(&self.statistics)
    }
}
impl RelevanceStats {
    fn new(body: &str, path: &str, blob: &str) -> Self {
        let title = body
            .lines()
            .find_map(|line| {
                line.strip_prefix('#')
                    .map(|s| s.trim_start_matches('#').trim())
            })
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| source_path(path));
        let title = normalize(title);
        let body = normalize(body);
        Self {
            blob: blob.into(),
            path: path.into(),
            en: TokenCounts::new(&body, Language::English),
            ru: TokenCounts::new(&body, Language::Russian),
            en_title: TokenCounts::new(&title, Language::English),
            ru_title: TokenCounts::new(&title, Language::Russian),
        }
    }
    fn score(&self, query: &str) -> f32 {
        let mut plain = TextAnalyzer::builder(SimpleTokenizer::default())
            .filter(LowerCaser)
            .build();
        let normalized = normalize(query);
        let mut stream = plain.token_stream(&normalized);
        let mut terms = BTreeSet::new();
        while stream.advance() {
            terms.insert(stream.token().text.clone());
        }
        let tf_score = |count: usize, len: usize| {
            let tf = count as f32;
            tf / (tf + 1.2 * (0.25 + 0.75 * len as f32 / 100.0))
        };
        // Preserve language order, term ordering, and every f32 operation from
        // the original per-request scorer (including duplicate-query removal).
        terms
            .iter()
            .map(|raw| {
                [
                    (Language::English, &self.en, &self.en_title),
                    (Language::Russian, &self.ru, &self.ru_title),
                ]
                .into_iter()
                .map(|(language, body, title)| {
                    tokens(raw, language)
                        .iter()
                        .map(|term| {
                            tf_score(body.count(term), body.len)
                                + 2.0 * tf_score(title.count(term), title.len)
                        })
                        .sum::<f32>()
                })
                .fold(0.0_f32, f32::max)
            })
            .sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    #[test]
    fn snippet_reports_the_byte_offset_of_the_first_matching_word() {
        let body = "# Гарантия\n\nThe warranty lasts.";
        assert_eq!(snippet(body, "warranty", false).1, body.find("warranty"));
        assert_eq!(snippet(body, "гарантии", false).1, Some(2));
        assert_eq!(snippet(body, "absent", false).1, None);
        assert_eq!(snippet(body, "absent", false).0, body);
        // A fuzzy hit is anchored on the near word; an exact one never is.
        assert_eq!(snippet(body, "warrnaty", true).1, body.find("warranty"));
        assert_eq!(snippet(body, "warrnaty", false).1, None);
        assert_eq!(snippet(body, "wrn", true).1, None);
    }
    struct Temp(std::path::PathBuf);
    impl Temp {
        fn new() -> Self {
            Self(std::env::temp_dir().join(format!(
                "mnemosyne-managed-index-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            )))
        }
    }
    impl Drop for Temp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    fn document(path: &str, body: &str) -> ManagedDocument {
        ManagedDocument {
            path: path.into(),
            file_id: path.into(),
            blob: format!("{:x}", Sha256::digest(body.as_bytes())),
        }
    }
    #[test]
    fn incremental_cache_loads_only_changed_blobs_and_rebuilds_corruption() {
        let t = Temp::new();
        let mut doc = document("docs/a.md", "# Running\nМы разрабатываем поисковые системы");
        let allowed = BTreeSet::from([doc.path.clone()]);
        let first = sync_search(
            &t.0,
            "tenant",
            "r1",
            &[doc.clone()],
            &allowed,
            "run системы",
            |_| Ok("# Running\nМы разрабатываем поисковые системы".into()),
        )
        .unwrap();
        assert_eq!(first.update, "rebuilt");
        assert_eq!(first.hits.len(), 1);
        let current = sync_search(
            &t.0,
            "tenant",
            "r1",
            &[doc.clone()],
            &allowed,
            "run система",
            |_| panic!("unchanged source must not be read"),
        )
        .unwrap();
        assert_eq!(current.update, "current");
        assert_eq!(current.hits.len(), 1);
        doc.blob = format!("{:x}", Sha256::digest(b"updated only"));
        let changed = sync_search(
            &t.0,
            "tenant",
            "r2",
            &[doc.clone()],
            &allowed,
            "updated",
            |_| Ok("updated only".into()),
        )
        .unwrap();
        assert_eq!(changed.update, "incremental");
        assert_eq!(changed.hits.len(), 1);
        let dir =
            t.0.join("search-v1")
                .join(format!("{:x}", Sha256::digest(b"tenant")));
        std::fs::write(dir.join("meta.json"), "broken").unwrap();
        let recovered = sync_search(&t.0, "tenant", "r2", &[doc], &allowed, "updated", |_| {
            Ok("updated only".into())
        })
        .unwrap();
        assert_eq!(recovered.update, "rebuilt");
        assert_eq!(recovered.hits.len(), 1);
        let removed = sync_search(&t.0, "tenant", "r3", &[], &allowed, "updated", |_| {
            panic!("deleted document must not be read")
        })
        .unwrap();
        assert!(removed.hits.is_empty());
    }
    #[test]
    fn extraction_segment_keys_keep_source_titles_and_cannot_match_segment_numbers() {
        let t = Temp::new();
        let segment = document("\0docs/source.pdf\042", "shared searchable text");
        let real = document("docs/source.pdf", "shared searchable text");
        let docs = vec![segment.clone(), real.clone()];
        let allowed = docs.iter().map(|d| d.path.clone()).collect();
        let hits = sync_search(&t.0, "tenant", "r1", &docs, &allowed, "source", |_| {
            Ok("shared searchable text".into())
        })
        .unwrap();
        assert_eq!(hits.hits.len(), 2);
        assert_eq!(hits.hits[0].score, hits.hits[1].score);
        assert_ne!(hits.hits[0].path, hits.hits[1].path);
        let absent = sync_search(&t.0, "tenant", "r1", &docs, &allowed, "42", |_| {
            panic!("cache is current")
        })
        .unwrap();
        assert!(absent.hits.is_empty());
    }
    #[test]
    fn failed_source_update_never_serves_old_cache() {
        let t = Temp::new();
        let old = document("a.md", "oldtoken");
        let allowed = BTreeSet::from([old.path.clone()]);
        sync_search(&t.0, "tenant", "r1", &[old], &allowed, "oldtoken", |_| {
            Ok("oldtoken".into())
        })
        .unwrap();
        let new = document("a.md", "newtoken");
        assert!(
            sync_search(&t.0, "tenant", "r2", &[new], &allowed, "oldtoken", |_| Err(
                "missing source".into()
            ))
            .is_err()
        );
    }
    #[test]
    fn persisted_statistics_preserve_reference_score_bits() {
        let long = "Running поисковые системы ёлки ".repeat(500);
        let overlong = "x".repeat(81);
        let boundary = "я".repeat(40);
        let bodies = vec![
            "# Running searches\nRUN run running searches dogs dogs".into(),
            "### Ёлки и системы\nёж ёлка ЕЛКА собаками поисковых систем".into(),
            "before heading\n# \n# Ignored later title\nsearch running".into(),
            "no heading; résumé CAFÉ İSTANBUL 中文 e\u{301} 😀 runs".into(),
            String::new(),
            long,
            format!("# {boundary}\n{overlong} {boundary}"),
        ];
        for path in ["Running-system.md", "\0docs/Ёлка.pdf\042", "docs/中文.txt"] {
            for body in &bodies {
                let blob = format!("{:x}", Sha256::digest(body.as_bytes()));
                let stats = RelevanceStats::new(body, path, &blob);
                let restored: RelevanceStats =
                    serde_json::from_str(&serde_json::to_string(&stats).unwrap()).unwrap();
                for query in [
                    "running search",
                    "search search SEARCH",
                    "ёлка ЕЛКА",
                    "собака системы",
                    "42",
                    "résumé CAFÉ 中文",
                    "İSTANBUL e\u{301}",
                    "absent",
                    "",
                    &overlong,
                    &boundary,
                ] {
                    assert_eq!(
                        restored.score(query).to_bits(),
                        reference_relevance(body, path, query).to_bits(),
                        "path={path:?} query={query:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn old_version_and_corrupt_statistics_rebuild_from_authority() {
        let t = Temp::new();
        let body = "# Running\nsearch searches поисковые системы";
        let document = document("a.md", body);
        let docs = [document.clone()];
        let allowed = BTreeSet::from([document.path.clone()]);
        let expected = sync_search(&t.0, "tenant", "r1", &docs, &allowed, "search", |_| {
            Ok(body.into())
        })
        .unwrap();
        let dir =
            t.0.join("search-v1")
                .join(format!("{:x}", Sha256::digest(b"tenant")));
        for corruption in 0..4 {
            {
                let index = Index::open_in_dir(&dir).unwrap();
                register(&index);
                let (_, fields) = schema();
                let mut manifest: Manifest =
                    serde_json::from_str(index.load_metas().unwrap().payload.as_deref().unwrap())
                        .unwrap();
                let mut writer = index
                    .writer_with_num_threads::<TantivyDocument>(1, 20_000_000)
                    .unwrap();
                if corruption != 0 {
                    let mut stored =
                        StoredRelevance::new(RelevanceStats::new(body, "a.md", &document.blob))
                            .unwrap();
                    let serialized = match corruption {
                        1 => "invalid statistics".into(),
                        2 => {
                            stored.statistics.en.len += 1;
                            serde_json::to_string(&stored).unwrap()
                        }
                        _ => {
                            stored.statistics.path = "other.md".into();
                            serde_json::to_string(&StoredRelevance::new(stored.statistics).unwrap())
                                .unwrap()
                        }
                    };
                    writer.delete_all_documents().unwrap();
                    writer
                        .add_document(doc!(fields.path=>"a.md", fields.body=>body,
                        fields.en=>normalize(body), fields.ru=>normalize(body),
                        fields.relevance=>serialized))
                        .unwrap();
                } else {
                    manifest.version = 2;
                }
                let mut commit = writer.prepare_commit().unwrap();
                commit.set_payload(&serde_json::to_string(&manifest).unwrap());
                commit.commit().unwrap();
                writer.wait_merging_threads().unwrap();
            }
            let mut loads = 0;
            let rebuilt = sync_search(&t.0, "tenant", "r1", &docs, &allowed, "search", |_| {
                loads += 1;
                Ok(body.into())
            })
            .unwrap();
            assert_eq!(loads, 1);
            assert_eq!(rebuilt.update, "rebuilt");
            assert_eq!(rebuilt.hits.len(), expected.hits.len());
            assert_eq!(
                rebuilt.hits[0].score.to_bits(),
                expected.hits[0].score.to_bits()
            );
            assert_eq!(rebuilt.hits[0].snippet, expected.hits[0].snippet);
        }
    }

    #[test]
    fn name_hits_add_a_fixed_boost_and_need_every_term() {
        let raws = |q: &str| vec![q.to_string()];
        assert!(name_matches("docs/Warranty.md", &raws("warranty")));
        assert!(name_matches("warranties.md", &raws("warranty")));
        assert!(!name_matches("warranty/terms.md", &raws("warranty")));
        assert!(!name_matches("notes.md", &raws("md")));
        assert!(name_matches(
            concat!("\0docs/Warranty.pdf\0", "3"),
            &raws("warranty")
        ));
        assert!(!name_matches("a.md", &[]));
        assert!(first_segment("a.md") && first_segment(concat!("\0a.pdf\0", "0")));
        assert!(!first_segment(concat!("\0a.pdf\0", "10")));

        let t = Temp::new();
        let named = document("Warranty.md", "# Coverage\nplan");
        let body = document("b.md", "warranty text");
        let docs = vec![named.clone(), body.clone()];
        let allowed = docs.iter().map(|d| d.path.clone()).collect();
        let bodies = |blob: &str| {
            Ok(if blob == named.blob {
                "# Coverage\nplan".into()
            } else {
                "warranty text".into()
            })
        };
        let run = |body: bool, name: bool| {
            let query = ManagedQuery {
                text: "warranty",
                body,
                name,
                typos: false,
            };
            sync_search_fields(&t.0, "tenant", "r1", &docs, &allowed, query, bodies)
                .unwrap()
                .hits
                .into_iter()
                .map(|h| (h.path, h.score))
                .collect::<Vec<_>>()
        };
        let only_body = run(true, false);
        assert_eq!(only_body.len(), 1);
        assert_eq!(
            run(true, true),
            vec![("Warranty.md".into(), NAME_BOOST), only_body[0].clone()]
        );
        assert_eq!(run(false, true), vec![("Warranty.md".into(), NAME_BOOST)]);
    }

    #[test]
    fn fuzzy_terms_rank_after_every_exact_document() {
        let t = Temp::new();
        let bodies = [
            ("exact.md", "the warranty lasts"),
            ("near.md", "a warrnaty typo and a warrnaty again"),
            ("cat.md", "the cat sat"),
        ];
        let docs: Vec<_> = bodies.iter().map(|(p, b)| document(p, b)).collect();
        let allowed = docs.iter().map(|d| d.path.clone()).collect();
        let load = |blob: &str| {
            Ok(bodies
                .iter()
                .find(|(_, b)| format!("{:x}", Sha256::digest(b)) == blob)
                .unwrap()
                .1
                .to_string())
        };
        let run = |text: &str, typos: bool| {
            let query = ManagedQuery {
                text,
                body: true,
                name: true,
                typos,
            };
            sync_search_fields(&t.0, "tenant", "r1", &docs, &allowed, query, load)
                .unwrap()
                .hits
                .into_iter()
                .map(|h| (h.path, h.fuzzy))
                .collect::<Vec<_>>()
        };
        assert_eq!(
            run("warranty", true),
            vec![("exact.md".into(), false), ("near.md".into(), true)]
        );
        // The near document scores higher on its own exact word, yet stays first.
        assert_eq!(
            run("warrnaty", true),
            vec![("near.md".into(), false), ("exact.md".into(), true)]
        );
        assert_eq!(run("warranty", false), vec![("exact.md".into(), false)]);
        assert!(run("cta", true).is_empty());
        // Every word must match, exactly or with an edit.
        assert_eq!(run("warrnaty lasts", true), vec![("exact.md".into(), true)]);
    }

    fn reference_relevance(body: &str, path: &str, query: &str) -> f32 {
        let title = body
            .lines()
            .find_map(|line| {
                line.strip_prefix('#')
                    .map(|s| s.trim_start_matches('#').trim())
            })
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| source_path(path));
        let body = normalize(body);
        let title = normalize(title);
        let mut plain = TextAnalyzer::builder(SimpleTokenizer::default())
            .filter(LowerCaser)
            .build();
        let normalized = normalize(query);
        let mut stream = plain.token_stream(&normalized);
        let mut terms = BTreeSet::new();
        while stream.advance() {
            terms.insert(stream.token().text.clone());
        }
        let en = tokens(&body, Language::English);
        let ru = tokens(&body, Language::Russian);
        let en_title = tokens(&title, Language::English);
        let ru_title = tokens(&title, Language::Russian);
        let tf_score = |count: usize, len: usize| {
            let tf = count as f32;
            tf / (tf + 1.2 * (0.25 + 0.75 * len as f32 / 100.0))
        };
        terms
            .iter()
            .map(|raw| {
                [
                    (Language::English, &en, &en_title),
                    (Language::Russian, &ru, &ru_title),
                ]
                .into_iter()
                .map(|(language, body_tokens, title_tokens)| {
                    tokens(raw, language)
                        .iter()
                        .map(|term| {
                            tf_score(
                                body_tokens.iter().filter(|s| *s == term).count(),
                                body_tokens.len(),
                            ) + 2.0
                                * tf_score(
                                    title_tokens.iter().filter(|s| *s == term).count(),
                                    title_tokens.len(),
                                )
                        })
                        .sum::<f32>()
                })
                .fold(0.0_f32, f32::max)
            })
            .sum()
    }
}
