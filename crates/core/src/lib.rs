//! Clean-room parsing of Markdown vaults.
//!
//! This crate implements the *data format* only: YAML frontmatter,
//! `[[wikilinks]]`, `![[embeds]]`, `#tags`, and the link/backlink graph.

/// The per-vault configuration directory. Its name is kept so existing vaults
/// open with their settings intact.
pub const CONFIG_DIR: &str = ".obsidian";

pub mod cache;
pub mod frontmatter;
pub mod graph;
pub mod graph_refs;
pub mod ignore;
pub mod link;
pub mod markdown;
pub mod note;
pub mod path;
pub mod rewrite;
pub mod search;
pub mod sections;
pub mod subpath;
pub mod vault;
pub mod vault_search;
pub mod yaml;
pub mod yaml_graph;
pub mod yaml_value;
pub mod wordcount;

pub use frontmatter::Frontmatter;
pub use graph::{Graph, GraphEdge};
pub use link::{
    is_internal_link, is_valid_tag, parse_linktext, strip_heading, strip_heading_for_link,
    Extracted, Linktext, Tag, Wikilink,
};
pub use markdown::{Heading, MarkdownLink};
pub use note::{LinkReference, Note};
pub use path::{
    basename, basename_no_md, extension, has_hidden_ancestor, normalize_path, parent,
    strip_extension, strip_md_extension,
};
pub use search::{sort_search_results, FuzzySearch, SearchMatch, SimpleSearch};
pub use sections::{parse_sections, ListItem, Pos, Range, Section, Sections};
pub use subpath::{
    parse_blocks, parse_footnotes, resolve_subpath, BlockRef, FootnoteRef, ResolvedSubpath,
};
pub use vault::{Vault, VaultError};

pub mod collation;
