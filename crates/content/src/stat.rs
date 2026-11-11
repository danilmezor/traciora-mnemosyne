//! `stat`: what a file is and how big, without its text: mime, words, characters, tokens,
//! its colour and visibility, and who created and last changed it. Visibility is that of
//! `read`; counts a binary lacks (no usable extraction yet) are `null`, never zero.
use super::*;
use std::sync::OnceLock;

const TOKEN_MODEL: &str = "cl100k_base";

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Stamp {
    pub at: Option<String>,
    pub actor: TrustedIdentity,
    pub revision_id: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StatResult {
    pub path: String,
    pub revision_id: String,
    pub kind: EntryKind,
    pub mime: String,
    pub words: Option<u32>,
    pub characters: Option<u32>,
    pub tokens: Option<u32>,
    pub token_model: String,
    pub created: Option<Stamp>,
    pub changed: Option<Stamp>,
    pub seen_from: String,
    pub inherited_from: Option<String>,
    pub colour: String,
    pub colour_source: ranks::ColourSource,
}

pub(crate) fn mime_for(path: &str) -> &'static str {
    let Some((_, ext)) = path.rsplit_once('.') else {
        return "application/octet-stream";
    };
    match ext.to_ascii_lowercase().as_str() {
        "md" => "text/markdown",
        "txt" => "text/plain",
        "pdf" => "application/pdf",
        "docx" => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        "pptx" => "application/vnd.openxmlformats-officedocument.presentationml.presentation",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "svg" => "image/svg+xml",
        "py" => "text/x-python",
        "js" => "text/x-javascript",
        "ts" => "text/x-typescript",
        "rs" => "text/x-rust",
        "go" => "text/x-go",
        "json" => "text/x-json",
        "yaml" => "text/x-yaml",
        "toml" => "text/x-toml",
        _ => "application/octet-stream",
    }
}

/// Unicode words (runs of alphanumerics; an apostrophe or hyphen between alphanumerics
/// stays inside the run) and characters.
pub(crate) fn count_words(text: &str) -> (u32, u32) {
    let (mut words, mut in_word, mut joined) = (0u32, false, false);
    for c in text.chars() {
        if c.is_alphanumeric() {
            if !in_word {
                words = words.saturating_add(1);
            }
            (in_word, joined) = (true, false);
        } else if matches!(c, '\'' | '\u{2019}' | '-') && in_word && !joined {
            joined = true;
        } else {
            (in_word, joined) = (false, false);
        }
    }
    (
        words,
        u32::try_from(text.chars().count()).unwrap_or(u32::MAX),
    )
}

/// Longer texts are counted for words and characters but not tokenised.
const TOKEN_TEXT_LIMIT: usize = 4 * 1024 * 1024;

fn tokens(text: &str) -> Option<u32> {
    if text.len() > TOKEN_TEXT_LIMIT {
        return None;
    }
    static BPE: OnceLock<Option<tiktoken_rs::CoreBPE>> = OnceLock::new();
    let bpe = BPE
        .get_or_init(|| tiktoken_rs::cl100k_base().ok())
        .as_ref()?;
    u32::try_from(bpe.encode_ordinary(text).len()).ok()
}

struct Counts {
    words: Option<u32>,
    characters: Option<u32>,
    tokens: Option<u32>,
}
const NONE: Counts = Counts {
    words: None,
    characters: None,
    tokens: None,
};

