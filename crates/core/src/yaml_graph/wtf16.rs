//! Lossless surrogate escape adapter around the scalar-only Rust scanner.
//! Recovery is driven by actual scanner failures in a quoted scalar. It does
//! not rewrite block literals/comments or reserve any string the input can use.
use yaml_rust2::{
    parser::{Event, Parser},
    scanner::Scanner,
};
#[derive(Clone)]
struct Escape {
    start: usize,
    end: usize,
    unit: u16,
}
pub(super) struct Prepared {
    pub source: String,
    pub marks: Vec<usize>,
    prefix: String,
}
impl Prepared {
    /// The native composer accepts undefined aliases; resolution is a later
    /// toJS operation. The scanner transport preserves original token marks.
    pub fn recover_undefined_aliases(&mut self) -> std::collections::BTreeMap<usize, Vec<u16>> {
        let mut aliases = std::collections::BTreeMap::new();
        if !self.source.contains('*') {
            return aliases;
        }
        loop {
            let mut parser = Parser::new_from_str(&self.source);
            let error = loop {
                match parser.next_token() {
                    Ok((Event::StreamEnd, _)) => break None,
                    Ok(_) => {}
                    Err(error) => break Some(error),
                }
            };
            let Some(error) = error else { break };
            if !error.info().contains("found unknown anchor") {
                break;
            }
            let index = error.marker().index();
            let start = self
                .source
                .char_indices()
                .nth(index)
                .map_or(self.source.len(), |(i, _)| i);
            let tail = &self.source[start..];
            if !tail.starts_with('*') {
                break;
            }
            let end = tail
                .find(|c: char| c.is_whitespace() || matches!(c, ',' | ']' | '}'))
                .unwrap_or(tail.len());
            let alias = tail[1..end].encode_utf16().collect();
            aliases.insert(self.marks[index], alias);
            // This is positional transport, never a returned YAML null value.
            let width = tail[..end].chars().count();
            self.source.replace_range(
                start..start + end,
                &("~".to_owned() + &" ".repeat(width - 1)),
            );
        }
        aliases
    }
    pub fn restore(&self, text: &str) -> Option<Vec<u16>> {
        if self.prefix.is_empty() || !text.contains(&self.prefix) {
            return None;
        }
        let mut units = Vec::new();
        let mut remaining = text;
        while let Some(index) = remaining.find(&self.prefix) {
            units.extend(remaining[..index].encode_utf16());
            let tail = &remaining[index + self.prefix.len()..];
            let unit = u16::from_str_radix(&tail[..4], 16).expect("private escape marker");
            assert!(tail[4..].starts_with('\0'));
            units.push(unit);
            remaining = &tail[5..];
        }
        units.extend(remaining.encode_utf16());
        Some(units)
    }
}
pub(super) fn prepare(source: &str) -> Prepared {
    if let Some(rest) = source.strip_prefix('\u{feff}') {
        let mut prepared = prepare_impl(rest);
        // The native lexer consumes one leading BOM; source positions retain it.
        for mark in &mut prepared.marks {
            *mark += 3;
        }
        prepared
    } else {
        prepare_impl(source)
    }
}
fn prepare_impl(source: &str) -> Prepared {
    let mut safe = source.to_owned();
    let mut escapes = Vec::new();
    // Scanner recovery keeps the original byte and character lengths intact.
    loop {
        let mut scanner = Scanner::new(safe.chars());
        while scanner.next().is_some() {}
        let Some(error) = scanner.get_error() else {
            break;
        };
        if !error
            .info()
            .contains("invalid Unicode character escape code")
        {
            break;
        }
        let start = safe
            .char_indices()
            .nth(error.marker().index())
            .map_or(safe.len(), |(i, _)| i);
        if safe.as_bytes().get(start) != Some(&b'"') {
            break;
        }
        let mut index = start + 1;
        let mut found = Vec::new();
        while index < safe.len() {
            let character = safe[index..].chars().next().unwrap();
            if character == '"' {
                break;
            }
            if character != '\\' {
                index += character.len_utf8();
                continue;
            }
            let width = match safe.as_bytes().get(index + 1) {
                Some(b'u') => 4,
                Some(b'U') => 8,
                _ => 0,
            };
            if width > 0 && index + 2 + width <= safe.len() {
                let end = index + 2 + width;
                if let Ok(unit) = u32::from_str_radix(&safe[index + 2..end], 16) {
                    if (0xd800..=0xdfff).contains(&unit) {
                        found.push(Escape {
                            start: index,
                            end,
                            unit: unit as u16,
                        });
                    }
                }
                index = end;
            } else {
                index += 1;
                if index < safe.len() {
                    index += safe[index..].chars().next().unwrap().len_utf8();
                }
            }
        }
        if found.is_empty() {
            break;
        }
        for escape in &found {
            let replacement = if escape.end - escape.start == 6 {
                "\\uFFFD"
            } else {
                "\\U0000FFFD"
            };
            safe.replace_range(escape.start..escape.end, replacement);
        }
        escapes.extend(found);
    }
    if escapes.is_empty() {
        return Prepared {
            source: source.to_owned(),
            marks: source
                .char_indices()
                .map(|(i, _)| i)
                .chain(std::iter::once(source.len()))
                .collect(),
            prefix: String::new(),
        };
    }
    // Read every recovered scalar before choosing a marker prefix. Consequently
    // neither a literal user string nor an escaped spelling can collide with it.
    let mut parser = Parser::new_from_str(&safe);
    let mut scalars = Vec::new();
    while let Ok((event, _)) = parser.next_token() {
        match event {
            Event::Scalar(text, ..) => scalars.push(text),
            Event::StreamEnd => break,
            _ => {}
        }
    }
    let mut nonce = 0usize;
    let prefix = loop {
        let prefix = format!("\0yaml-surrogate:{nonce}:");
        if scalars.iter().all(|text| !text.contains(&prefix)) {
            break prefix;
        }
        nonce += 1;
    };
    escapes.sort_by_key(|escape| escape.start);
    let mut prepared = String::new();
    let mut marks = Vec::new();
    let mut previous = 0;
    for escape in escapes {
        for (offset, character) in source[previous..escape.start].char_indices() {
            marks.push(previous + offset);
            prepared.push(character);
        }
        let marker = format!("\\u0000yaml-surrogate:{nonce}:{:04x}\\u0000", escape.unit);
        for character in marker.chars() {
            marks.push(escape.start);
            prepared.push(character);
        }
        previous = escape.end;
    }
    for (offset, character) in source[previous..].char_indices() {
        marks.push(previous + offset);
        prepared.push(character);
    }
    marks.push(source.len());
    Prepared {
        source: prepared,
        marks,
        prefix,
    }
}
