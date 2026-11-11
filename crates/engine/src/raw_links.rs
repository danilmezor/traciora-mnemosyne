//! FileManager's pre-mutation reference snapshot for retained raw-path aliases.
//! Normal indexed references are handled by core. Raw aliases are not traversal
//! members: their cached reference offsets survive until adapter reconciliation
//! removes the hidden raw identity. Native duplicate raw/canonical writes can
//! therefore apply the same stale UTF-16 span twice; preserve that behavior.
use crate::{io, read_text, Engine, EngineError};
use mnemosyne_core::{cache::CachedMetadata, graph_refs::link_path, rewrite::rewrite_link};

#[derive(Debug, Clone)]
enum Location {
    Body { start: usize, end: usize },
    Property(String),
}
#[derive(Debug, Clone)]
struct Reference {
    original: String,
    link: String,
    location: Location,
    resolved: Vec<String>,
}
#[derive(Debug, Clone)]
struct RawSource {
    path: String,
    physical: String,
    references: Vec<Reference>,
}
#[derive(Debug, Clone, Default)]
pub struct RawLinkUpdatePlan {
    sources: Vec<RawSource>,
}

fn references(cache: &CachedMetadata) -> Vec<(String, String, Location)> {
    cache
        .frontmatter_links
        .iter()
        .flatten()
        .map(|r| {
            (
                r.original.clone(),
                r.link.clone(),
                Location::Property(r.key.clone()),
            )
        })
        .chain(cache.links.iter().chain(cache.embeds.iter()).map(|r| {
            (
                r.original.clone(),
                r.link.clone(),
                Location::Body {
                    start: r.position.start.offset,
                    end: r.position.end.offset,
                },
            )
        }))
        .collect()
}

impl Engine {
    /// Capture before a FileManager rename. This is not used for ordinary
    /// modify/delete/create events: those resolve live graphs without rewriting.
    pub fn prepare_raw_link_update(&self) -> RawLinkUpdatePlan {
        let sources = self
            .cached_file_paths()
            .into_iter()
            .filter_map(|path| {
                let physical = self.raw_file_destinations.get(path)?;
                let cache = self.file_cache(path)?;
                let references = references(cache)
                    .into_iter()
                    .filter_map(|(original, link, location)| {
                        let target = link_path(&link);
                        if target.is_empty() {
                            return None;
                        }
                        let resolved = self.vault.resolve_all_from_path(path, target);
                        (!resolved.is_empty()).then_some(Reference {
                            original,
                            link,
                            location,
                            resolved,
                        })
                    })
                    .collect();
                Some(RawSource {
                    path: path.into(),
                    physical: physical.clone(),
                    references,
                })
            })
            .collect();
        RawLinkUpdatePlan { sources }
    }
    /// Complete the raw-source portion after the core's indexed rename edits.
    /// For the captured initially-identical alias/cache pair, the repeated
    /// transformation has the same settled bytes in either processing order.
    /// Different stale raw/canonical cache contents and concurrent writes still
    /// require full shared FileManager transaction scheduling certification.
    pub fn complete_raw_link_update(
        &mut self,
        plan: RawLinkUpdatePlan,
        from: &str,
        to: &str,
    ) -> Result<Vec<String>, EngineError> {
        let mut modified = Vec::new();
        for source in plan.sources {
            if !self.raw_file_cache.contains_key(&source.path) {
                continue;
            }
            let mut changes = Vec::new();
            for reference in source.references {
                let target = link_path(&reference.link);
                let current = self.vault.resolve_all_from_path(&source.path, target);
                if !current.is_empty() && current == reference.resolved {
                    continue;
                }
                let resolved = &reference.resolved[0];
                let destination = if resolved == from {
                    to
                } else {
                    resolved.as_str()
                };
                let wiki =
                    reference.original.starts_with("[[") || reference.original.starts_with("![[");
                let new_link = format!(
                    "{}{}",
                    self.vault.file_to_linktext(destination, &source.path, wiki),
                    &reference.link[target.len()..]
                );
                if new_link == reference.link {
                    continue;
                }
                let text = rewrite_link(&reference.original, &reference.link, &new_link);
                changes.push((reference.location, text));
            }
            if changes.is_empty() {
                continue;
            }
            let path = self.vault.root.join(&source.physical);
            let before = read_text(&path)?;
            let mut units: Vec<u16> = before.encode_utf16().collect();
            let mut body = changes
                .iter()
                .filter_map(|(location, text)| {
                    if let Location::Body { start, end } = location {
                        Some((*start, *end, text))
                    } else {
                        None
                    }
                })
                .collect::<Vec<_>>();
            body.sort_by_key(|(start, _, _)| std::cmp::Reverse(*start));
            for (start, end, text) in body {
                // JavaScript substring clamps endpoints to source length. It
                // neither rejects stale offsets nor repairs split surrogates.
                let start = start.min(units.len());
                let end = end.min(units.len()).max(start);
                units.splice(start..end, text.encode_utf16());
            }
            let mut after = String::from_utf16_lossy(&units);
            if changes
                .iter()
                .any(|(location, _)| matches!(location, Location::Property(_)))
            {
                if let Some(bounds) = mnemosyne_core::frontmatter::processing_bounds(&after) {
                    if let Ok(mut document) =
                        mnemosyne_core::yaml_graph::parse(&after[bounds.from..bounds.to])
                    {
                        for (location, text) in &changes {
                            if let Location::Property(key) = location {
                                document.update_string(key, text);
                            }
                        }
                        let yaml = document
                            .stringify()
                            .map_err(|e| EngineError::InvalidSourceEdit(e.message))?;
                        after = format!("{}{}{}", &after[..bounds.from], yaml, &after[bounds.to..]);
                    }
                }
            }
            if after == before {
                continue;
            }
            std::fs::write(&path, &after).map_err(|e| io(&path, e))?;
            // Adapter reconcileInternalFile(raw) sees Al(raw), emits delete for
            // the raw TFile; a watcher then updates the physical canonical file.
            if mnemosyne_core::has_hidden_ancestor(&source.path) {
                self.file_deleted(&source.path);
            }
            if self.vault.has_file(&source.physical) {
                self.vault.modify(&source.physical, &after)?;
                self.file_modified(&source.physical);
            }
            modified.push(source.physical);
        }
        Ok(modified)
    }
}
