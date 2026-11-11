//! Vault search queries over source text and metadata.
//! Matching order is supplied by the caller; the index is not the query grammar.

use std::ops::Range;

use regress::Regex;
use serde::Serialize;
use serde_yaml::Value;

use crate::Note;
#[path = "search_word_ranges.rs"]
mod word_ranges;

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct LineMatch {
    pub line: usize,
    pub text: String,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct SearchHit {
    pub file: String,
    pub matches: Vec<LineMatch>,
}

#[derive(Debug, Clone, thiserror::Error)]
pub enum QueryError {
    #[error("{0}")]
    Message(String),
    #[error("{kind}: {message}")]
    Exception { kind: &'static str, message: String },
}

#[derive(Debug, Default)]
struct RegexCacheState {
    poisoned: [bool; 2],
    reset_at: Option<std::time::Instant>,
}
/// Renderer-local rL dictionaries and the observed trailing 60s idle reset.
/// Arbitrary JS Object.prototype mutation remains a hosted-runtime contract.
#[derive(Debug, Default)]
pub struct RegexCacheRealm {
    state: std::cell::RefCell<RegexCacheState>,
}
impl RegexCacheRealm {
    /// Timer/event boundary also allows clock-controlled differential replay.
    pub fn poll_idle_at(&self, now: std::time::Instant) {
        let mut state = self.state.borrow_mut();
        if state.reset_at.is_some_and(|deadline| now >= deadline) {
            *state = RegexCacheState::default();
        }
    }
    fn begin_match(&self) {
        let now = std::time::Instant::now();
        self.poll_idle_at(now);
        self.state.borrow_mut().reset_at = Some(now + std::time::Duration::from_secs(60));
    }
    fn access(&self, source: &str, sensitive: bool) -> Result<(), QueryError> {
        let mut state = self.state.borrow_mut();
        let slot = usize::from(sensitive);
        if state.poisoned[slot] {
            return Err(QueryError::Exception {
                kind: "TypeError",
                message: "i.hasOwnProperty is not a function".into(),
            });
        }
        if source == "hasOwnProperty" {
            state.poisoned[slot] = true;
        }
        Ok(())
    }
}

#[derive(Debug, Clone)]
enum Expr {
    Pattern {
        source: String,
        literal: String,
        quoted: bool,
        regex_literal: bool,
    },
    And(Vec<Expr>),
    Or(Vec<Expr>),
    Not(Box<Expr>),
    Case(bool, Box<Expr>),
    Scope(String, Box<Expr>),
    Tag(String),
    Property(Box<Expr>, Option<Box<Expr>>),
    Compare {
        less: bool,
        text: String,
    },
    Typed(&'static str),
    Any,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Token {
    Text(String),
    Quote(String),
    Regex(String),
    Symbol(char),
    Or,
    Typed(&'static str),
}

fn escape_regex(text: &str) -> String {
    let mut escaped = String::new();
    for ch in text.chars() {
        if "\\^$.*+?()[]{}|".contains(ch) {
            escaped.push('\\');
        }
        escaped.push(ch);
    }
    escaped
}

fn tokenize(query: &str) -> Vec<Token> {
    let chars: Vec<char> = query.chars().collect();
    let mut at = 0;
    let mut tokens = Vec::new();
    while at < chars.len() {
        let first = chars[at];
        if first == ' ' {
            at += 1;
            continue;
        }
        if first == '"' || first == '/' {
            at += 1;
            let mut text = String::new();
            while at < chars.len() {
                let ch = chars[at];
                at += 1;
                if ch == first {
                    break;
                }
                if ch == '\\' && at < chars.len() {
                    let next = chars[at];
                    if first == '"' || next == '/' {
                        text.push(next);
                        at += 1;
                        continue;
                    }
                    if next == '\\' {
                        text.push_str("\\\\");
                        at += 1;
                        continue;
                    }
                }
                text.push(ch);
            }
            tokens.push(if first == '"' {
                Token::Quote(text)
            } else {
                Token::Regex(text)
            });
        } else if first == '-' {
            tokens.push(Token::Symbol('-'));
            at += 1;
        } else {
            let mut text = String::new();
            while at < chars.len() && chars[at] != ' ' && !"[]():<>".contains(chars[at]) {
                text.push(chars[at]);
                at += 1;
            }
            if !text.is_empty() {
                tokens.push(match text.as_str() {
                    "OR" => Token::Or,
                    "TRUE" => Token::Typed("TRUE"),
                    "FALSE" => Token::Typed("FALSE"),
                    "EMPTY" => Token::Typed("EMPTY"),
                    _ => Token::Text(text),
                });
            }
            if at < chars.len() {
                if chars[at] != ' ' {
                    tokens.push(Token::Symbol(chars[at]));
                }
                at += 1;
            }
        }
    }
    tokens
}

struct Parser {
    tokens: Vec<Token>,
    at: usize,
    scopes: Vec<String>,
    in_property: bool,
}

impl Parser {
    fn or(&mut self) -> Result<Option<Expr>, QueryError> {
        let mut items = Vec::new();
        loop {
            let Some(item) = self.and()? else { break };
            items.push(item);
            if self.tokens.get(self.at) != Some(&Token::Or) {
                break;
            }
            self.at += 1;
        }
        Ok(match items.len() {
            0 => None,
            1 => items.pop(),
            _ => Some(Expr::Or(items)),
        })
    }
    fn and(&mut self) -> Result<Option<Expr>, QueryError> {
        let mut items = Vec::new();
        while let Some(item) = self.one()? {
            items.push(item);
        }
        Ok(match items.len() {
            0 => None,
            1 => items.pop(),
            _ => Some(Expr::And(items)),
        })
    }
    fn one(&mut self) -> Result<Option<Expr>, QueryError> {
        let Some(token) = self.tokens.get(self.at).cloned() else {
            return Ok(None);
        };
        if matches!(token, Token::Symbol(']' | ')' | ':') | Token::Or) {
            return Ok(None);
        }
        self.at += 1;
        let expr = match token {
            Token::Text(text)
                if !self.in_property && self.tokens.get(self.at) == Some(&Token::Symbol(':')) =>
            {
                self.at += 1;
                let scope = text.to_lowercase();
                let exclusive = !matches!(scope.as_str(), "match-case" | "ignore-case");
                if !matches!(
                    scope.as_str(),
                    "match-case"
                        | "ignore-case"
                        | "path"
                        | "file"
                        | "content"
                        | "line"
                        | "block"
                        | "section"
                        | "task"
                        | "task-todo"
                        | "task-done"
                        | "tag"
                ) {
                    return Err(QueryError::Message(format!(
                        "Operator \"{scope}\" not recognized"
                    )));
                }
                if exclusive {
                    if let Some(parent) = self
                        .scopes
                        .iter()
                        .rev()
                        .find(|p| !matches!(p.as_str(), "match-case" | "ignore-case"))
                    {
                        if scope != "section" || parent != "section" {
                            return Err(QueryError::Message(format!(
                                "Operator \"{scope}\" cannot be nested within \"{parent}\""
                            )));
                        }
                    }
                }
                self.scopes.push(scope.clone());
                let inner = self.one()?.unwrap_or(Expr::Pattern {
                    source: String::new(),
                    literal: String::new(),
                    quoted: false,
                    regex_literal: false,
                });
                self.scopes.pop();
                match scope.as_str() {
                    "match-case" => Expr::Case(true, Box::new(inner)),
                    "ignore-case" => Expr::Case(false, Box::new(inner)),
                    "tag" => match inner {
                        Expr::Pattern {
                            source,
                            quoted: false,
                            regex_literal: false,
                            ..
                        } => Expr::Tag(source),
                        _ => {
                            return Err(QueryError::Message(
                                "Operator \"tag\" can only be followed by text".into(),
                            ))
                        }
                    },
                    _ => Expr::Scope(scope, Box::new(inner)),
                }
            }
            Token::Text(text) => Expr::Pattern {
                source: escape_regex(&text),
                literal: text,
                quoted: false,
                regex_literal: false,
            },
            Token::Quote(text) => Expr::Pattern {
                source: escape_regex(&text),
                literal: text,
                quoted: true,
                regex_literal: false,
            },
            Token::Regex(source) => {
                Regex::new(&non_unicode_pattern(&source)).map_err(|e| {
                    let detail = match e.to_string().as_str() {
                        "Unbalanced bracket" => "Unterminated character class".to_string(),
                        "Unbalanced parenthesis" => "Unterminated group".to_string(),
                        "Invalid quantifier" => "numbers out of order in {} quantifier".to_string(),
                        "Invalid atom character" => "Nothing to repeat".to_string(),
                        _ => e.to_string(),
                    };
                    QueryError::Message(format!("Failed to parse regular expression. Invalid regular expression: /{source}/: {detail}"))
                })?;
                Expr::Pattern {
                    literal: source.clone(),
                    source,
                    quoted: false,
                    regex_literal: true,
                }
            }
            Token::Symbol('(') => {
                let inner = self.or()?.unwrap_or(Expr::Pattern {
                    source: String::new(),
                    literal: String::new(),
                    quoted: false,
                    regex_literal: false,
                });
                if self.tokens.get(self.at) == Some(&Token::Symbol(')')) {
                    self.at += 1;
                }
                inner
            }
            Token::Symbol('[') => {
                if self.in_property {
                    return Err(QueryError::Message(
                        "Property cannot be nested within a property.".into(),
                    ));
                }
                self.in_property = true;
                let key = self.or()?.unwrap_or(Expr::Pattern {
                    source: String::new(),
                    literal: String::new(),
                    quoted: false,
                    regex_literal: false,
                });
                let value = if self.tokens.get(self.at) == Some(&Token::Symbol(':')) {
                    self.at += 1;
                    self.or()?.map(Box::new)
                } else {
                    None
                };
                self.in_property = false;
                if self.tokens.get(self.at) == Some(&Token::Symbol(']')) {
                    self.at += 1;
                }
                Expr::Property(Box::new(key), value)
            }
            Token::Symbol('-') => {
                let Some(inner) = self.one()? else {
                    return Ok(None);
                };
                Expr::Not(Box::new(inner))
            }
            Token::Symbol(ch @ ('<' | '>')) => {
                let Some(inner) = self.one()? else {
                    return Ok(None);
                };
                let Expr::Pattern {
                    literal,
                    regex_literal: false,
                    ..
                } = inner
                else {
                    return Err(QueryError::Message(format!(
                        "{} operator cannot be applied to matcher {}",
                        if ch == '<' { "lessthan" } else { "greaterthan" },
                        inner.info()
                    )));
                };
                Expr::Compare {
                    less: ch == '<',
                    text: literal,
                }
            }
            Token::Typed(expected) => Expr::Typed(expected),
            _ => return Err(QueryError::Message("Invalid search query.".into())),
        };
        Ok(Some(expr))
    }
}

#[derive(Default, Clone)]
struct Matched {
    content: Vec<Range<usize>>,
    content_present: bool,
}

impl Matched {
    fn extend(&mut self, other: Matched) {
        self.content_present |= other.content_present;
        self.content.extend(other.content);
    }
    fn ranges(content: Vec<Range<usize>>) -> Self {
        Self {
            content,
            content_present: true,
        }
    }
}

// The shipped matcher composes null, undefined and match objects separately.
// Empty quoted/regex matchers become match-all only at the final boundary.
enum Outcome {
    No,
    Unavailable,
    Match(Matched),
}

#[derive(Clone)]
struct Environment<'a> {
    note: &'a Note,
    cache: &'a crate::cache::CachedMetadata,
    positions: &'a crate::cache::SourcePositions<'a>,
    content: String,
    // Property primitives may contain unpaired UTF-16 surrogates. Keep their
    // code units through RegExp and relational comparison; UTF-8 replacement
    // belongs only at an output boundary.
    content_units: Option<Vec<u16>>,
    keys: Vec<&'static str>,
    property_name: String,
    property_name_units: Option<Vec<u16>>,
    typed: Option<Value>,
    case_sensitive: bool,
    sections: Option<(Vec<(usize, usize, usize)>, usize)>,
    base_byte: usize,
    patterns: &'a std::collections::BTreeMap<(String, bool), Regex>,
    realm: Option<&'a RegexCacheRealm>,
    failure: &'a std::cell::RefCell<Option<QueryError>>,
}

fn js_string(value: &Value) -> String {
    match value {
        Value::Null => "null".into(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => ecmascript_number(n.as_f64().unwrap()),
        Value::String(s) => s.clone(),
        Value::Sequence(items) => items
            .iter()
            .map(|v| {
                if v.is_null() {
                    String::new()
                } else {
                    js_string(v)
                }
            })
            .collect::<Vec<_>>()
            .join(","),
        Value::Mapping(_) => "[object Object]".into(),
        Value::Tagged(tag) => js_string(&tag.value),
    }
}

/// JavaScript Number string formatting, including rounding notation and NaN.
pub fn ecmascript_number(value: f64) -> String {
    ryu_js::Buffer::new().format(value).to_string()
}

fn u16_len(text: &str) -> usize {
    text.encode_utf16().count()
}
fn byte_to_u16(text: &str, byte: usize) -> usize {
    u16_len(&text[..byte.min(text.len())])
}
fn regex_matches_units(regex: &Regex, encoded: &[u16]) -> Vec<Range<usize>> {
    regex
        // The shipped matcher omits the `u` flag. UCS-2 advances one code unit
        // even across surrogate pairs; UTF-16 decoding would change dot and
        // quantified patterns into Unicode-code-point matching.
        .find_from_ucs2(encoded, 0)
        .filter_map(|m| {
            let range = m.range();
            if range.is_empty() {
                None
            } else {
                Some(range)
            }
        })
        .collect()
}

fn word_character(ch: Option<u16>) -> bool {
    let Some(ch) = ch else { return false };
    let index = word_ranges::WORD_RANGES.partition_point(|(_, end)| *end < ch);
    word_ranges::WORD_RANGES
        .get(index)
        .is_some_and(|(start, _)| *start <= ch)
}

// regress accepts braced codepoint escapes even without `u`. V8's non-u
// grammar treats an incomplete four-hex-digit escape as identity `u`.
pub(crate) fn non_unicode_pattern(source: &str) -> String {
    let mut output = String::new();
    let mut chars = source.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\\' {
            match chars.next() {
                Some('u') if chars.peek() == Some(&'{') => output.push('u'),
                Some(next) => {
                    output.push(ch);
                    output.push(next);
                }
                None => output.push(ch),
            }
        } else {
            output.push(ch);
        }
    }
    output
}

// Feed non-u source as code units, and prevent regress's Unicode escape parser
// from unconditionally joining adjacent escaped surrogate atoms. Its internal
// braced escape grammar represents a single surrogate without joining it; user
// braced escapes have already been converted to native non-u identity escapes.
fn non_unicode_pattern_units(source: &str) -> Vec<u32> {
    let prepared = non_unicode_pattern(source);
    let units: Vec<u16> = prepared.encode_utf16().collect();
    let mut output = Vec::new();
    let mut at = 0;
    while at < units.len() {
        if units[at] == b'\\' as u16 && at + 1 < units.len() {
            if units[at + 1] == b'u' as u16 && at + 5 < units.len() {
                let digits = &units[at + 2..at + 6];
                let parsed = digits.iter().try_fold(0u16, |value, unit| {
                    char::from_u32(u32::from(*unit))?
                        .to_digit(16)
                        .map(|digit| value * 16 + digit as u16)
                });
                if parsed.is_some_and(|unit| (0xd800..=0xdbff).contains(&unit)) {
                    output.extend([b'\\' as u32, b'u' as u32, b'{' as u32]);
                    output.extend(digits.iter().map(|unit| u32::from(*unit)));
                    output.push(b'}' as u32);
                    at += 6;
                    continue;
                }
            }
            output.extend([u32::from(units[at]), u32::from(units[at + 1])]);
            at += 2;
        } else {
            output.push(u32::from(units[at]));
            at += 1;
        }
    }
    output
}

fn merge_ranges(ranges: &mut Vec<Range<usize>>) {
    ranges.sort_by_key(|r| (r.start, r.end));
    let mut merged: Vec<Range<usize>> = Vec::new();
    for range in ranges.drain(..) {
        if let Some(last) = merged.last_mut() {
            if last.end >= range.start {
                last.end = last.end.max(range.end);
                continue;
            }
        }
        merged.push(range);
    }
    *ranges = merged;
}

impl Expr {
    fn info(&self) -> String {
        match self {
            Self::Pattern {
                source,
                regex_literal: true,
                ..
            } => format!("Matches regex: /{source}/"),
            Self::Pattern {
                literal,
                quoted: true,
                ..
            } => format!("Matches exact text: \"{literal}\""),
            Self::Pattern { literal, .. } => format!("Matches text: \"{literal}\""),
            Self::And(_) => "Match all of: ".into(),
            Self::Or(_) => "Match any of: ".into(),
            Self::Not(_) => "Excluding: ".into(),
            Self::Case(true, _) => "Case sensitive".into(),
            Self::Case(false, _) => "Case insensitive".into(),
            Self::Scope(scope, _) => match scope.as_str() {
                "file" => "Match file name: ",
                "path" => "Match file path: ",
                "content" => "Match file content: ",
                "line" => "Match line: ",
                "block" => "Match block: ",
                "section" => "Match section: ",
                "task" => "Match task: ",
                "task-todo" => "Match task (todo): ",
                "task-done" => "Match task (done): ",
                _ => unreachable!(),
            }
            .into(),
            Self::Tag(text) => format!(
                "Match tag: {}",
                if text.starts_with('#') {
                    text.clone()
                } else {
                    format!("#{text}")
                }
            ),
            Self::Property(_, _) => "Contains property:".into(),
            Self::Compare { less, text } => {
                format!("{} than: {text}", if *less { "Less" } else { "Greater" })
            }
            Self::Typed(expected) => match *expected {
                "TRUE" => "Is true",
                "FALSE" => "Is false",
                _ => "Is empty",
            }
            .into(),
            Self::Any => "Matches all".into(),
        }
    }
    fn patterns(
        &self,
        output: &mut std::collections::BTreeMap<(String, bool), Regex>,
    ) -> Result<(), QueryError> {
        match self {
            Self::Pattern { source, .. } => {
                for sensitive in [false, true] {
                    if !output.contains_key(&(source.clone(), sensitive)) {
                        let regex = Regex::from_unicode(
                            non_unicode_pattern_units(source).into_iter(),
                            if sensitive { "m" } else { "mi" },
                        )
                        .map_err(|e| {
                            QueryError::Message(format!("Failed to parse regular expression. {e}"))
                        })?;
                        output.insert((source.clone(), sensitive), regex);
                    }
                }
            }
            Self::And(items) | Self::Or(items) => {
                for item in items {
                    item.patterns(output)?;
                }
            }
            Self::Not(inner) | Self::Case(_, inner) | Self::Scope(_, inner) => {
                inner.patterns(output)?
            }
            Self::Property(key, value) => {
                key.patterns(output)?;
                if let Some(value) = value {
                    value.patterns(output)?;
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn evaluate(&self, env: &Environment<'_>) -> Outcome {
        use Outcome::{Match, No, Unavailable};
        if env.failure.borrow().is_some() {
            return No;
        }
        match self {
            Expr::Pattern {
                source,
                literal,
                quoted,
                regex_literal,
            } => {
                if source.is_empty() {
                    return if *quoted || *regex_literal {
                        Unavailable
                    } else {
                        No
                    };
                }
                if let Some(realm) = env.realm {
                    if let Err(error) = realm.access(source, env.case_sensitive) {
                        *env.failure.borrow_mut() = Some(error);
                        return No;
                    }
                }
                let mut matched = Matched::default();
                let mut any = false;
                let regex = &env.patterns[&(source.clone(), env.case_sensitive)];
                for key in &env.keys {
                    let filename = env
                        .note
                        .path
                        .file_name()
                        .unwrap_or_default()
                        .to_string_lossy();
                    let path = env.note.path.to_string_lossy();
                    let text = match *key {
                        "filename" => &*filename,
                        "filepath" => &*path,
                        "propertyName" => &env.property_name,
                        _ => &env.content,
                    };
                    let encoded;
                    let supplied_units = match *key {
                        "content" => env.content_units.as_deref(),
                        "propertyName" => env.property_name_units.as_deref(),
                        _ => None,
                    };
                    let units = if let Some(units) = supplied_units {
                        units
                    } else {
                        encoded = text.encode_utf16().collect::<Vec<_>>();
                        &encoded
                    };
                    let mut ranges = regex_matches_units(regex, units);
                    if *quoted && *key == "content" {
                        let literal_units: Vec<u16> = literal.encode_utf16().collect();
                        let left = word_character(literal_units.first().copied());
                        let right = word_character(literal_units.last().copied());
                        ranges.retain(|r| {
                            (!left
                                || r.start == 0
                                || !word_character(units.get(r.start - 1).copied()))
                                && (!right || !word_character(units.get(r.end).copied()))
                        });
                    }
                    if *quoted && *key == "propertyName" {
                        let literal_units = literal.encode_utf16().collect::<Vec<_>>();
                        match (
                            crate::collation::lowercase_units(units),
                            crate::collation::lowercase_units(&literal_units),
                        ) {
                            (Ok(left), Ok(right)) if left != right => ranges.clear(),
                            (Ok(_), Ok(_)) => {}
                            (Err(error), _) | (_, Err(error)) => {
                                *env.failure.borrow_mut() = Some(QueryError::Exception {
                                    kind: "Error",
                                    message: error.to_string(),
                                });
                                return No;
                            }
                        }
                    }
                    any |= !ranges.is_empty();
                    if *key == "content" && !ranges.is_empty() {
                        matched.content_present = true;
                        matched.content.extend(ranges);
                    }
                }
                if any {
                    Match(matched)
                } else {
                    No
                }
            }
            Expr::And(items) => {
                let mut result = Matched::default();
                let mut any = false;
                for item in items {
                    match item.evaluate(env) {
                        No => return No,
                        Unavailable => {}
                        Match(m) => {
                            any = true;
                            result.extend(m);
                        }
                    }
                }
                if any {
                    Match(result)
                } else {
                    Unavailable
                }
            }
            Expr::Or(items) => {
                let mut result = Matched::default();
                let mut any = false;
                let mut no = false;
                for item in items {
                    match item.evaluate(env) {
                        No => no = true,
                        Unavailable => {}
                        Match(m) => {
                            any = true;
                            result.extend(m);
                        }
                    }
                }
                if any {
                    Match(result)
                } else if no {
                    No
                } else {
                    Unavailable
                }
            }
            Expr::Not(inner) => match inner.evaluate(env) {
                No => Match(Matched::default()),
                Unavailable => Unavailable,
                Match(_) => No,
            },
            Expr::Case(sensitive, inner) => {
                let mut scoped = env.clone();
                scoped.case_sensitive = *sensitive;
                inner.evaluate(&scoped)
            }
            Expr::Scope(scope, inner) => {
                let mut scoped = env.clone();
                match scope.as_str() {
                    "file" => {
                        scoped.keys = vec!["filename"];
                        return inner.evaluate(&scoped);
                    }
                    "path" => {
                        scoped.keys = vec!["filepath"];
                        return inner.evaluate(&scoped);
                    }
                    "content" => {
                        scoped.keys = vec!["content"];
                        return inner.evaluate(&scoped);
                    }
                    _ => scoped.keys = vec!["content"],
                }
                let mut segments: Vec<(usize, usize, Option<usize>)> = Vec::new();
                let mut section_rows = Vec::new();
                if scope == "line" {
                    let mut offset = 0;
                    for line in env.content.split('\n') {
                        segments.push((offset, offset + line.len(), None));
                        offset += line.len() + 1;
                    }
                } else if scope == "block" {
                    for section in &env.cache.sections {
                        if section.kind != "list" {
                            if let (Some(start), Some(end)) = (
                                env.positions
                                    .byte_at_utf16_offset(section.position.start.offset),
                                env.positions
                                    .byte_at_utf16_offset(section.position.end.offset),
                            ) {
                                segments.push((start, end, None));
                            }
                        }
                    }
                    for item in &env.cache.list_items {
                        if let (Some(start), Some(end)) = (
                            env.positions
                                .byte_at_utf16_offset(item.position.start.offset),
                            env.positions.byte_at_utf16_offset(item.position.end.offset),
                        ) {
                            segments.push((start, end, None));
                        }
                    }
                } else if scope.starts_with("task") {
                    for item in &env.cache.list_items {
                        if let Some(status) = &item.task {
                            if scope == "task-todo" && status != " "
                                || scope == "task-done" && status == " "
                            {
                                continue;
                            }
                            if let (Some(start), Some(end)) = (
                                env.positions
                                    .byte_at_utf16_offset(item.position.start.offset),
                                env.positions.byte_at_utf16_offset(item.position.end.offset),
                            ) {
                                segments.push((start, end, None));
                            }
                        }
                    }
                } else if scope == "section" {
                    if let Some((sections, parent)) = &env.sections {
                        section_rows = sections.clone();
                        for index in
                            parent + 1..(parent + 1 + sections[*parent].2).min(sections.len())
                        {
                            let (start, end, _) = sections[index];
                            segments.push((start, end, Some(index)));
                        }
                    } else {
                        let headings = &env.cache.headings;
                        if let Some(first) = headings.first() {
                            let start = env
                                .positions
                                .byte_at_utf16_offset(first.position.start.offset)
                                .unwrap();
                            if start > 0 {
                                section_rows.push((0, start, 0));
                            }
                        }
                        for (i, heading) in headings.iter().enumerate() {
                            let start = env
                                .positions
                                .byte_at_utf16_offset(heading.position.start.offset)
                                .unwrap();
                            let end = headings
                                .get(i + 1)
                                .map(|h| {
                                    env.positions
                                        .byte_at_utf16_offset(h.position.start.offset)
                                        .unwrap()
                                })
                                .unwrap_or(env.note.source.len());
                            let children = headings
                                .iter()
                                .skip(i + 1)
                                .take_while(|h| h.level > heading.level)
                                .count();
                            section_rows.push((start, end, children));
                        }
                        if headings.is_empty() {
                            section_rows.push((0, env.note.source.len(), 0));
                        }
                        segments.extend(
                            section_rows
                                .iter()
                                .enumerate()
                                .map(|(i, (s, e, _))| (*s, *e, Some(i))),
                        );
                    }
                }
                let mut any = false;
                let mut unknown = false;
                let mut output = Matched::ranges(Vec::new());
                for (start, end, section_index) in segments {
                    let Some(segment) = env.note.source.get(start..end) else {
                        continue;
                    };
                    scoped.content = segment.to_string();
                    scoped.content_units = None;
                    scoped.base_byte = start;
                    if let Some(index) = section_index {
                        scoped.sections = Some((section_rows.clone(), index));
                    }
                    let matcher = if scope.starts_with("task")
                        && matches!(inner.as_ref(),Expr::Pattern { source,quoted:true,.. } if source.is_empty())
                    {
                        &Expr::Any
                    } else {
                        inner.as_ref()
                    };
                    match matcher.evaluate(&scoped) {
                        Match(mut matched) => {
                            any = true;
                            let offset = byte_to_u16(&env.note.source, start)
                                .saturating_sub(byte_to_u16(&env.note.source, env.base_byte));
                            if !matched.content_present {
                                output.content.push(offset..offset);
                            }
                            for range in &mut matched.content {
                                range.start += offset;
                                range.end += offset;
                            }
                            output.content.extend(matched.content);
                        }
                        Unavailable => unknown = true,
                        No => {}
                    }
                }
                if any {
                    Match(output)
                } else if unknown {
                    Unavailable
                } else {
                    No
                }
            }
            Expr::Tag(source) => {
                let source = if source.starts_with('#') {
                    source.clone()
                } else {
                    format!("#{source}")
                };
                let Ok(regex) = Regex::with_flags(&format!("^{source}($|/)"), "i") else {
                    return No;
                };
                let mut output = Matched::default();
                let mut any = false;
                for tag in &env.cache.tags {
                    if regex.find(&tag.tag).is_some() {
                        any = true;
                        output.content_present = true;
                        output
                            .content
                            .push(tag.position.start.offset..tag.position.end.offset);
                    }
                }
                if let Some(fm) = env
                    .note
                    .frontmatter
                    .as_ref()
                    .filter(|_| env.cache.frontmatter.is_some())
                {
                    any |= fm.tags().iter().any(|t| regex.find(t).is_some());
                }
                if any {
                    Match(output)
                } else {
                    No
                }
            }
            Expr::Property(key, value) => {
                if env.cache.frontmatter.is_none() && env.cache.native_frontmatter.is_none() {
                    return No;
                }
                // The rich document is authoritative. A UTF-8 convenience map
                // cannot enumerate or identify unpaired-surrogate property keys.
                let fields = if let Some(doc) = &env.cache.native_frontmatter {
                    doc.own_fields_units()
                        .into_iter()
                        .map(|(name, id)| (name, Some(id), None))
                        .collect::<Vec<_>>()
                } else if let Some(fm) = env.note.frontmatter.as_ref() {
                    fm.data
                        .iter()
                        .map(|(name, data)| (name.encode_utf16().collect(), None, Some(data)))
                        .collect()
                } else {
                    return No;
                };
                for (name, native_id, legacy_data) in fields {
                    let mut scoped = env.clone();
                    scoped.keys = vec!["propertyName"];
                    scoped.property_name.clear();
                    scoped.property_name_units = Some(name);
                    if !matches!(key.evaluate(&scoped), Match(_)) {
                        continue;
                    }
                    let Some(value) = value else {
                        return Match(Matched::default());
                    };
                    // Search receives native YAML constructor values, rather
                    // than their JSON projection or our legacy tagged payload.
                    // Date's relational primitive is numeric; other object
                    // constructors use their JavaScript string primitive.
                    let native_values = env.cache.native_frontmatter.as_ref().and_then(|doc| {
                        let id = native_id?;
                        use crate::yaml_value::Node;
                        let ids = match &doc.nodes[id] {
                            Node::Array(ids) => ids.clone(),
                            _ => vec![id],
                        };
                        Some(
                            ids.into_iter()
                                .filter_map(|id| {
                                    let content = doc.javascript_units(id).ok()?;
                                    let primitive = match &doc.nodes[id] {
                                        Node::Null => Value::Null,
                                        Node::Bool(v) => Value::Bool(*v),
                                        Node::Number(v) => Value::Number(v.clone()),
                                        Node::String(v) => Value::String(v.clone()),
                                        Node::Wtf16String(_) => Value::String(String::new()),
                                        Node::Date(ms) => Value::Number((*ms).into()),
                                        _ => Value::String(String::new()),
                                    };
                                    Some((content, primitive))
                                })
                                .collect::<Vec<_>>(),
                        )
                    });
                    let values = native_values.unwrap_or_else(|| {
                        let Some(data) = legacy_data else {
                            return Vec::new();
                        };
                        let values = match data {
                            Value::Sequence(v) => v.clone(),
                            _ => vec![data.clone()],
                        };
                        values
                            .into_iter()
                            .map(|data| (js_string(&data).encode_utf16().collect(), data))
                            .collect()
                    });
                    for (content, data) in values {
                        scoped.keys = vec!["content"];
                        scoped.content.clear();
                        scoped.content_units = Some(content);
                        scoped.typed = Some(data);
                        if matches!(value.evaluate(&scoped), Match(m) if !m.content.is_empty()) {
                            return Match(Matched::default());
                        }
                    }
                }
                No
            }
            Expr::Compare { less, text } => {
                let Some(value) = env.typed.as_ref().filter(|v| !v.is_null()) else {
                    return No;
                };
                // Abstract relational comparison keeps original strings lexical,
                // and coerces booleans/numbers when the right operand is text.
                let number = || js_number(text);
                let ordering = match value {
                    Value::String(s) => Some(match &env.content_units {
                        Some(units) => units.iter().copied().cmp(text.encode_utf16()),
                        None => s.encode_utf16().cmp(text.encode_utf16()),
                    }),
                    Value::Bool(b) => {
                        number().and_then(|n| f64::from(u8::from(*b)).partial_cmp(&n))
                    }
                    Value::Number(n) => n
                        .as_f64()
                        .and_then(|n| number().and_then(|r| n.partial_cmp(&r))),
                    Value::Mapping(_) | Value::Sequence(_) => {
                        Some(js_string(value).encode_utf16().cmp(text.encode_utf16()))
                    }
                    _ => None,
                };
                if ordering.is_some_and(|o| if *less { o.is_lt() } else { o.is_gt() }) {
                    Match(Matched::ranges(vec![
                        0..env
                            .content_units
                            .as_ref()
                            .map_or_else(|| u16_len(&js_string(value)), Vec::len),
                    ]))
                } else {
                    No
                }
            }
            Expr::Typed(expected) => {
                let data = env.typed.as_ref().unwrap_or(&Value::Null);
                let matched = match *expected {
                    "TRUE" => data == &Value::Bool(true),
                    "FALSE" => data == &Value::Bool(false),
                    _ => data.is_null(),
                };
                if matched {
                    Match(Matched::ranges(vec![0..u16_len(&js_string(data))]))
                } else {
                    No
                }
            }
            Expr::Any => {
                if env
                    .content_units
                    .as_ref()
                    .map_or_else(|| env.content.is_empty(), |units| units.is_empty())
                {
                    No
                } else {
                    Match(Matched::ranges(vec![0..0]))
                }
            }
        }
    }
}

/// ECMAScript WhiteSpace/LineTerminator membership, shared with CLI parseInt.
pub fn js_whitespace(ch: char) -> bool {
    matches!(ch, '\u{0009}'..='\u{000d}' | '\u{0020}' | '\u{00a0}' | '\u{1680}' | '\u{2000}'..='\u{200a}' | '\u{2028}' | '\u{2029}' | '\u{202f}' | '\u{205f}' | '\u{3000}' | '\u{feff}')
}

fn js_number(text: &str) -> Option<f64> {
    let text = text.trim_matches(js_whitespace);
    if text.is_empty() {
        return Some(0.0);
    }
    for (prefix, radix) in [
        ("0x", 16),
        ("0X", 16),
        ("0b", 2),
        ("0B", 2),
        ("0o", 8),
        ("0O", 8),
    ] {
        if let Some(digits) = text.strip_prefix(prefix) {
            return u64::from_str_radix(digits, radix).ok().map(|n| n as f64);
        }
    }
    if matches!(text, "Infinity" | "+Infinity") {
        return Some(f64::INFINITY);
    }
    if text == "-Infinity" {
        return Some(f64::NEG_INFINITY);
    }
    if !text
        .chars()
        .all(|ch| ch.is_ascii_digit() || matches!(ch, '+' | '-' | '.' | 'e' | 'E'))
    {
        return None;
    }
    text.parse::<f64>().ok().filter(|v| !v.is_nan())
}

/// Parse once, then match caller-ordered Markdown notes. Positions use UTF-16,
/// matching the reference's JavaScript RegExp and context line calculation.
pub fn search<'a>(
    notes: impl IntoIterator<Item = &'a Note>,
    query: &str,
    case_sensitive: bool,
) -> Result<Vec<SearchHit>, QueryError> {
    search_impl(
        notes.into_iter().map(|note| (note, None)),
        query,
        case_sensitive,
        None,
    )
}

/// Match retained engine metadata without rebuilding each cache object.
pub fn search_cached<'a>(
    notes: impl IntoIterator<Item = (&'a Note, &'a crate::cache::CachedMetadata)>,
    query: &str,
    case_sensitive: bool,
) -> Result<Vec<SearchHit>, QueryError> {
    search_impl(
        notes.into_iter().map(|(note, cache)| (note, Some(cache))),
        query,
        case_sensitive,
        None,
    )
}

/// Global metadata availability does not gate saved text search. A failed
/// metadata job still has a saved Note and can be parsed by the search worker.
pub fn search_with_optional_cache<'a>(
    notes: impl IntoIterator<Item = (&'a Note, Option<&'a crate::cache::CachedMetadata>)>,
    query: &str,
    case_sensitive: bool,
) -> Result<Vec<SearchHit>, QueryError> {
    search_impl(notes, query, case_sensitive, None)
}

pub fn search_in_realm<'a>(
    notes: impl IntoIterator<Item = (&'a Note, Option<&'a crate::cache::CachedMetadata>)>,
    query: &str,
    case_sensitive: bool,
    realm: &RegexCacheRealm,
) -> Result<Vec<SearchHit>, QueryError> {
    search_impl(notes, query, case_sensitive, Some(realm))
}

fn search_impl<'a>(
    notes: impl IntoIterator<Item = (&'a Note, Option<&'a crate::cache::CachedMetadata>)>,
    query: &str,
    case_sensitive: bool,
    realm: Option<&RegexCacheRealm>,
) -> Result<Vec<SearchHit>, QueryError> {
    let mut parser = Parser {
        tokens: tokenize(query),
        at: 0,
        scopes: Vec::new(),
        in_property: false,
    };
    let expr = parser
        .or()?
        .ok_or_else(|| QueryError::Message("Invalid search query.".into()))?;
    let mut patterns = std::collections::BTreeMap::new();
    expr.patterns(&mut patterns)?;
    let mut output = Vec::new();
    let failure = std::cell::RefCell::new(None);
    for (note, supplied_cache) in notes {
        // The reference adapter's text read removes a leading BOM before
        // search, while file-operation snapshots retain the on-disk bytes.
        let normalized;
        let note = if let Some(source) = note.source.strip_prefix('\u{feff}') {
            normalized = Note::parse(note.path.clone(), source.to_string());
            &normalized
        } else {
            note
        };
        let parsed_cache;
        let cache = if let Some(cache) = supplied_cache {
            cache
        } else {
            parsed_cache = crate::cache::CachedMetadata::try_from_note(note).unwrap_or_default();
            &parsed_cache
        };
        let positions = crate::cache::SourcePositions::new(&note.source);
        let env = Environment {
            note,
            cache,
            positions: &positions,
            content: note.source.clone(),
            content_units: None,
            keys: vec!["filename", "content"],
            property_name: String::new(),
            property_name_units: None,
            typed: None,
            case_sensitive,
            sections: None,
            base_byte: 0,
            patterns: &patterns,
            realm,
            failure: &failure,
        };
        if let Some(realm) = realm {
            realm.begin_match();
        }
        let outcome = expr.evaluate(&env);
        if let Some(error) = failure.borrow_mut().take() {
            return Err(error);
        }
        let matched = match outcome {
            Outcome::No => continue,
            Outcome::Unavailable => Matched::default(),
            Outcome::Match(m) => m,
        };
        {
            let mut matched = matched;
            merge_ranges(&mut matched.content);
            let source: Vec<u16> = note.source.encode_utf16().collect();
            let lines: Vec<&str> = note.source.split('\n').collect();
            let matches = matched
                .content
                .iter()
                .map(|range| {
                    let line = source[..range.start.min(source.len())]
                        .iter()
                        .filter(|c| **c == 10)
                        .count()
                        + 1;
                    LineMatch {
                        line,
                        text: lines
                            .get(line - 1)
                            .copied()
                            .unwrap_or_default()
                            .trim_matches(js_whitespace)
                            .to_string(),
                    }
                })
                .collect();
            output.push(SearchHit {
                file: note.path.to_string_lossy().replace('\\', "/"),
                matches,
            });
        }
    }
    Ok(output)
}
