//! The tantivy-backed search index.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use mnemosyne_core::{Note, Vault};
use tantivy::collector::TopDocs;
use tantivy::query::QueryParser;
use tantivy::schema::Value;
use tantivy::schema::{Field, Schema, STORED, STRING, TEXT};
use tantivy::{doc, Index, TantivyDocument, Term};

/// Errors from indexing and searching.
#[derive(Debug, thiserror::Error)]
pub enum IndexError {
    #[error("tantivy error: {0}")]
    Tantivy(#[from] tantivy::TantivyError),
    #[error("index schema is missing required field: {0}")]
    MissingField(String),
    #[error("query parse error: {0}")]
    Query(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

/// A single search hit.
#[derive(Debug, Clone, serde::Serialize)]
pub struct SearchResult {
    /// Vault-relative note path.
    pub path: String,
    /// Note title.
    pub title: String,
    /// Relevance score.
    pub score: f32,
    /// A short excerpt around the first matching term.
    pub snippet: String,
}

struct Fields {
    path: Field,
    title: Field,
    body: Field,
    tags: Field,
}

impl Fields {
    fn resolve(schema: &Schema) -> Result<Fields, IndexError> {
        Ok(Fields {
            path: schema
                .get_field("path")
                .map_err(|_| IndexError::MissingField("path".into()))?,
            title: schema
                .get_field("title")
                .map_err(|_| IndexError::MissingField("title".into()))?,
            body: schema
                .get_field("body")
                .map_err(|_| IndexError::MissingField("body".into()))?,
            tags: schema
                .get_field("tags")
                .map_err(|_| IndexError::MissingField("tags".into()))?,
        })
    }
}

fn build_schema() -> Schema {
    let mut builder = Schema::builder();
    builder.add_text_field("path", STRING | STORED);
    builder.add_text_field("title", TEXT | STORED);
    builder.add_text_field("body", TEXT | STORED);
    builder.add_text_field("tags", TEXT | STORED);
    builder.build()
}

/// A persistent full-text index over a vault.
pub struct SearchIndex {
    index: Index,
    fields: Fields,
    dir: PathBuf,
    hashes: BTreeMap<String, u64>,
}

impl SearchIndex {
    /// Opens the index at `dir`, creating it (empty) if it does not exist.
    ///
    /// Use [`SearchIndex::rebuild`] to populate the index.
    pub fn open_or_create(dir: &Path) -> Result<SearchIndex, IndexError> {
        std::fs::create_dir_all(dir)?;
        let index = if dir.join("meta.json").exists() {
            Index::open_in_dir(dir)?
        } else {
            Index::create_in_dir(dir, build_schema())?
        };
        let fields = Fields::resolve(&index.schema())?;
        let hashes = load_hashes(dir);
        Ok(SearchIndex {
            index,
            fields,
            dir: dir.to_path_buf(),
            hashes,
        })
    }

    /// Rebuilds the index from scratch, replacing any existing documents.
    pub fn rebuild(&mut self, vault: &Vault) -> Result<(), IndexError> {
        let mut writer = self.index.writer(50_000_000)?;
        writer.delete_all_documents()?;
        let mut hashes = BTreeMap::new();
        for (path, note) in &vault.notes {
            add_note(&self.fields, &mut writer, path, note)?;
            hashes.insert(path.clone(), note_hash(note));
        }
        writer.commit()?;
        save_hashes(&self.dir, &hashes)?;
        self.hashes = hashes;
        Ok(())
    }

    /// Updates only the notes whose contents changed (or were added/removed)
    /// since the last build.
    pub fn rebuild_incremental(&mut self, vault: &Vault) -> Result<(), IndexError> {
        let mut stored = std::mem::take(&mut self.hashes);
        if stored.is_empty() {
            return self.rebuild(vault);
        }
        let mut writer = self.index.writer(50_000_000)?;
        let mut changed = false;

        for (path, note) in &vault.notes {
            let h = note_hash(note);
            if stored.get(path) == Some(&h) {
                continue;
            }
            changed = true;
            writer.delete_term(Term::from_field_text(self.fields.path, path));
            add_note(&self.fields, &mut writer, path, note)?;
            stored.insert(path.clone(), h);
        }

        let removed: Vec<String> = stored
            .keys()
            .filter(|p| !vault.notes.contains_key(*p))
            .cloned()
            .collect();
        for path in &removed {
            changed = true;
            writer.delete_term(Term::from_field_text(self.fields.path, path));
            stored.remove(path);
        }

        if changed {
            writer.commit()?;
            save_hashes(&self.dir, &stored)?;
        }
        self.hashes = stored;
        Ok(())
    }

    /// Returns the number of indexed documents.
    pub fn doc_count(&self) -> u64 {
        self.index
            .reader()
            .map(|r| {
                let searcher = r.searcher();
                searcher.num_docs()
            })
            .unwrap_or(0)
    }

    /// Runs a full-text query over title, body, and tags.
    pub fn search(&self, query: &str, limit: usize) -> Result<Vec<SearchResult>, IndexError> {
        let reader = self.index.reader()?;
        let searcher = reader.searcher();

        let query_parser = QueryParser::for_index(
            &self.index,
            vec![self.fields.title, self.fields.body, self.fields.tags],
        );
        let parsed_query = query_parser
            .parse_query(query)
            .map_err(|e| IndexError::Query(format!("{e:?}")))?;

        let collector = TopDocs::with_limit(limit).order_by_score();
        let top_docs = searcher.search(&parsed_query, &collector)?;

        let mut results = Vec::with_capacity(top_docs.len());
        for (score, doc_addr) in top_docs {
            let retrieved: TantivyDocument = searcher.doc(doc_addr)?;
            let path = retrieved
                .get_first(self.fields.path)
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();
            let title = retrieved
                .get_first(self.fields.title)
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();
            let body = retrieved
                .get_first(self.fields.body)
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();

            results.push(SearchResult {
                path,
                title,
                score,
                snippet: snippet(&body, query),
            });
        }

        Ok(results)
    }
}

fn add_note(
    fields: &Fields,
    writer: &mut tantivy::IndexWriter,
    path: &str,
    note: &Note,
) -> Result<(), IndexError> {
    let tags = note.all_tags().join(" ");
    writer.add_document(doc!(
        fields.path => path,
        fields.title => note.title.as_str(),
        fields.body => note.body.as_str(),
        fields.tags => tags.as_str(),
    ))?;
    Ok(())
}

/// A stable (FNV-1a) content hash of a note's full source.
fn note_hash(note: &Note) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for &b in note.source.as_bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

fn hashes_path(dir: &Path) -> PathBuf {
    dir.join("hashes")
}

fn load_hashes(dir: &Path) -> BTreeMap<String, u64> {
    let Ok(content) = std::fs::read_to_string(hashes_path(dir)) else {
        return BTreeMap::new();
    };
    content
        .lines()
        .filter_map(|line| {
            let (p, h) = line.split_once('\t')?;
            Some((p.to_string(), u64::from_str_radix(h, 16).ok()?))
        })
        .collect()
}

fn save_hashes(dir: &Path, hashes: &BTreeMap<String, u64>) -> Result<(), IndexError> {
    let mut out = String::new();
    for (p, h) in hashes {
        out.push_str(p);
        out.push('\t');
        out.push_str(&format!("{h:016x}"));
        out.push('\n');
    }
    std::fs::write(hashes_path(dir), out)?;
    Ok(())
}

const SNIPPET_LEN: usize = 240;
/// Builds a short plain-text excerpt around the first query term found in the
/// body, using `…` for elisions.
fn snippet(body: &str, query: &str) -> String {
    let term = query.split_whitespace().next().unwrap_or("").to_lowercase();

    let anchor = term
        .is_empty()
        .then_some(0)
        .or_else(|| {
            body.to_lowercase()
                .find(&term)
                .map(|idx| {
                    // Map back to a byte index in the original text. The
                    // lowercased length can differ from the original for some
                    // characters, so clamp to a char boundary.
                    idx.min(body.len())
                })
                .map(|idx| body.floor_char_boundary(idx))
        })
        .unwrap_or(0);

    let start = body.floor_char_boundary(anchor.saturating_sub(SNIPPET_LEN / 4));
    let end = (anchor + SNIPPET_LEN).min(body.len());
    let end = body.ceil_char_boundary(end);

    let mut s = String::new();
    if start > 0 {
        s.push('…');
    }
    s.push_str(&body[start..end]);
    if end < body.len() {
        s.push('…');
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    struct TempDir(std::path::PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            let mut p = std::env::temp_dir();
            p.push(format!("mnemosyne-idx-{tag}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&p);
            fs::create_dir_all(&p).unwrap();
            TempDir(p)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn indexes_and_searches() {
        let t = TempDir::new("full");
        let vault_dir = t.0.join("vault");
        fs::create_dir_all(&vault_dir).unwrap();
        fs::write(
            vault_dir.join("alpha.md"),
            "---\ntags: [rust]\n---\n# Alpha\n\nA note about the tantivy search engine.\n",
        )
        .unwrap();
        fs::write(
            vault_dir.join("beta.md"),
            "# Beta\n\nTotally unrelated content.\n",
        )
        .unwrap();

        let vault = Vault::open(&vault_dir).unwrap();
        let idx_dir = t.0.join("idx");
        let mut index = SearchIndex::open_or_create(&idx_dir).unwrap();
        index.rebuild(&vault).unwrap();
        assert_eq!(index.doc_count(), 2);

        let hits = index.search("tantivy", 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].path, "alpha.md");

        let none = index.search("zzz-no-match", 10).unwrap();
        assert!(none.is_empty());
    }

    #[test]
    fn incremental_indexing() {
        let t = TempDir::new("incr");
        let vault_dir = t.0.join("vault");
        fs::create_dir_all(&vault_dir).unwrap();
        fs::write(vault_dir.join("a.md"), "alpha").unwrap();
        fs::write(vault_dir.join("b.md"), "beta").unwrap();

        let idx_dir = t.0.join("idx");
        let vault = Vault::open(&vault_dir).unwrap();
        let mut index = SearchIndex::open_or_create(&idx_dir).unwrap();
        index.rebuild(&vault).unwrap();
        assert_eq!(index.doc_count(), 2);

        // Modify a.md and add c.md, then reindex incrementally.
        fs::write(vault_dir.join("a.md"), "alpha updated").unwrap();
        fs::write(vault_dir.join("c.md"), "gamma").unwrap();
        let vault2 = Vault::open(&vault_dir).unwrap();
        index.rebuild_incremental(&vault2).unwrap();
        assert_eq!(index.doc_count(), 3);

        let hits = index.search("updated", 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].path, "a.md");
    }
}
