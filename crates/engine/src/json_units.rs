//! Strict JSON data with JavaScript UTF16 strings and object keys. This boundary
//! never substitutes U+FFFD for unpaired surrogate code units.
use mnemosyne_core::{vault_search::ecmascript_number, yaml_value::quote_units};
use serde_json::Value;
#[derive(Debug)]
pub enum JsonUnits {
    Null,
    Bool(bool),
    Number(f64),
    String(Vec<u16>),
    Array(Vec<Self>),
    Object(Vec<(Vec<u16>, Self)>),
}
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("invalid JSON at byte {offset}: {message}")]
pub struct JsonUnitsError {
    pub offset: usize,
    pub message: &'static str,
}
impl JsonUnitsError {
    /// V8 15.0.245.31 JSON parser diagnostics over UTF16. Templates and context
    /// selection are backed by the versioned upstream source; the pinned
    /// Electron renderer independently supplies the portable runtime corpus.
    pub fn native_message_units(&self, source: &str) -> Vec<u16> {
        let source_units = units(source);
        let offset = self.offset.min(source.len());
        let position = source
            .get(..offset)
            .map(|s| s.encode_utf16().count())
            .unwrap_or(source_units.len());
        let mut line = 1;
        let mut last = 0;
        let mut i = 0;
        while i < position {
            if source_units[i] == 13 && i + 1 < position && source_units[i + 1] == 10 {
                i += 1;
            }
            if matches!(source_units[i], 10 | 13) {
                line += 1;
                last = i + 1;
            }
            i += 1;
        }
        let column = position - last + 1;
        let label = match self.message {
            "expected initial key" => Some("Expected property name or '}' in JSON"),
            "expected following key" => Some("Expected double-quoted property name in JSON"),
            "expected array comma" => Some("Expected ',' or ']' after array element in JSON"),
            "expected object comma" => Some("Expected ',' or '}' after property value in JSON"),
            "expected colon" => Some("Expected ':' after property name in JSON"),
            "expected fraction" => Some("Unterminated fractional number in JSON"),
            "expected exponent" => Some("Exponent part is missing a number in JSON"),
            "no number after minus" => Some("No number after minus sign in JSON"),
            "unexpected number" => Some("Unexpected number in JSON"),
            "trailing input" => Some("Unexpected non-whitespace character after JSON"),
            "unterminated string" => Some("Unterminated string in JSON"),
            "invalid escape" => Some("Bad escaped character in JSON"),
            "control character in string" => {
                Some("Bad control character in string literal in JSON")
            }
            "invalid unicode escape" | "incomplete unicode escape" => {
                Some("Bad Unicode escape in JSON")
            }
            _ => None,
        };
        if let Some(label) = label {
            return units(&format!(
                "{label} at position {position} (line {line} column {column})"
            ));
        }
        let Some(&token) = source_units.get(position) else {
            return units("Unexpected end of JSON input");
        };
        if matches!(token, 48..=57 | 45 | 34) {
            let label = if token == 34 { "string" } else { "number" };
            return units(&format!(
                "Unexpected {label} in JSON at position {position} (line {line} column {column})"
            ));
        }
        if ["undefined", "NaN", "Infinity", "[object Object]"]
            .iter()
            .any(|s| units(s) == source_units)
        {
            let mut output = units("\"");
            output.extend(&source_units);
            output.extend(units("\" is not valid JSON"));
            return output;
        }
        let mut output = units("Unexpected token '");
        output.push(token);
        output.extend(units("', "));
        let (start, end, before, after) = if source_units.len() < 21 {
            (0, source_units.len(), false, false)
        } else if position < 10 {
            (0, position + 10, false, true)
        } else if position < source_units.len() - 10 {
            (position - 10, position + 10, true, true)
        } else {
            (position - 10, source_units.len(), true, false)
        };
        if before {
            output.extend(units("..."));
        }
        output.push(34);
        output.extend(&source_units[start..end]);
        output.push(34);
        if after {
            output.extend(units("..."));
        }
        output.extend(units(" is not valid JSON"));
        output
    }
    pub fn native_message(&self, source: &str) -> Option<String> {
        String::from_utf16(&self.native_message_units(source)).ok()
    }
}
pub fn units(value: &str) -> Vec<u16> {
    value.encode_utf16().collect()
}
pub fn array_index_units(name: &[u16]) -> Option<u32> {
    if name.is_empty() || name.len() > 10 || (name.len() > 1 && name[0] == 48) {
        return None;
    }
    let mut n = 0u32;
    for &unit in name {
        if !(48..=57).contains(&unit) {
            return None;
        }
        n = n.checked_mul(10)?.checked_add((unit - 48) as u32)?;
    }
    (n != u32::MAX).then_some(n)
}
pub fn enumerate<T>(entries: &[(Vec<u16>, T)]) -> Vec<&(Vec<u16>, T)> {
    let mut result = entries.iter().collect::<Vec<_>>();
    result.sort_by(
        |a, b| match (array_index_units(&a.0), array_index_units(&b.0)) {
            (Some(a), Some(b)) => a.cmp(&b),
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            _ => std::cmp::Ordering::Equal,
        },
    );
    result
}
impl Clone for JsonUnits {
    fn clone(&self) -> Self {
        enum Work<'a> {
            Value(&'a JsonUnits),
            Array(usize),
            Object(Vec<Vec<u16>>),
        }
        let mut work = vec![Work::Value(self)];
        let mut values = Vec::new();
        while let Some(item) = work.pop() {
            match item {
                Work::Array(count) => {
                    let fields = values.split_off(values.len() - count);
                    values.push(Self::Array(fields));
                }
                Work::Object(keys) => {
                    let fields = values.split_off(values.len() - keys.len());
                    values.push(Self::Object(keys.into_iter().zip(fields).collect()));
                }
                Work::Value(value) => match value {
                    Self::Null => values.push(Self::Null),
                    Self::Bool(b) => values.push(Self::Bool(*b)),
                    Self::Number(n) => values.push(Self::Number(*n)),
                    Self::String(s) => values.push(Self::String(s.clone())),
                    Self::Array(a) => {
                        work.push(Work::Array(a.len()));
                        work.extend(a.iter().rev().map(Work::Value));
                    }
                    Self::Object(o) => {
                        work.push(Work::Object(o.iter().map(|(k, _)| k.clone()).collect()));
                        work.extend(o.iter().rev().map(|(_, v)| Work::Value(v)));
                    }
                },
            }
        }
        values.pop().unwrap()
    }
}
impl PartialEq for JsonUnits {
    fn eq(&self, other: &Self) -> bool {
        let mut work = vec![(self, other)];
        while let Some((left, right)) = work.pop() {
            match (left, right) {
                (Self::Null, Self::Null) => {}
                (Self::Bool(a), Self::Bool(b)) if a == b => {}
                (Self::Number(a), Self::Number(b)) if a == b => {}
                (Self::String(a), Self::String(b)) if a == b => {}
                (Self::Array(a), Self::Array(b)) if a.len() == b.len() => {
                    work.extend(a.iter().zip(b))
                }
                (Self::Object(a), Self::Object(b)) if a.len() == b.len() => {
                    for ((ak, av), (bk, bv)) in a.iter().zip(b) {
                        if ak != bk {
                            return false;
                        }
                        work.push((av, bv));
                    }
                }
                _ => return false,
            }
        }
        true
    }
}
impl Drop for JsonUnits {
    fn drop(&mut self) {
        fn children(value: &mut JsonUnits, work: &mut Vec<JsonUnits>) {
            match value {
                JsonUnits::Array(items) => work.append(items),
                JsonUnits::Object(fields) => {
                    work.extend(std::mem::take(fields).into_iter().map(|(_, value)| value))
                }
                _ => {}
            }
        }
        let mut work = Vec::new();
        children(self, &mut work);
        while let Some(mut value) = work.pop() {
            children(&mut value, &mut work);
        }
    }
}
impl JsonUnits {
    pub fn parse(source: &str) -> Result<Self, JsonUnitsError> {
        let mut parser = Parser { source, pos: 0 };
        let value = parser.value()?;
        parser.ws();
        if parser.pos != source.len() {
            return Err(parser.error("trailing input"));
        }
        Ok(value)
    }
    pub fn from_value(value: &Value) -> Self {
        enum Work<'a> {
            Value(&'a Value),
            Array(usize),
            Object(Vec<Vec<u16>>),
        }
        let mut work = vec![Work::Value(value)];
        let mut values = Vec::new();
        while let Some(item) = work.pop() {
            match item {
                Work::Array(count) => {
                    let items = values.split_off(values.len() - count);
                    values.push(Self::Array(items));
                }
                Work::Object(keys) => {
                    let items = values.split_off(values.len() - keys.len());
                    values.push(Self::Object(keys.into_iter().zip(items).collect()));
                }
                Work::Value(value) => match value {
                    Value::Null => values.push(Self::Null),
                    Value::Bool(b) => values.push(Self::Bool(*b)),
                    Value::Number(n) => values.push(Self::Number(n.as_f64().unwrap())),
                    Value::String(s) => values.push(Self::String(units(s))),
                    Value::Array(a) => {
                        work.push(Work::Array(a.len()));
                        work.extend(a.iter().rev().map(Work::Value));
                    }
                    Value::Object(o) => {
                        work.push(Work::Object(o.keys().map(|k| units(k)).collect()));
                        work.extend(o.values().rev().map(Work::Value));
                    }
                },
            }
        }
        values.pop().unwrap()
    }
    /// JSON-compatible UTF8 projection, without substituting invalid strings.
    /// Non-finite Number values project to null as they do in JSON.stringify.
    /// The returned serde_json value has that crate's own drop/consumer rules;
    /// retain JsonUnits for arbitrary-depth native data and lossless strings.
    pub fn to_value(&self) -> Option<Value> {
        enum Work<'a> {
            Value(&'a JsonUnits),
            Array(usize),
            Object(Vec<String>),
        }
        fn discard(values: &mut Vec<Value>) {
            while let Some(value) = values.pop() {
                match value {
                    Value::Array(mut items) => values.append(&mut items),
                    Value::Object(fields) => values.extend(fields.into_values()),
                    _ => {}
                }
            }
        }
        let mut work = vec![Work::Value(self)];
        let mut values = Vec::new();
        while let Some(item) = work.pop() {
            match item {
                Work::Array(count) => {
                    let items = values.split_off(values.len() - count);
                    values.push(Value::Array(items));
                }
                Work::Object(keys) => {
                    let items = values.split_off(values.len() - keys.len());
                    values.push(Value::Object(keys.into_iter().zip(items).collect()));
                }
                Work::Value(value) => match value {
                    Self::Null => values.push(Value::Null),
                    Self::Bool(b) => values.push(Value::Bool(*b)),
                    Self::Number(n) => values.push(if n.is_finite() {
                        serde_json::from_str(&ecmascript_number(*n)).ok()?
                    } else {
                        Value::Null
                    }),
                    Self::String(s) => match String::from_utf16(s) {
                        Ok(s) => values.push(Value::String(s)),
                        Err(_) => {
                            discard(&mut values);
                            return None;
                        }
                    },
                    Self::Array(a) => {
                        work.push(Work::Array(a.len()));
                        work.extend(a.iter().rev().map(Work::Value));
                    }
                    Self::Object(o) => {
                        let entries = enumerate(o);
                        let keys = entries
                            .iter()
                            .map(|(k, _)| String::from_utf16(k))
                            .collect::<Result<Vec<_>, _>>();
                        let Ok(keys) = keys else {
                            discard(&mut values);
                            return None;
                        };
                        work.push(Work::Object(keys));
                        work.extend(entries.into_iter().rev().map(|(_, v)| Work::Value(v)));
                    }
                },
            }
        }
        values.pop()
    }
    pub fn property(&self, name: &[u16]) -> Option<&Self> {
        match self {
            Self::Object(entries) => entries.iter().find(|(k, _)| k == name).map(|(_, v)| v),
            _ => None,
        }
    }
    pub fn pretty(&self) -> String {
        self.stringify(true)
    }
    pub fn compact(&self) -> String {
        self.stringify(false)
    }
    fn stringify(&self, pretty: bool) -> String {
        enum Work<'a> {
            Value(&'a JsonUnits, usize),
            Text(String),
        }
        let mut work = vec![Work::Value(self, 0)];
        let mut out = String::new();
        while let Some(item) = work.pop() {
            match item {
                Work::Text(text) => out.push_str(&text),
                Work::Value(value, depth) => match value {
                    Self::Null => out.push_str("null"),
                    Self::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
                    Self::Number(n) => {
                        if n.is_finite() {
                            out.push_str(&ecmascript_number(*n))
                        } else {
                            out.push_str("null")
                        }
                    }
                    Self::String(text) => out.push_str(&quote_units(text)),
                    Self::Array(values) => {
                        out.push('[');
                        if values.is_empty() {
                            out.push(']');
                            continue;
                        }
                        work.push(Work::Text(if pretty {
                            format!("\n{}]", "  ".repeat(depth))
                        } else {
                            "]".into()
                        }));
                        for (i, value) in values.iter().enumerate().rev() {
                            work.push(Work::Value(value, depth + 1));
                            work.push(Work::Text(if pretty {
                                format!(
                                    "{}\n{}",
                                    if i > 0 { "," } else { "" },
                                    "  ".repeat(depth + 1)
                                )
                            } else {
                                if i > 0 { "," } else { "" }.into()
                            }));
                        }
                    }
                    Self::Object(entries) => {
                        out.push('{');
                        if entries.is_empty() {
                            out.push('}');
                            continue;
                        }
                        work.push(Work::Text(if pretty {
                            format!("\n{}}}", "  ".repeat(depth))
                        } else {
                            "}".into()
                        }));
                        for (i, (key, value)) in enumerate(entries).into_iter().enumerate().rev() {
                            work.push(Work::Value(value, depth + 1));
                            work.push(Work::Text(if pretty {
                                format!(
                                    "{}\n{}{}: ",
                                    if i > 0 { "," } else { "" },
                                    "  ".repeat(depth + 1),
                                    quote_units(key)
                                )
                            } else {
                                format!("{}{}:", if i > 0 { "," } else { "" }, quote_units(key))
                            }));
                        }
                    }
                },
            }
        }
        out
    }
}
struct Parser<'a> {
    source: &'a str,
    pos: usize,
}
impl Parser<'_> {
    fn error(&self, message: &'static str) -> JsonUnitsError {
        JsonUnitsError {
            offset: self.pos,
            message,
        }
    }
    fn ws(&mut self) {
        while self
            .source
            .as_bytes()
            .get(self.pos)
            .is_some_and(|b| matches!(b, b' ' | b'\t' | b'\n' | b'\r'))
        {
            self.pos += 1;
        }
    }
    fn eat(&mut self, byte: u8) -> bool {
        if self.source.as_bytes().get(self.pos) == Some(&byte) {
            self.pos += 1;
            true
        } else {
            false
        }
    }
    fn value(&mut self) -> Result<JsonUnits, JsonUnitsError> {
        enum Frame {
            Array(Vec<JsonUnits>),
            Object(Vec<(Vec<u16>, JsonUnits)>, Vec<u16>),
        }
        let mut stack = Vec::new();
        loop {
            self.ws();
            let mut value = match self.source.as_bytes().get(self.pos).copied() {
                Some(b'"') => JsonUnits::String(self.string()?),
                Some(b'{') => {
                    self.pos += 1;
                    self.ws();
                    if self.eat(b'}') {
                        JsonUnits::Object(Vec::new())
                    } else {
                        if self.source.as_bytes().get(self.pos) != Some(&b'"') {
                            return Err(self.error("expected initial key"));
                        }
                        let key = self.string()?;
                        self.ws();
                        if !self.eat(b':') {
                            return Err(self.error("expected colon"));
                        }
                        stack.push(Frame::Object(Vec::new(), key));
                        continue;
                    }
                }
                Some(b'[') => {
                    self.pos += 1;
                    self.ws();
                    if self.eat(b']') {
                        JsonUnits::Array(Vec::new())
                    } else {
                        stack.push(Frame::Array(Vec::new()));
                        continue;
                    }
                }
                Some(b'n') => self.literal("null", JsonUnits::Null)?,
                Some(b't') => self.literal("true", JsonUnits::Bool(true))?,
                Some(b'f') => self.literal("false", JsonUnits::Bool(false))?,
                Some(b'-' | b'0'..=b'9') => self.number()?,
                _ => return Err(self.error("expected value")),
            };
            loop {
                let Some(frame) = stack.last_mut() else {
                    return Ok(value);
                };
                self.ws();
                let close = match frame {
                    Frame::Array(items) => {
                        items.push(value);
                        self.eat(b']')
                    }
                    Frame::Object(fields, key) => {
                        let key = std::mem::take(key);
                        if let Some((_, old)) = fields.iter_mut().find(|(k, _)| *k == key) {
                            *old = value;
                        } else {
                            fields.push((key, value));
                        }
                        self.eat(b'}')
                    }
                };
                if close {
                    value = match stack.pop().unwrap() {
                        Frame::Array(items) => JsonUnits::Array(items),
                        Frame::Object(fields, _) => JsonUnits::Object(fields),
                    };
                    continue;
                }
                if !self.eat(b',') {
                    return Err(self.error(match stack.last().unwrap() {
                        Frame::Array(_) => "expected array comma",
                        Frame::Object(_, _) => "expected object comma",
                    }));
                }
                if let Frame::Object(_, key) = stack.last_mut().unwrap() {
                    self.ws();
                    if self.source.as_bytes().get(self.pos) != Some(&b'"') {
                        return Err(self.error("expected following key"));
                    }
                    *key = self.string()?;
                    self.ws();
                    if !self.eat(b':') {
                        return Err(self.error("expected colon"));
                    }
                }
                break;
            }
        }
    }
    fn literal(&mut self, text: &str, value: JsonUnits) -> Result<JsonUnits, JsonUnitsError> {
        for expected in text.bytes() {
            if self.source.as_bytes().get(self.pos) != Some(&expected) {
                return Err(self.error("invalid literal"));
            }
            self.pos += 1;
        }
        Ok(value)
    }
    fn number(&mut self) -> Result<JsonUnits, JsonUnitsError> {
        let start = self.pos;
        self.eat(b'-');
        if !self.eat(b'0') {
            let Some(b'1'..=b'9') = self.source.as_bytes().get(self.pos) else {
                return Err(self.error("no number after minus"));
            };
            self.pos += 1;
            self.digits();
        }
        if self
            .source
            .as_bytes()
            .get(self.pos)
            .is_some_and(u8::is_ascii_digit)
        {
            return Err(self.error("unexpected number"));
        }
        if self.eat(b'.') {
            let at = self.pos;
            self.digits();
            if at == self.pos {
                return Err(self.error("expected fraction"));
            }
        }
        if self.eat(b'e') || self.eat(b'E') {
            let _ = self.eat(b'+') || self.eat(b'-');
            let at = self.pos;
            self.digits();
            if at == self.pos {
                return Err(self.error("expected exponent"));
            }
        }
        Ok(JsonUnits::Number(
            self.source[start..self.pos]
                .parse::<f64>()
                .map_err(|_| self.error("invalid number"))?,
        ))
    }
    fn digits(&mut self) {
        while self
            .source
            .as_bytes()
            .get(self.pos)
            .is_some_and(u8::is_ascii_digit)
        {
            self.pos += 1;
        }
    }
    fn string(&mut self) -> Result<Vec<u16>, JsonUnitsError> {
        if !self.eat(b'"') {
            return Err(self.error("expected string"));
        }
        let mut output = Vec::new();
        loop {
            let Some(&byte) = self.source.as_bytes().get(self.pos) else {
                return Err(self.error("unterminated string"));
            };
            self.pos += 1;
            match byte {
                b'"' => return Ok(output),
                b'\\' => {
                    let Some(&escape) = self.source.as_bytes().get(self.pos) else {
                        return Err(self.error("unterminated escape"));
                    };
                    self.pos += 1;
                    output.push(match escape {
                        b'"' => 34,
                        b'\\' => 92,
                        b'/' => 47,
                        b'b' => 8,
                        b'f' => 12,
                        b'n' => 10,
                        b'r' => 13,
                        b't' => 9,
                        b'u' => {
                            let mut unit = 0u16;
                            for _ in 0..4 {
                                let Some(&b) = self.source.as_bytes().get(self.pos) else {
                                    return Err(self.error("incomplete unicode escape"));
                                };
                                let digit = (b as char)
                                    .to_digit(16)
                                    .ok_or_else(|| self.error("invalid unicode escape"))?;
                                self.pos += 1;
                                unit = unit * 16 + digit as u16;
                            }
                            unit
                        }
                        _ => {
                            self.pos -= 1;
                            let character = self.source[self.pos..].chars().next().unwrap();
                            return Err(self.error(if character as u32 > 255 {
                                "unexpected escaped character"
                            } else {
                                "invalid escape"
                            }));
                        }
                    });
                }
                0..=31 => {
                    self.pos -= 1;
                    return Err(self.error("control character in string"));
                }
                _ => {
                    self.pos -= 1;
                    let ch = self.source[self.pos..].chars().next().unwrap();
                    self.pos += ch.len_utf8();
                    let mut buffer = [0; 2];
                    output.extend_from_slice(ch.encode_utf16(&mut buffer));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn escaped_units_do_not_collide_with_replacement_or_literal_backslash_keys() {
        let value = JsonUnits::parse(r#"{"\ud800":1,"�":2,"\\ud800":3,"\ud800\udc00":4}"#).unwrap();
        assert_eq!(value.property(&[0xd800]), Some(&JsonUnits::Number(1.0)));
        assert_eq!(value.property(&[0xfffd]), Some(&JsonUnits::Number(2.0)));
        assert_eq!(
            value.property(&units("\\ud800")),
            Some(&JsonUnits::Number(3.0))
        );
        assert_eq!(
            value.property(&[0xd800, 0xdc00]),
            Some(&JsonUnits::Number(4.0))
        );
        assert_eq!(JsonUnits::parse(&value.pretty()).unwrap(), value);
        assert!(value.to_value().is_none());
        assert!(value.pretty().contains("\\ud800\""));
        assert!(value.pretty().contains('𐀀'));
    }
    #[test]
    fn native_object_enumeration_and_duplicate_assignment_keep_distinct_positions() {
        let value =
            JsonUnits::parse(r#"{"z":1,"10":2,"2":3,"z":4,"01":5,"4294967295":6}"#).unwrap();
        assert_eq!(
            value.pretty(),
            "{\n  \"2\": 3,\n  \"10\": 2,\n  \"z\": 4,\n  \"01\": 5,\n  \"4294967295\": 6\n}"
        );
    }
    #[test]
    fn number_boundary_uses_javascript_precision_and_stringification() {
        let value = JsonUnits::parse("[9007199254740993,1e400,-0,0.0000001,1e21]").unwrap();
        assert_eq!(
            value.pretty(),
            "[\n  9007199254740992,\n  null,\n  0,\n  1e-7,\n  1e+21\n]"
        );
    }
    #[test]
    fn strict_json_rejects_yaml_and_incomplete_or_non_json_literals() {
        for text in [
            "{a: 1}",
            "{\"a\":1,}",
            "[1,]",
            "01",
            "-.1",
            "1.",
            "1e+",
            "NaN",
            "undefined",
            "\u{feff}{}",
            "\u{a0}{}",
            "\"\\u123\"",
            "\"\\x41\"",
            "\"a\nb\"",
        ] {
            assert!(JsonUnits::parse(text).is_err(), "{text:?}");
        }
    }
}