/// What the authority pass found; the text is read and counted after it is released.
enum Source {
    None,
    Text(String),
    Binary { blob: String, format: &'static str },
}

struct Found {
    result: StatResult,
    file_id: String,
    ancestors: HashSet<String>,
    source: Source,
}

fn counts_of(text: &str) -> Counts {
    let (words, characters) = count_words(text);
    Counts {
        words: Some(words),
        characters: Some(characters),
        tokens: tokens(text),
    }
}

/// Counts of an extraction record's text (segments joined with a newline) when it
/// `succeeded` or was `partial`; `null` otherwise.
fn counts_from_record(record: &extraction::ExtractionRecord) -> Counts {
    if !matches!(
        record.status,
        extraction::ExtractionStatus::Succeeded | extraction::ExtractionStatus::Partial
    ) {
        return NONE;
    }
    let joined = record
        .segments
        .iter()
        .map(|s| s.text.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    counts_of(&joined)
}

impl Service {
    pub fn stat(
        &self,
        who: &TrustedIdentity,
        path: &str,
        revision: Option<&str>,
    ) -> Result<StatResult> {
        valid_path(path)?;
        // Metadata only: manifests for the head and `revision`, no revision text. Blobs
        // are immutable and content-addressed, so reading and counting after the lock is
        // released sees the same bytes.
        let found = self.transaction_metadata(
            who,
            revision.into_iter().map(str::to_owned).collect(),
            |s| {
                ranks::check(s, who, Permission::Read, path)?;
                let t = s
                    .tenants
                    .get(&who.tenant_id)
                    .ok_or_else(|| err("not_found"))?;
                let id = revision.unwrap_or(&t.head);
                let snap = t.revisions.get(id).ok_or_else(|| t.missing_revision(id))?;
                let entry = snap.files.get(path).ok_or_else(|| err("not_found"))?;
                authorize_identity(s, who, entry, path)?;
                ranks::check_at(s, who, Permission::Read, path, &snap.files)?;
                let source = match (&entry.kind, entry.blob.as_deref()) {
                    (EntryKind::Text, Some(blob)) => Source::Text(blob.into()),
                    (EntryKind::Text, None) => return Err(err("not_found")),
                    (EntryKind::Binary, Some(blob)) => Source::Binary {
                        blob: blob.into(),
                        format: extraction::format(path).unwrap_or("unsupported"),
                    },
                    _ => Source::None,
                };
                let tp = ranks::tenant_policy(s, &who.tenant_id);
                let (seen_from, inherited_from) = ranks::seen_from_source(&snap.files, &tp, path);
                let (colour, colour_source) =
                    ranks::effective_colour(&snap.files, &tp, &who.tenant_id, path);
                Ok(Found {
                    file_id: entry.file_id.clone(),
                    ancestors: chain(t, id).into_iter().collect(),
                    source,
                    result: StatResult {
                        path: path.into(),
                        revision_id: id.into(),
                        kind: entry.kind.clone(),
                        mime: if entry.kind == EntryKind::Folder {
                            "inode/directory"
                        } else {
                            mime_for(path)
                        }
                        .into(),
                        words: None,
                        characters: None,
                        tokens: None,
                        token_model: TOKEN_MODEL.into(),
                        created: None,
                        changed: None,
                        seen_from,
                        inherited_from,
                        colour,
                        colour_source,
                    },
                })
            },
        )?;
        let counts = match &found.source {
            Source::None => NONE,
            Source::Text(blob) => {
                let text = String::from_utf8(storage::read_blob(&self.root, blob)?).map_err(io)?;
                counts_of(&text)
            }
            Source::Binary { blob, format } => match extraction::cached(&self.root, blob, format) {
                Ok(Some(record)) => counts_from_record(&record),
                _ => NONE,
            },
        };
        // History of this file identity, newest first; only revisions up to the one asked.
        let stamp = |r: &Revision| Stamp {
            at: r.at.clone(),
            actor: r.actor.clone(),
            revision_id: r.id.clone(),
        };
        let history = self.history_by(who, path, Some(&found.file_id), true)?;
        let mut within = history.iter().filter(|r| found.ancestors.contains(&r.id));
        let changed = within.next().map(stamp);
        let created = within.next_back().map(stamp).or_else(|| changed.clone());
        Ok(StatResult {
            created,
            changed,
            words: counts.words,
            characters: counts.characters,
            tokens: counts.tokens,
            ..found.result
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extraction::{
        ExtractionRecord, ExtractionSegment, ExtractionStatus, SourceLocation,
    };

    fn record(status: ExtractionStatus, texts: &[&str]) -> ExtractionRecord {
        ExtractionRecord {
            source_blob: "b".into(),
            extractor: "x".into(),
            version: "1".into(),
            status,
            segments: texts
                .iter()
                .map(|t| ExtractionSegment {
                    text: (*t).into(),
                    location: SourceLocation {
                        page: None,
                        paragraph: None,
                        section: None,
                        slide: None,
                    },
                })
                .collect(),
            limits: vec![],
        }
    }

    #[test]
    fn counts_come_from_succeeded_and_partial_records_only() {
        for status in [ExtractionStatus::Succeeded, ExtractionStatus::Partial] {
            let c = counts_from_record(&record(status, &["one two", "three"]));
            // Segments are joined with a newline: "one two\nthree".
            assert_eq!((c.words, c.characters), (Some(3), Some(13)));
            assert!(c.tokens.is_some_and(|t| t > 0));
        }
        for status in [
            ExtractionStatus::Failed,
            ExtractionStatus::Pending,
            ExtractionStatus::Unsupported,
        ] {
            let c = counts_from_record(&record(status, &["one two"]));
            assert_eq!((c.words, c.characters, c.tokens), (None, None, None));
        }
    }
}
