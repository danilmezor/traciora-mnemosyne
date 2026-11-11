//! Reversible scanner transport for JavaScript source units. Carrier spellings
//! are private, checked against both literal and decoded token contents, and
//! removed before values, keys, aliases or diagnostics leave the parser.
use crate::yaml_value::{Diagnostic, LinePosition};
use yaml_rust2::parser::{Event, Parser};
pub(super) struct Source {
    pub text: String,
    pub units: Vec<u16>,
    byte_positions: Vec<(usize, usize)>,
    prefix: Vec<u16>,
}
fn source_parts(units: &[u16]) -> Vec<(usize, usize, Option<char>)> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < units.len() {
        let a = units[i];
        if (0xd800..=0xdbff).contains(&a)
            && units
                .get(i + 1)
                .is_some_and(|b| (0xdc00..=0xdfff).contains(b))
        {
            let c = char::from_u32(
                0x10000 + ((a as u32 - 0xd800) << 10) + (units[i + 1] as u32 - 0xdc00),
            )
            .unwrap();
            out.push((i, 2, Some(c)));
            i += 2;
        } else {
            out.push((i, 1, char::from_u32(a as u32).filter(|_| a != 0)));
            i += 1;
        }
    }
    out
}
impl Source {
    pub fn new(units: &[u16]) -> Option<Self> {
        let parts = source_parts(units);
        if parts.iter().all(|(_, _, c)| c.is_some()) {
            return None;
        }
        // This initial spelling is only used to inspect decoded scanner tokens. It
        // never determines a returned value or key, and carries no replacement units.
        let analysis: String = parts.iter().map(|(_, _, c)| c.unwrap_or('X')).collect();
        let repaired = super::wtf16::prepare(&analysis);
        let mut decoded = Vec::new();
        let mut parser = Parser::new_from_str(&repaired.source);
        while let Ok((event, _)) = parser.next_token() {
            match event {
                Event::Scalar(text, _, _, tag) => {
                    decoded.push(text);
                    if let Some(tag) = tag {
                        decoded.push(format!("{}{}", tag.handle, tag.suffix));
                    }
                }
                Event::SequenceStart(_, Some(tag)) | Event::MappingStart(_, Some(tag)) => {
                    decoded.push(format!("{}{}", tag.handle, tag.suffix))
                }
                Event::StreamEnd => break,
                _ => {}
            }
        }
        let mut nonce = 0;
        let prefix = loop {
            let p = format!("\u{e001}yamlSurrogateRaw{nonce}U");
            if !analysis.contains(&p) && decoded.iter().all(|s| !s.contains(&p)) {
                break p;
            }
            nonce += 1;
        };
        let mut text = String::new();
        let mut positions = Vec::new();
        for (offset, _, character) in parts {
            if let Some(character) = character {
                positions.push((text.len(), offset));
                text.push(character);
            } else {
                let marker = format!("{prefix}{:04x}\u{e002}", units[offset]);
                for character in marker.chars() {
                    positions.push((text.len(), offset));
                    text.push(character);
                }
            }
        }
        positions.push((text.len(), units.len()));
        Some(Self {
            text,
            units: units.to_vec(),
            byte_positions: positions,
            prefix: prefix.encode_utf16().collect(),
        })
    }
    pub fn restore(&self, text: &[u16]) -> Vec<u16> {
        let mut result = Vec::new();
        let mut at = 0;
        while at < text.len() {
            if text[at..].starts_with(&self.prefix) {
                let start = at + self.prefix.len();
                let end = start + 4;
                if text.get(end) == Some(&0xe002) {
                    if let Ok(hex) = String::from_utf16(&text[start..end]) {
                        if let Ok(unit) = u16::from_str_radix(&hex, 16) {
                            result.push(unit);
                            at = end + 1;
                            continue;
                        }
                    }
                }
            }
            result.push(text[at]);
            at += 1;
        }
        result
    }
    pub fn position(&self, byte: usize) -> usize {
        if byte > self.text.len() {
            return self.units.len() + byte - self.text.len();
        }
        let index = self
            .byte_positions
            .partition_point(|(start, _)| *start <= byte)
            .saturating_sub(1);
        self.byte_positions[index].1
    }
    pub fn exception(&self, mut error: Diagnostic) -> Diagnostic {
        let old = error
            .message_units
            .as_ref()
            .cloned()
            .unwrap_or_else(|| error.message.encode_utf16().collect());
        let units = self.restore(&old);
        error.message = String::from_utf16_lossy(&units);
        error.message_units = String::from_utf16(&units).is_err().then_some(units);
        error
    }
    pub fn diagnostic(
        &self,
        start: usize,
        end: usize,
        name: &str,
        code: &str,
        message: &str,
    ) -> Diagnostic {
        let start = self.position(start);
        let end = self.position(end);
        diagnostic(
            &self.units,
            start,
            end,
            name,
            code,
            &self.restore(&message.encode_utf16().collect::<Vec<_>>()),
        )
    }
}
/// Native Gv formatting uses full-source UTF16 positions and source excerpts.
pub(super) fn diagnostic(
    source: &[u16],
    start: usize,
    end: usize,
    name: &str,
    code: &str,
    message: &[u16],
) -> Diagnostic {
    let start = start.min(source.len());
    let extra = end.saturating_sub(source.len());
    let end = end.min(source.len()).max(start);
    let locate = |position: usize| {
        let line = source[..position].iter().filter(|u| **u == 10).count() + 1;
        let line_start = source[..position]
            .iter()
            .rposition(|u| *u == 10)
            .map_or(0, |p| p + 1);
        LinePosition {
            line,
            col: position - line_start + 1,
        }
    };
    let first = locate(start);
    let mut last = locate(end);
    last.col += extra;
    let line_start = source[..start]
        .iter()
        .rposition(|u| *u == 10)
        .map_or(0, |p| p + 1);
    let line_end = source[start..]
        .iter()
        .position(|u| *u == 10)
        .map_or(source.len(), |p| start + p + 1);
    let mut text = source[line_start..line_end].to_vec();
    while matches!(text.last(), Some(10 | 13)) {
        text.pop();
    }
    let mut column = first.col - 1;
    if column >= 60 && text.len() > 80 {
        let cut = (column - 39).min(text.len() - 79);
        text = std::iter::once(0x2026)
            .chain(text[cut..].iter().copied())
            .collect();
        column -= cut - 1;
    }
    if text.len() > 80 {
        text.truncate(79);
        text.push(0x2026);
    }
    if first.line > 1 && text[..column.min(text.len())].iter().all(|u| *u == 32) {
        let previous_start = source[..line_start - 1]
            .iter()
            .rposition(|u| *u == 10)
            .map_or(0, |p| p + 1);
        let mut previous = source[previous_start..line_start].to_vec();
        if previous.len() > 80 {
            previous.truncate(79);
            previous.extend([0x2026, 10]);
        }
        previous.extend(text);
        text = previous;
    }
    let mut pretty = message.to_vec();
    pretty.extend(format!(" at line {}, column {}", first.line, first.col).encode_utf16());
    if text.iter().any(|u| *u != 32) {
        let width = if last.line == first.line && last.col > first.col {
            (last.col - first.col)
                .min(80usize.saturating_sub(column))
                .max(1)
        } else {
            1
        };
        pretty.extend(":\n\n".encode_utf16());
        pretty.extend(text);
        pretty.extend(format!("\n{}{}\n", " ".repeat(column), "^".repeat(width)).encode_utf16());
    }
    Diagnostic {
        name: name.into(),
        message: String::from_utf16_lossy(&pretty),
        message_units: String::from_utf16(&pretty).is_err().then_some(pretty),
        code: Some(code.into()),
        pos: Some([start, end + extra]),
        line_pos: Some([first, last]),
    }
}
