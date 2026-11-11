//! Graph references projected from normalized saved metadata.
//!
//! Source parsing/rewrite spans remain on Note. Graph lookup consumes cache
//! hrefs instead: wiki targets are normalized, Markdown/YAML follow their own
//! decode contracts, and reference definitions do not become outgoing links.
use crate::{cache::CachedMetadata, Note};

/// Shipped metadata reference order: frontmatter, ordinary links, then embeds.
pub fn references(cache: &CachedMetadata) -> impl Iterator<Item = &str> {
    cache
        .frontmatter_links
        .iter()
        .flatten()
        .map(|link| link.link.as_str())
        .chain(cache.links.iter().map(|link| link.link.as_str()))
        .chain(cache.embeds.iter().map(|link| link.link.as_str()))
}

/// Destination lookup ignores the heading/block subpath, including empty paths
/// that refer to the current file. It does not normalize the href a second time.
pub fn link_path(link: &str) -> &str {
    link.split_once('#').map_or(link, |(path, _)| path)
}

/// Build the same reference projection for Vault callers without retained cache.
/// This cannot recurse: cache construction uses raw Note.references, while this
/// helper leaves that source-span model unchanged.
pub fn link_targets(note: &Note) -> Vec<String> {
    let Ok(cache) = CachedMetadata::try_from_note(note) else {
        return Vec::new();
    };
    references(&cache)
        .map(|href| link_path(href).to_owned())
        .collect()
}
