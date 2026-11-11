//! The [`Note`] model: a single Markdown file parsed into structured parts.

use std::collections::BTreeSet;
use std::path::PathBuf;
use unicode_normalization::UnicodeNormalization;

use crate::frontmatter::{self, Frontmatter, FrontmatterLink};
use crate::link::{self, parse_linktext, Tag, Wikilink};
use crate::markdown::{self, Heading, MarkdownLink};
use crate::sections::{self, ListItem, Section};
use crate::subpath::{self, BlockRef, FootnoteRef, ResolvedSubpath};

/// A single note in a vault.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Note {
    /// Path relative to the vault root, using `/` separators.
    pub path: PathBuf,
    /// The note title, derived from the file name without extension.
    pub title: String,
    /// The raw file contents.
    #[serde(skip)]
    pub source: String,
    /// The parsed frontmatter, if the file begins with a `---` block.
    pub frontmatter: Option<Frontmatter>,
    /// Byte offset of the body within [`Note::source`].
    #[serde(skip)]
    pub body_offset: usize,
    /// The note body: everything after the frontmatter.
    pub body: String,
    /// Headings found in the body.
    pub headings: Vec<Heading>,
    /// Outbound `[[wikilinks]]` and `![[embeds]]`.
    pub wikilinks: Vec<Wikilink>,
    /// Inline `#tags` found in the body.
    pub tags: Vec<Tag>,
    /// Standard `[text](url)` links.
    pub markdown_links: Vec<MarkdownLink>,
    /// Standard `![alt](url)` images.
    pub markdown_images: Vec<MarkdownLink>,
    /// `^block-id` references, keyed by lowercased id.
    pub blocks: std::collections::BTreeMap<String, BlockRef>,
    /// `[^id]` footnote definitions.
    pub footnotes: Vec<FootnoteRef>,
    /// `[[links]]` declared in frontmatter properties.
    pub frontmatter_links: Vec<FrontmatterLink>,
    /// Top-level blocks.
    pub sections: Vec<Section>,
    /// List items.
    pub list_items: Vec<ListItem>,
}

impl Note {
    /// Parses a note from its relative path and raw source text.
    pub fn parse(path: PathBuf, source: String) -> Self {
        let title = path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();

        let (frontmatter, body_start) = frontmatter::parse(&source);
        let body = source[body_start..].to_string();

        let extracted = link::extract(&body);
        let md = markdown::parse(&body);
        let headings = md.headings;
        let markdown_links = md.links;
        let markdown_images = md.images;
        let blocks = subpath::parse_blocks(&body);
        let footnotes = subpath::parse_footnotes(&body);
        let frontmatter_links = frontmatter
            .as_ref()
            .map(Frontmatter::links)
            .unwrap_or_default();
        let parsed_sections = sections::parse_sections(&body);

        Note {
            path,
            title,
            source,
            frontmatter,
            body_offset: body_start,
            body,
            headings,
            wikilinks: extracted.wikilinks,
            tags: extracted.tags,
            markdown_links,
            markdown_images,
            blocks,
            footnotes,
            frontmatter_links,
            sections: parsed_sections.sections,
            list_items: parsed_sections.list_items,
        }
    }

    /// Resolves a link subpath (`#heading`, `#a#b`, `#^block`, `#[^footnote]`)
    /// against this note.
    pub fn resolve_subpath(&self, subpath: &str) -> Option<ResolvedSubpath> {
        subpath::resolve_subpath(&self.headings, &self.blocks, &self.footnotes, subpath)
    }

    /// All outbound link references (wikilinks, embeds, and Markdown links),
    /// each with its raw text and byte span in [`Note::body`].
    pub fn references(&self) -> Vec<LinkReference> {
        let mut refs = Vec::new();
        for wl in &self.wikilinks {
            refs.push(LinkReference {
                original: self.body[wl.start..wl.end].to_string(),
                link: wl.target.clone(),
                start: wl.start,
                end: wl.end,
            });
        }
        for ml in &self.markdown_links {
            let Some(link) = decoded_markdown_destination(&ml.destination) else {
                continue;
            };
            refs.push(LinkReference {
                original: self.body[ml.start..ml.end].to_string(),
                link,
                start: ml.start,
                end: ml.end,
            });
        }
        for img in &self.markdown_images {
            let Some(link) = decoded_markdown_destination(&img.destination) else {
                continue;
            };
            refs.push(LinkReference {
                original: self.body[img.start..img.end].to_string(),
                link,
                start: img.start,
                end: img.end,
            });
        }
        refs.sort_by_key(|r| r.start);
        refs
    }

    /// Path parts of every outbound internal link (body references plus
    /// frontmatter links), for backlink and graph resolution. External URLs
    /// are excluded.
    pub fn link_targets(&self) -> Vec<String> {
        let mut out = Vec::new();
        for r in self.references() {
            if !crate::link::is_internal_link(&r.link) {
                continue;
            }
            out.push(parse_linktext(&r.link).path);
        }
        for fl in &self.frontmatter_links {
            out.push(parse_linktext(&fl.link).path);
        }
        out
    }

    /// All tags for the note: inline `#tags` plus any frontmatter `tags`,
    /// deduplicated and sorted.
    pub fn all_tags(&self) -> Vec<String> {
        let mut set = BTreeSet::new();
        for t in &self.tags {
            set.insert(t.name.clone());
        }
        if let Some(fm) = &self.frontmatter {
            for t in fm.tags() {
                set.insert(t);
            }
        }
        set.into_iter().collect()
    }
}

fn decoded_markdown_destination(destination: &str) -> Option<String> {
    if !link::is_internal_link(destination) {
        return Some(destination.to_string());
    }
    crate::rewrite::decode_uri(destination)
        .map(|decoded| decoded.replace('\u{00a0}', " ").trim().nfc().collect())
}

/// A single link reference in a note's body, with its raw text and byte span.
#[derive(Debug, Clone)]
pub struct LinkReference {
    /// The raw link text (e.g. `[[Note#Heading|Alias]]` or `[text](Note.md)`).
    pub original: String,
    /// The link target: path plus any `#subpath` (or the Markdown destination).
    pub link: String,
    /// Byte offset of the reference within [`Note::body`].
    pub start: usize,
    /// Byte offset one past the reference within [`Note::body`].
    pub end: usize,
}
