//! Private cache snapshots, distinct from native enumerable cache projection.
use super::*;

#[derive(Serialize, Deserialize)]
struct FrontmatterLink {
    key: String,
    key_units: Option<Vec<u16>>,
    link: String,
    original: String,
    display_text: String,
}
#[derive(Serialize, Deserialize)]
struct Snapshot {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    v: Option<u32>,
    links: Vec<ReferenceCache>,
    embeds: Vec<ReferenceCache>,
    tags: Vec<TagCache>,
    headings: Vec<HeadingCache>,
    footnotes: Vec<FootnoteCache>,
    footnote_refs: Vec<FootnoteRefCache>,
    reference_links: Vec<ReferenceLinkCache>,
    sections: Vec<SectionCache>,
    list_items: Vec<ListItemCache>,
    blocks: BTreeMap<String, BlockCache>,
    native_frontmatter: Option<String>,
    legacy_frontmatter: Option<String>,
    frontmatter_position: Option<CachePosition>,
    frontmatter_links: Option<Vec<FrontmatterLink>>,
}
impl CachedMetadata {
    /// Encode parsed cache fields and the native JSON-cloned frontmatter.
    /// Preserve an actual enumerable `v`; fresh unversioned caches omit it.
    pub fn encode_persistent(&self) -> Result<serde_json::Value, String> {
        let snapshot = Snapshot {
            v: self.version,
            links: self.links.clone(),
            embeds: self.embeds.clone(),
            tags: self.tags.clone(),
            headings: self.headings.clone(),
            footnotes: self.footnotes.clone(),
            footnote_refs: self.footnote_refs.clone(),
            reference_links: self.reference_links.clone(),
            sections: self.sections.clone(),
            list_items: self.list_items.clone(),
            blocks: self.blocks.clone(),
            native_frontmatter: self
                .native_frontmatter
                .as_deref()
                .map(|document| document.json(false).map_err(|error| error.to_string()))
                .transpose()?,
            legacy_frontmatter: if self.native_frontmatter.is_none() {
                self.frontmatter
                    .as_ref()
                    .map(serde_yaml::to_string)
                    .transpose()
                    .map_err(|error| error.to_string())?
            } else {
                None
            },
            frontmatter_position: self.frontmatter_position,
            frontmatter_links: self.frontmatter_links.as_ref().map(|links| {
                links
                    .iter()
                    .map(|link| FrontmatterLink {
                        key: link.key.clone(),
                        key_units: link.key_units.clone(),
                        link: link.link.clone(),
                        original: link.original.clone(),
                        display_text: link.display_text.clone(),
                    })
                    .collect()
            }),
        };
        serde_json::to_value(snapshot).map_err(|error| error.to_string())
    }
    /// Hydrate actual persisted cache data, parsing only stored JSON rather than
    /// Markdown source. Native JSON cloning intentionally loses constructors
    /// and shared alias identity; escaped UTF16 names remain lossless.
    pub fn decode_persistent(value: serde_json::Value) -> Result<Self, String> {
        let snapshot: Snapshot =
            serde_json::from_value(value).map_err(|error| error.to_string())?;
        let graph = snapshot
            .native_frontmatter
            .map(|json| {
                // RawValue validates strict JSON without decoding its UTF16 names
                // through Rust String; escaped lone surrogates remain lossless.
                let _: Box<serde_json::value::RawValue> =
                    serde_json::from_str(&json).map_err(|error| error.to_string())?;
                let document =
                    crate::yaml_graph::parse(&json).map_err(|error| error.to_string())?;
                if let Some(failure) = metadata_worker_failure(&document) {
                    return Err(format!(
                        "Invalid persisted metadata worker result: {failure}"
                    ));
                }
                Ok::<_, String>(document)
            })
            .transpose()?;
        let frontmatter = if let Some(graph) = &graph {
            graph.metadata_mapping().ok().flatten()
        } else {
            snapshot
                .legacy_frontmatter
                .as_deref()
                .map(serde_yaml::from_str)
                .transpose()
                .map_err(|error| error.to_string())?
        };
        Ok(Self {
            // Native preload migrates absent/zero markers to 1 and preserves
            // already versioned records, including markers above 1.
            version: Some(snapshot.v.unwrap_or(1).max(1)),
            links: snapshot.links,
            embeds: snapshot.embeds,
            tags: snapshot.tags,
            headings: snapshot.headings,
            footnotes: snapshot.footnotes,
            footnote_refs: snapshot.footnote_refs,
            reference_links: snapshot.reference_links,
            sections: snapshot.sections,
            list_items: snapshot.list_items,
            blocks: snapshot.blocks,
            frontmatter,
            native_frontmatter: graph.map(std::sync::Arc::new),
            frontmatter_position: snapshot.frontmatter_position,
            frontmatter_links: snapshot.frontmatter_links.map(|links| {
                links
                    .into_iter()
                    .map(|link| FrontmatterLinkCache {
                        key: link.key,
                        key_units: link.key_units,
                        link: link.link,
                        original: link.original,
                        display_text: link.display_text,
                    })
                    .collect()
            }),
        })
    }
}
