//! Saved Markdown word-count contract. Only frontmatter is removed; markup,
//! whitespace and line endings remain part of the character count.
#[path = "word_letter_ranges.rs"]
mod letters;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Counts {
    pub words: usize,
    pub characters: usize,
}
fn letter(unit: u16) -> bool {
    if matches!(unit, 0x0027 | 0x002d | 0x0b80..=0x0bff | 0xac00..=0xd7a3 | 0xa960..=0xa97c | 0xd7b0..=0xd7c6)
    {
        return true;
    }
    let index = letters::LETTER_RANGES.partition_point(|(_, end)| *end < unit);
    letters::LETTER_RANGES
        .get(index)
        .is_some_and(|(start, end)| (*start..=*end).contains(&unit))
}
fn individual(unit: u16) -> bool {
    matches!(unit, 0x0f00 | 0x0f40..=0x0f47 | 0x0f49..=0x0f6c | 0x0f88..=0x0f8c | 0x3041..=0x3096 | 0x309d..=0x309f | 0x30a1..=0x30fa | 0x30fc..=0x30ff | 0x4e00..=0x9fd5 | 0xac00..=0xd7a3 | 0xa960..=0xa97c | 0xd7b0..=0xd7c6)
}
fn digit(unit: u16) -> bool {
    (b'0' as u16..=b'9' as u16).contains(&unit)
}

/// Input is the cached-read string, after removing exactly one initial BOM.
pub fn count(source: &str) -> Counts {
    let start =
        crate::frontmatter::processing_bounds(source).map_or(0, |bounds| bounds.content_start);
    let units: Vec<_> = source[start..].encode_utf16().collect();
    let mut words = 0;
    let mut cursor = 0;
    while let Some(&unit) = units.get(cursor) {
        if digit(unit) || letter(unit) {
            // The repeated group contains both numeric and letter alternatives.
            // Adjacent alternatives (abc123def) form one match.
            while let Some(&next) = units.get(cursor) {
                if digit(next) {
                    cursor += 1;
                    while units.get(cursor).is_some_and(|unit| digit(*unit)) {
                        cursor += 1;
                    }
                    while units
                        .get(cursor)
                        .is_some_and(|unit| matches!(*unit, 0x002c | 0x002e))
                        && units.get(cursor + 1).is_some_and(|unit| digit(*unit))
                    {
                        cursor += 2;
                        while units.get(cursor).is_some_and(|unit| digit(*unit)) {
                            cursor += 1;
                        }
                    }
                } else if letter(next) {
                    cursor += 1;
                } else {
                    break;
                }
            }
            words += 1;
        } else {
            words += usize::from(individual(unit));
            cursor += 1;
        }
    }
    Counts {
        words,
        characters: units.len(),
    }
}
