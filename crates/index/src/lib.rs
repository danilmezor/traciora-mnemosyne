//! Persistent full-text search over a [`Vault`], built on tantivy.

pub mod fuzzy;
mod managed;
mod search;

pub use managed::{
    sync_search, sync_search_fields, ManagedDocument, ManagedHit, ManagedQuery, ManagedSearch,
    MIN_TYPO_CHARS, NAME_BOOST,
};
pub use search::{IndexError, SearchIndex, SearchResult};

pub mod semantic;
