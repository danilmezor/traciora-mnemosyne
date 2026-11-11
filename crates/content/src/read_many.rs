//! Several reads at one head revision under one byte budget. Each
//! item answers for itself; a path the caller may not read answers exactly as an absent
//! one. Binaries give their cached extracted text: `read_many` never runs the extractor.
use crate::extraction::{self, ExtractionStatus};
use crate::{
    auth::{Permission, TrustedIdentity},
    err, storage, ContentError, EntryKind, Result, Service,
};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

pub const MAX_PATHS: usize = 64;
pub const DEFAULT_MAX_BYTES: usize = 256 * 1024;
pub const MAX_MAX_BYTES: usize = 8 * 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ItemError {
    pub code: String,
    pub message: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ReadItem {
    pub path: String,
    pub ok: bool,
    /// `"text"` or `"extracted"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    /// Cut by the budget, at a character boundary.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub truncated: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<ItemError>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ReadManyResult {
    pub revision_id: String,
    pub items: Vec<ReadItem>,
    pub used_bytes: usize,
}

/// What one readable item holds, before the budget is spent on it.
enum Source {
    Text(String),
    Extracted(String),
}

fn failed(path: &str, e: ContentError) -> ReadItem {
    ReadItem {
        path: path.into(),
        ok: false,
        kind: None,
        content: None,
        truncated: false,
        error: Some(ItemError {
            code: e.code,
            message: e.message,
        }),
    }
}

fn invalid_request(message: &str) -> ContentError {
    ContentError::new("invalid_request", message)
}

impl Service {
    /// Up to 64 paths at head, in request order. `max_bytes` (1 B – 8 MiB) is spent on
    /// the contents in that order: the item that crosses it is cut at a character
    /// boundary (`truncated`), and every later readable item answers `max_bytes`.
    pub fn read_many(
        &self,
        who: &TrustedIdentity,
        paths: &[String],
        max_bytes: usize,
    ) -> Result<ReadManyResult> {
        if paths.is_empty() || paths.len() > MAX_PATHS {
            return Err(invalid_request("paths must hold 1 to 64 paths"));
        }
        let mut distinct = HashSet::new();
        if !paths.iter().all(|p| distinct.insert(p.as_str())) {
            return Err(invalid_request("paths must not repeat"));
        }
        if !(1..=MAX_MAX_BYTES).contains(&max_bytes) {
            return Err(invalid_request("max_bytes must be between 1 and 8388608"));
        }
        self.transaction_for(who, false, vec![], false, None, None, |s| {
            let revision_id = s
                .tenants
                .get(&who.tenant_id)
                .map(|t| t.head.as_str())
                .filter(|h| !h.is_empty())
                .unwrap_or("r0")
                .to_string();
            let files = crate::ranks::head_files(s, &who.tenant_id);
            let mut items = Vec::with_capacity(paths.len());
            let (mut used, mut spent) = (0usize, false);
            for path in paths {
                let source = (|| -> Result<Source> {
                    crate::valid_path(path)?;
                    let Some(entry) = files.get(path.as_str()) else {
                        // An implicit folder: readable only through a readable entry
                        // below it, as `graph::visible_among`; the first one decides.
                        let below = format!("{path}/");
                        let readable = |p: &str, e: &crate::Entry| {
                            crate::ranks::check_at(s, who, Permission::Read, p, &files).is_ok()
                                && crate::authorize_identity(s, who, e, p).is_ok()
                        };
                        let shown = crate::ranks::check_at(s, who, Permission::Read, path, &files)
                            .is_ok()
                            && files
                                .range::<str, _>((
                                    std::ops::Bound::Included(below.as_str()),
                                    std::ops::Bound::Unbounded,
                                ))
                                .take_while(|(p, _)| p.starts_with(&below))
                                .any(|(p, e)| readable(p, e));
                        return Err(err(if shown { "not_text" } else { "not_found" }));
                    };
                    // As `read`: the rank check and every name the identity has held;
                    // hidden answers exactly as absent.
                    if crate::ranks::check_at(s, who, Permission::Read, path, &files).is_err()
                        || crate::authorize_identity(s, who, entry, path).is_err()
                    {
                        return Err(err("not_found"));
                    }
                    let blob = || entry.blob.as_deref().ok_or_else(|| err("corrupt_blob"));
                    match entry.kind {
                        EntryKind::Folder => Err(err("not_text")),
                        EntryKind::Text => Ok(Source::Text(blob()?.to_string())),
                        EntryKind::Binary => {
                            let format = extraction::format(path).ok_or_else(|| err("not_text"))?;
                            let record = extraction::cached(&self.root, blob()?, format)?
                                .ok_or_else(|| err("extraction_pending"))?;
                            match record.status {
                                ExtractionStatus::Succeeded | ExtractionStatus::Partial => {
                                    Ok(Source::Extracted(
                                        record
                                            .segments
                                            .iter()
                                            .map(|g| g.text.as_str())
                                            .collect::<Vec<_>>()
                                            .join("\n\n"),
                                    ))
                                }
                                _ => Err(err("not_text")),
                            }
                        }
                    }
                })();
                let source = match source {
                    Ok(source) => source,
                    Err(e) if e.code == "corrupt_blob" => return Err(e),
                    Err(e) => {
                        items.push(failed(path, e));
                        continue;
                    }
                };
                if spent {
                    items.push(failed(path, err("max_bytes")));
                    continue;
                }
                let (kind, mut content) = match source {
                    Source::Text(blob) => (
                        "text",
                        String::from_utf8(storage::read_blob(&self.root, &blob)?)
                            .map_err(crate::io)?,
                    ),
                    Source::Extracted(text) => ("extracted", text),
                };
                let room = max_bytes - used;
                let truncated = content.len() > room;
                if truncated {
                    let mut cut = room;
                    while !content.is_char_boundary(cut) {
                        cut -= 1;
                    }
                    content.truncate(cut);
                }
                used += content.len();
                spent = truncated || used == max_bytes;
                items.push(ReadItem {
                    path: path.clone(),
                    ok: true,
                    kind: Some(kind.into()),
                    content: Some(content),
                    truncated,
                    error: None,
                });
            }
            Ok(ReadManyResult {
                revision_id,
                items,
                used_bytes: used,
            })
        })
    }
}
