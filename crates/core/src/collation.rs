//! Filename ordering matching the shipped natural-sort comparator's options.
//!
//! The reference uses the renderer's default Intl locale, not Moment's locale.
//! Callers supply their selected BCP-47 locale. ICU4C78.2 and the exact
//! Chromium/Electron target data (including its trimming) are bundled; no
//! system or installed-app fallback is used.
use icu_locale::Locale;
pub use mnemosyne_icu78::{lowercase_units, Error as UnicodeCaseError};
use std::cmp::Ordering;

#[derive(Debug)]
pub struct NaturalCollator {
    collator: mnemosyne_icu78::Collator,
    locale: String,
}
#[derive(Debug, thiserror::Error)]
pub enum CollationError {
    #[error("Invalid collation locale: {0}")]
    Locale(String),
    #[error("Cannot load collation data for {0}: {1}")]
    Data(String, String),
}
impl NaturalCollator {
    /// Sort usage, base sensitivity and numeric ordering. Punctuation retains
    /// the locale's default handling, matching Intl's ignorePunctuation=false
    /// except for locales such as Thai whose default is locale-dependent.
    pub fn new(locale: &str) -> Result<Self, CollationError> {
        let parsed: Locale = locale
            .parse()
            .map_err(|_| CollationError::Locale(locale.into()))?;
        let collator = mnemosyne_icu78::Collator::new(&parsed.to_string())
            .map_err(|error| CollationError::Data(locale.into(), error.to_string()))?;
        Ok(Self {
            collator,
            locale: locale.into(),
        })
    }
    pub fn locale(&self) -> &str {
        &self.locale
    }
    /// Direct UTF16 comparison preserves native surrogate collation weights.
    pub fn compare_units(&self, left: &[u16], right: &[u16]) -> Ordering {
        self.collator.compare(left, right)
    }
    pub fn compare(&self, left: &str, right: &str) -> Ordering {
        self.compare_units(
            &left.encode_utf16().collect::<Vec<_>>(),
            &right.encode_utf16().collect::<Vec<_>>(),
        )
    }
}
