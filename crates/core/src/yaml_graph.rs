//! Typed, identity-preserving YAML data. Parser and projections are kept apart.
mod raw_source;
mod template_merge;
pub mod sdk;
mod wtf16;
use crate::yaml_value::{Diagnostic, LinePosition, Node, NodeId, ObjectKey};
use serde_json::{json, Value as Json};
use serde_yaml::{Mapping, Value};
use std::collections::{BTreeMap, BTreeSet};
use yaml_rust2::{
    parser::{Event, Parser, Tag},
    scanner::TScalarStyle,
};
#[derive(Clone, Debug)]
pub struct Document {
    pub root: NodeId,
    pub nodes: Vec<Node>,
    pub warnings: Vec<Diagnostic>,
    /// Plain console.warn messages are distinct from YAMLWarning objects.
    pub console_warnings: Vec<Vec<u16>>,
    own_utf16_properties: BTreeMap<NodeId, Vec<(ObjectKey, NodeId)>>,
    flow_quoted: BTreeSet<NodeId>,
    flow_single_quoted: BTreeSet<NodeId>,
    flow_aliases: BTreeMap<NodeId, BTreeMap<usize, Vec<u16>>>,
    flow_children: BTreeMap<NodeId, Vec<NodeId>>,
    own_properties: BTreeMap<NodeId, Vec<(String, NodeId)>>,
}
impl Document {
    /// Lossless native string/Array<string> property extraction.
    pub fn frontmatter_string_units(&self, property: &str) -> Vec<Vec<u16>> {
        let fields = self.own_fields_units();
        let Some((_, id)) = fields.iter().find(|(key, _)| {
            String::from_utf16(key).is_ok_and(|key| key.eq_ignore_ascii_case(property))
        }) else {
            return Vec::new();
        };
        let ids = match &self.nodes[*id] {
            Node::String(_) | Node::Wtf16String(_) => vec![*id],
            Node::Array(values) => values
                .iter()
                .copied()
                .filter(|id| matches!(self.nodes[*id], Node::String(_) | Node::Wtf16String(_)))
                .collect(),
            _ => return Vec::new(),
        };
        ids.into_iter()
            .map(|id| {
                let units = self.javascript_units(id).unwrap();
                let white = |unit: &u16| {
                    char::from_u32(*unit as u32).is_some_and(crate::vault_search::js_whitespace)
                };
                let start = units
                    .iter()
                    .position(|unit| !white(unit))
                    .unwrap_or(units.len());
                let end = units
                    .iter()
                    .rposition(|unit| !white(unit))
                    .map_or(start, |index| index + 1);
                units[start..end].to_vec()
            })
            .collect()
    }
    /// Explicit UTF8 presentation convenience. SDK values use the units method.
    pub fn frontmatter_strings(&self, property: &str) -> Vec<String> {
        self.frontmatter_string_units(property)
            .iter()
            .map(|units| String::from_utf16_lossy(units))
            .collect()
    }
}

struct Frame {
    id: NodeId,
    map: bool,
    children: Vec<(NodeId, bool)>,
    tag: Option<Tag>,
    mark: usize,
    key_aliases: BTreeMap<usize, Vec<u16>>,
    aliases: BTreeMap<usize, Vec<u16>>,
}
#[derive(Default)]
struct Anchor {
    id: NodeId,
    count: usize,
    weight: usize,
}
struct Builder<'a> {
    source: &'a str,
    raw_source: Option<&'a raw_source::Source>,
    doc: Document,
    frames: Vec<Frame>,
    anchors: BTreeMap<usize, Anchor>,
    edges: BTreeMap<NodeId, Vec<(NodeId, bool)>>,
    schema11: bool,
    marks: BTreeMap<NodeId, usize>,
    ast_error: Option<Diagnostic>,
    conversion_errors: BTreeMap<NodeId, Diagnostic>,
    mapping_nodes: BTreeSet<NodeId>,
}
/// A console argument remains either a YAMLWarning object or a native string.
#[derive(Clone, Debug)]
pub enum ConsoleWarning {
    Yaml(Diagnostic),
    Text(Vec<u16>),
}
/// Console calls precede both successful return and a thrown parse error.
#[derive(Clone, Debug)]
pub struct ParseOutcome {
    pub warnings: Vec<ConsoleWarning>,
    pub result: Result<Document, Diagnostic>,
}
/// JavaScript source input, preserving every UTF16 unit and source position.
pub fn parse_units_outcome(units: &[u16]) -> ParseOutcome {
    if let Some(raw) = raw_source::Source::new(units) {
        let mut outcome = parse_impl(&raw.text, Some(&raw));
        outcome.result = outcome.result.map_err(|error| raw.exception(error));
        outcome
    } else {
        parse_impl(
            &String::from_utf16(units).expect("validated source units"),
            None,
        )
    }
}
pub fn parse_units(units: &[u16]) -> Result<Document, Diagnostic> {
    parse_units_outcome(units).result
}
pub fn parse(source: &str) -> Result<Document, Diagnostic> {
    if source.contains('\0') {
        return parse_units(&source.encode_utf16().collect::<Vec<_>>());
    }
    parse_impl(source, None).result
}
fn parse_impl<'a>(source: &'a str, raw_source: Option<&'a raw_source::Source>) -> ParseOutcome {
    let mut builder = Builder {
        source,
        raw_source,
        doc: Document {
            root: 0,
            nodes: Vec::new(),
            warnings: Vec::new(),
            console_warnings: Vec::new(),
            own_utf16_properties: BTreeMap::new(),
            flow_quoted: BTreeSet::new(),
            flow_single_quoted: BTreeSet::new(),
            flow_aliases: BTreeMap::new(),
            flow_children: BTreeMap::new(),
            own_properties: BTreeMap::new(),
        },
        frames: Vec::new(),
        anchors: BTreeMap::new(),
        edges: BTreeMap::new(),
        schema11: false,
        marks: BTreeMap::new(),
        ast_error: None,
        conversion_errors: BTreeMap::new(),
        mapping_nodes: BTreeSet::new(),
    };
    for line in source.lines() {
        if let Some(version) = line.strip_prefix("%YAML ") {
            let version = version.trim();
            builder.schema11 = version == "1.1";
            if !matches!(version, "1.1" | "1.2") {
                let start = source.find(version).unwrap();
                let warning = builder.diag(
                    start,
                    start + version.len(),
                    "YAMLWarning",
                    "BAD_DIRECTIVE",
                    &format!("Unsupported YAML version {version}"),
                );
                builder.doc.warnings.push(warning);
            }
        }
    }
    let mut prepared = wtf16::prepare(source);
    let unresolved_aliases = prepared.recover_undefined_aliases();
    let build = (|| -> Result<(), Diagnostic> {
        let byte_marks = &prepared.marks;
        let mut parser = Parser::new_from_str(&prepared.source);
        let mut documents = 0;
        loop {
            let (event, mark) = parser.next_token().map_err(|error| {
            let position = byte_marks.get(error.marker().index()).copied().unwrap_or(source.len());
            if error.info().contains("found unknown anchor") {
                let name = source[position..].strip_prefix('*').unwrap_or(&source[position..]).split(|c:char| c.is_whitespace() || matches!(c,','|']'|'}')).next().unwrap_or("");
                return Diagnostic::exception("ReferenceError",&format!("Unresolved alias (the anchor must be set before the alias): {name}"));
            }
            if let Some(raw)=raw_source {
                if error.info().contains("tag")&&error.info().contains("whitespace"){
                    let original=raw.position(position);
                    // The native lexer points at the unsupported unit, while
                    // the Rust scanner reports the tag opener.
                    let original=raw.units.iter().enumerate().skip(original).take_while(|(_,unit)|!matches!(**unit,9|10|13|32)).find(|(_,unit)|**unit==0||(0xd800..=0xdfff).contains(*unit)).map_or(original,|(index,_)|index);
                    return raw_source::diagnostic(&raw.units,original,original+1,"YAMLParseError","MISSING_CHAR",&"Tags and anchors must be separated from the next token by white space".encode_utf16().collect::<Vec<_>>());
                }
                if error.info().contains("quoted scalar")&&error.info().contains("unexpected end") {
                    return raw_source::diagnostic(&raw.units,raw.units.len(),raw.units.len()+1,"YAMLParseError","MISSING_CHAR",&"Missing closing \"quote".encode_utf16().collect::<Vec<_>>());
                }
            }
            let flow = if position == source.len() && source.ends_with('\n') {
                if error.info().contains("flow sequence") { Some(("sequence", "]")) }
                else if error.info().contains("flow mapping") { Some(("map", "}")) }
                else if raw_source.is_some() {builder.frames.last().map(|frame|if frame.map{("map","}")}else{("sequence","]")})}
                else { None }
            } else { None };
            if let Some((collection, closing)) = flow {
                builder.diag(position, position + 1,"YAMLParseError","BAD_INDENT",&format!("Flow {collection} in block collection must be sufficiently indented and end with a {closing}"))
            } else {
                builder.diag(position,byte_marks.get(error.marker().index()+1).copied().unwrap_or(source.len()),"YAMLParseError","UNEXPECTED_TOKEN",error.info())
            }
        })?;
            let mark = byte_marks
                .get(mark.index())
                .copied()
                .unwrap_or(source.len());
            let is_map = matches!(&event, Event::MappingStart(..));
            match event {
                Event::StreamEnd => break,
                Event::DocumentStart => {
                    documents += 1;
                    if documents > 1 {
                        return Err(builder.diag(
                        mark,
                        mark + 3,
                        "YAMLParseError",
                        "MULTIPLE_DOCS",
                        "Source contains multiple documents; please use YAML.parseAllDocuments()",
                    ));
                    }
                }
                Event::Scalar(text, style, anchor, tag) => {
                    let id = if let Some(name) = unresolved_aliases.get(&mark) {
                        let id = builder.doc.nodes.len();
                        builder.doc.nodes.push(Node::Null);
                        let name =
                            raw_source.map_or_else(|| name.clone(), |source| source.restore(name));
                        let prefix = "Unresolved alias (the anchor must be set before the alias): ";
                        let mut error = Diagnostic::exception(
                            "ReferenceError",
                            &format!("{prefix}{}", String::from_utf16_lossy(&name)),
                        );
                        if String::from_utf16(&name).is_err() {
                            let mut units: Vec<u16> = prefix.encode_utf16().collect();
                            units.extend(name);
                            error.message_units = Some(units);
                        }
                        builder.conversion_errors.insert(id, error);
                        id
                    } else {
                        match builder.scalar(&text, style, tag.as_ref(), mark) {
                            Ok(id) => id,
                            Err(error) => {
                                builder.ast_error.get_or_insert(error);
                                let id = builder.doc.nodes.len();
                                builder.doc.nodes.push(Node::String(text.clone()));
                                id
                            }
                        }
                    };
                    if matches!(builder.doc.nodes[id], Node::String(_)) {
                        if let Some(units) = prepared.restore(&text) {
                            builder.doc.nodes[id] = match String::from_utf16(&units) {
                                Ok(text) => Node::String(text),
                                Err(_) => Node::Wtf16String(units),
                            };
                        }
                    }
                    if let Some(raw) = raw_source {
                        if matches!(
                            builder.doc.nodes[id],
                            Node::String(_) | Node::Wtf16String(_)
                        ) {
                            let original = builder.doc.javascript_units(id)?;
                            let units = raw.restore(&original);
                            builder.doc.nodes[id] = match String::from_utf16(&units) {
                                Ok(text) => Node::String(text),
                                Err(_) => Node::Wtf16String(units),
                            };
                        }
                    }
                    if matches!(style, TScalarStyle::DoubleQuoted) {
                        builder.doc.flow_quoted.insert(id);
                    } else if matches!(style, TScalarStyle::SingleQuoted) {
                        builder.doc.flow_single_quoted.insert(id);
                    }
                    builder.anchor(anchor, id);
                    builder.add(id, unresolved_aliases.contains_key(&mark));
                }
                Event::SequenceStart(anchor, tag) | Event::MappingStart(anchor, tag) => {
                    let map = is_map;
                    builder.collection_warning(tag.as_ref(), map, mark);
                    let id = builder.doc.nodes.len();
                    builder.doc.nodes.push(if map {
                        Node::Object(Vec::new())
                    } else {
                        Node::Array(Vec::new())
                    });
                    builder.marks.insert(id, mark);
                    if map {
                        builder.mapping_nodes.insert(id);
                    }
                    builder.anchor(anchor, id);
                    builder.frames.push(Frame {
                        id,
                        map,
                        children: Vec::new(),
                        tag,
                        mark: mark,
                        key_aliases: BTreeMap::new(),
                        aliases: BTreeMap::new(),
                    });
                }
                Event::SequenceEnd | Event::MappingEnd => {
                    let frame = builder.frames.pop().unwrap();
                    builder.edges.insert(frame.id, frame.children.clone());
                    builder
                        .doc
                        .flow_aliases
                        .insert(frame.id, frame.aliases.clone());
                    builder
                        .doc
                        .flow_children
                        .insert(frame.id, frame.children.iter().map(|(id, _)| *id).collect());
                    let node = match builder.collection(&frame) {
                        Ok(node) => node,
                        Err(error) => {
                            if error.name == "YAMLParseError" {
                                builder.ast_error.get_or_insert(error);
                            } else {
                                builder.conversion_errors.insert(frame.id, error);
                            }
                            if frame.map {
                                Node::Object(Vec::new())
                            } else {
                                Node::Array(Vec::new())
                            }
                        }
                    };
                    builder.doc.nodes[frame.id] = node;
                    builder.add(frame.id, false);
                }
                Event::Alias(anchor) => {
                    if let Some(frame) = builder.frames.last_mut() {
                        let raw = &source[mark..];
                        let end = raw
                            .find(|c: char| c.is_whitespace() || matches!(c, ',' | ']' | '}'))
                            .unwrap_or(raw.len());
                        let alias: Vec<u16> = raw[..end].encode_utf16().collect();
                        let alias = raw_source
                            .map_or_else(|| alias.clone(), |source| source.restore(&alias));
                        frame.aliases.insert(frame.children.len(), alias.clone());
                        if frame.map && frame.children.len() % 2 == 0 {
                            frame.key_aliases.insert(frame.children.len() / 2, alias);
                        }
                    }
                    let id = builder.anchors[&anchor].id;
                    let weight = builder.alias_weight(id, &mut BTreeSet::new());
                    let record = builder.anchors.get_mut(&anchor).unwrap();
                    record.count += 1;
                    if record.weight == 0 {
                        record.weight = weight;
                    }
                    if record.count.saturating_mul(record.weight) > 100 {
                        let id = builder.doc.nodes.len();
                        builder.doc.nodes.push(Node::Null);
                        builder.conversion_errors.insert(
                            id,
                            Diagnostic::exception(
                                "ReferenceError",
                                "Excessive alias count indicates a resource exhaustion attack",
                            ),
                        );
                        builder.add(id, true);
                        continue;
                    }
                    builder.add(id, true);
                }
                _ => {}
            }
        }
        if builder.doc.nodes.is_empty() {
            builder.doc.nodes.push(Node::Null);
        }
        Ok(())
    })();
    let mut warnings: Vec<_> = builder
        .doc
        .warnings
        .iter()
        .cloned()
        .map(ConsoleWarning::Yaml)
        .collect();
    if let Some(error) = builder.ast_error.clone().or_else(|| build.err()) {
        return ParseOutcome {
            warnings,
            result: Err(error),
        };
    }
    builder.doc.console_warnings.clear();
    let conversion = builder.convert_console(builder.doc.root, &mut BTreeSet::new());
    warnings.extend(
        builder
            .doc
            .console_warnings
            .iter()
            .cloned()
            .map(ConsoleWarning::Text),
    );
    if let Err(error) = conversion {
        return ParseOutcome {
            warnings,
            result: Err(error),
        };
    }
    // Uint8Array numeric indices are enumerable own properties, while Map,
    // Set and Date start with no enumerable own properties.
    let constructors: Vec<_> = builder
        .doc
        .nodes
        .iter()
        .enumerate()
        .filter_map(|(id, node)| match node {
            Node::Binary(bytes) => Some((id, Some(bytes.clone()))),
            Node::Date(_) | Node::Map(_) | Node::Set(_) => Some((id, None)),
            _ => None,
        })
        .collect();
    for (id, bytes) in constructors {
        let mut fields = Vec::new();
        if let Some(bytes) = bytes {
            for (index, byte) in bytes.into_iter().enumerate() {
                let node = builder.doc.nodes.len();
                builder.doc.nodes.push(Node::Number(byte.into()));
                fields.push((index.to_string(), node));
            }
        }
        builder.doc.own_properties.insert(id, fields);
    }
    ParseOutcome {
        warnings,
        result: Ok(builder.doc),
    }
}
impl Builder<'_> {
    fn convert_console(
        &mut self,
        id: NodeId,
        seen: &mut BTreeSet<NodeId>,
    ) -> Result<(), Diagnostic> {
        if !seen.insert(id) {
            return Ok(());
        }
        let children = self.doc.flow_children.get(&id).cloned().unwrap_or_default();
        if self.mapping_nodes.contains(&id) {
            let as_object = matches!(
                self.doc.nodes[id],
                Node::Object(_) | Node::ObjectWithUtf16Keys(_)
            );
            for (index, pair) in children.chunks_exact(2).enumerate() {
                self.convert_console(pair[0], seen)?;
                let object_key = matches!(
                    self.doc.nodes[pair[0]],
                    Node::Array(_)
                        | Node::Object(_)
                        | Node::ObjectWithUtf16Keys(_)
                        | Node::Date(_)
                        | Node::Binary(_)
                        | Node::Map(_)
                        | Node::Set(_)
                );
                if as_object && object_key && self.doc.console_warnings.is_empty() {
                    let alias = self
                        .doc
                        .flow_aliases
                        .get(&id)
                        .and_then(|a| a.get(&(index * 2)));
                    let key = match alias {
                        Some(alias) => alias.clone(),
                        None => self.doc.key_units(pair[0])?,
                    };
                    let mut quoted: Vec<u16> = crate::yaml_value::quote_units(&key)
                        .encode_utf16()
                        .collect();
                    if quoted.len() > 40 {
                        quoted.truncate(36);
                        quoted.extend("...\"".encode_utf16());
                    }
                    let mut message:Vec<u16>="Keys with collection values will be stringified due to JS Object restrictions: ".encode_utf16().collect();
                    message.extend(quoted);
                    message.extend(". Set mapAsMap: true to use object keys.".encode_utf16());
                    self.doc.console_warnings.push(message);
                }
                self.convert_console(pair[1], seen)?;
            }
        } else {
            for child in children {
                self.convert_console(child, seen)?;
            }
        }
        if let Some(error) = self.conversion_errors.get(&id) {
            return Err(error.clone());
        }
        Ok(())
    }
    fn add(&mut self, id: NodeId, alias: bool) {
        if let Some(frame) = self.frames.last_mut() {
            frame.children.push((id, alias));
            self.edges.insert(frame.id, frame.children.clone());
        } else {
            self.doc.root = id;
        }
    }
    fn anchor(&mut self, anchor: usize, id: NodeId) {
        if anchor > 0 {
            self.anchors.insert(
                anchor,
                Anchor {
                    id,
                    count: 1,
                    weight: 0,
                },
            );
        }
    }
    fn alias_weight(&self, id: NodeId, seen: &mut BTreeSet<NodeId>) -> usize {
        if !seen.insert(id) {
            return 0;
        }
        let result = if let Some(edges) = self.edges.get(&id) {
            edges
                .iter()
                .map(|(child, alias)| {
                    if *alias {
                        self.anchors
                            .values()
                            .find(|a| a.id == *child)
                            .map(|a| a.count.saturating_mul(a.weight))
                            .unwrap_or(0)
                    } else {
                        self.alias_weight(*child, seen)
                    }
                })
                .max()
                .unwrap_or(0)
        } else {
            1
        };
        seen.remove(&id);
        result
    }
    fn tag_name(tag: &Tag) -> String {
        format!("{}{}", tag.handle, tag.suffix)
    }
    fn tag_span(&self, mark: usize) -> (usize, usize) {
        let mark = mark.min(self.source.len());
        let prefix = &self.source[..mark];
        let Some(bang) = prefix.rfind('!') else {
            return (mark, mark + 1);
        };
        let token_start = prefix[..bang]
            .rfind(|c: char| c.is_whitespace() || matches!(c, '[' | '{' | ',' | ']'))
            .map_or(0, |i| i + 1);
        let start = token_start + self.source[token_start..].find('!').unwrap();
        if self.source[start..].starts_with("!<") {
            return (
                start,
                self.source[start..]
                    .find('>')
                    .map_or(mark, |end| start + end + 1),
            );
        }
        let len = self.source[start..]
            .chars()
            .take_while(|c| !c.is_whitespace() && !matches!(c, ',' | '[' | ']' | '{' | '}'))
            .map(char::len_utf8)
            .sum::<usize>();
        (start, start + len)
    }
    fn diag(&self, start: usize, end: usize, name: &str, code: &str, message: &str) -> Diagnostic {
        if let Some(raw) = self.raw_source {
            return raw.diagnostic(start, end, name, code, message);
        }
        // Gv in the pinned bundle formats source context in JavaScript UTF16
        // units, independently of the parser's character/byte coordinates.
        let boundary = |index: usize| {
            let mut index = index.min(self.source.len());
            while !self.source.is_char_boundary(index) {
                index -= 1;
            }
            index
        };
        let start = boundary(start);
        let end_extra = end.saturating_sub(self.source.len());
        let end = boundary(end).max(start);
        let locate = |position: usize| {
            let line = self.source[..position]
                .bytes()
                .filter(|b| *b == b'\n')
                .count()
                + 1;
            let line_start = self.source[..position].rfind('\n').map_or(0, |p| p + 1);
            LinePosition {
                line,
                col: self.source[line_start..position].encode_utf16().count() + 1,
            }
        };
        let first = locate(start);
        let mut last = locate(end);
        last.col += end_extra;
        let line_start = self.source[..start].rfind('\n').map_or(0, |p| p + 1);
        let line_end = self.source[start..]
            .find('\n')
            .map_or(self.source.len(), |p| start + p + 1);
        let mut text: Vec<u16> = self.source[line_start..line_end]
            .trim_end_matches(['\n', '\r'])
            .encode_utf16()
            .collect();
        let mut column = first.col - 1;
        if column >= 60 && text.len() > 80 {
            let cut = (column - 39).min(text.len() - 79);
            text = std::iter::once('…' as u16)
                .chain(text[cut..].iter().copied())
                .collect();
            column -= cut - 1;
        }
        if text.len() > 80 {
            text.truncate(79);
            text.push('…' as u16);
        }
        if first.line > 1
            && text[..column.min(text.len())]
                .iter()
                .all(|c| *c == b' ' as u16)
        {
            let previous_start = self.source[..line_start - 1]
                .rfind('\n')
                .map_or(0, |p| p + 1);
            let mut previous: Vec<_> = self.source[previous_start..line_start]
                .encode_utf16()
                .collect();
            if previous.len() > 80 {
                previous.truncate(79);
                previous.extend(['…' as u16, '\n' as u16]);
            }
            previous.extend(text);
            text = previous;
        }
        let mut pretty = format!("{message} at line {}, column {}", first.line, first.col);
        let mut pretty_units: Vec<u16> = pretty.encode_utf16().collect();
        if text.iter().any(|c| *c != b' ' as u16) {
            let width = if last.line == first.line && last.col > first.col {
                (last.col - first.col)
                    .min(80usize.saturating_sub(column))
                    .max(1)
            } else {
                1
            };
            pretty_units.extend(":\n\n".encode_utf16());
            pretty_units.extend(&text);
            pretty_units
                .extend(format!("\n{}{}\n", " ".repeat(column), "^".repeat(width)).encode_utf16());
            pretty += &format!(
                ":\n\n{}\n{}{}\n",
                String::from_utf16_lossy(&text),
                " ".repeat(column),
                "^".repeat(width)
            );
        }
        Diagnostic {
            name: name.into(),
            message: pretty,
            message_units: String::from_utf16(&pretty_units)
                .is_err()
                .then_some(pretty_units),
            code: Some(code.into()),
            pos: Some([
                self.source[..start].encode_utf16().count(),
                self.source[..end].encode_utf16().count() + end_extra,
            ]),
            line_pos: Some([first, last]),
        }
    }
    fn tag_error(&self, mark: usize, message: &str) -> Diagnostic {
        let (a, b) = self.tag_span(mark);
        self.diag(a, b, "YAMLParseError", "TAG_RESOLVE_FAILED", message)
    }
    fn warning(&mut self, mark: usize, tag: &Tag) {
        let (a, b) = self.tag_span(mark);
        let message = format!("Unresolved tag: {}", Self::tag_name(tag));
        let warning = self.diag(a, b, "YAMLWarning", "TAG_RESOLVE_FAILED", &message);
        self.doc.warnings.push(warning);
    }
    fn scalar(
        &mut self,
        text: &str,
        style: TScalarStyle,
        tag: Option<&Tag>,
        mark: usize,
    ) -> Result<NodeId, Diagnostic> {
        let name = tag.map(Self::tag_name);
        let standard = name
            .as_deref()
            .and_then(|s| s.strip_prefix("tag:yaml.org,2002:"));
        let node = match standard {
            Some("str") => Node::String(text.into()),
            Some("timestamp") => Node::Date(timestamp(text).ok_or_else(|| {
                self.tag_error(mark, "!!timestamp expects a date, starting with yyyy-mm-dd")
            })?),
            Some("binary") => {
                use base64::Engine;
                let value = text
                    .chars()
                    .filter(|c| !c.is_ascii_whitespace())
                    .collect::<String>();
                let config = base64::engine::GeneralPurposeConfig::new()
                    .with_decode_padding_mode(base64::engine::DecodePaddingMode::Indifferent)
                    .with_decode_allow_trailing_bits(true);
                let bytes=base64::engine::GeneralPurpose::new(&base64::alphabet::STANDARD,config).decode(value).map_err(|_|self.tag_error(mark,"Failed to execute 'atob' on 'Window': The string to be decoded is not correctly encoded."))?;
                Node::Binary(bytes)
            }
            Some("merge") => Node::Merge,
            Some(kind @ ("int" | "float" | "bool" | "null")) => {
                let value = resolve_scalar(text, self.schema11);
                let matches = match (&value, kind) {
                    (Node::Number(_), "int") => {
                        let patterns: &[usize] = if self.schema11 {
                            &[5, 6, 7, 8, 11]
                        } else {
                            &[0, 1, 2]
                        };
                        patterns.iter().any(|index| scalar_pattern(*index, text))
                    }
                    (Node::Number(_), "float") => {
                        let patterns: &[usize] = if self.schema11 { &[9, 10, 12] } else { &[3, 4] };
                        patterns.iter().any(|index| scalar_pattern(*index, text))
                            || matches!(
                                text,
                                ".inf"
                                    | ".Inf"
                                    | ".INF"
                                    | "+.inf"
                                    | "+.Inf"
                                    | "+.INF"
                                    | "-.inf"
                                    | "-.Inf"
                                    | "-.INF"
                                    | ".nan"
                                    | ".NaN"
                                    | ".NAN"
                            )
                    }
                    (Node::Bool(_), "bool") | (Node::Null, "null") => true,
                    _ => false,
                };
                if matches {
                    value
                } else {
                    self.warning(mark, tag.unwrap());
                    Node::String(text.into())
                }
            }
            _ if tag.is_some() => {
                self.warning(mark, tag.unwrap());
                Node::String(text.into())
            }
            _ if style == TScalarStyle::Plain => resolve_scalar(text, self.schema11),
            _ => Node::String(text.into()),
        };
        let id = self.doc.nodes.len();
        self.doc.nodes.push(node);
        self.marks.insert(id, mark);
        Ok(id)
    }
    fn collection_warning(&mut self, tag: Option<&Tag>, map: bool, mark: usize) {
        let Some(tag) = tag else { return };
        let name = Self::tag_name(tag);
        let standard = name.strip_prefix("tag:yaml.org,2002:");
        if map {
            if !matches!(standard, Some("map" | "set")) {
                self.warning(mark, tag);
            }
        } else if !matches!(standard, Some("seq" | "omap" | "pairs")) {
            if standard == Some("set") {
                let (start, end) = self.tag_span(mark);
                let warning = self.diag(
                    start,
                    end,
                    "YAMLWarning",
                    "BAD_COLLECTION_TYPE",
                    "tag:yaml.org,2002:set used for seq collection, but expects map",
                );
                self.doc.warnings.push(warning);
            } else {
                self.warning(mark, tag);
            }
        }
    }
    fn collection(&mut self, frame: &Frame) -> Result<Node, Diagnostic> {
        let standard = frame
            .tag
            .as_ref()
            .map(Self::tag_name)
            .and_then(|s| s.strip_prefix("tag:yaml.org,2002:").map(str::to_owned));
        if frame.map {
            let entries: Vec<_> = frame
                .children
                .chunks_exact(2)
                .map(|c| (c[0].0, c[1].0))
                .collect();
            // YAML's AST scalar uniqueness happens before JavaScript object
            // key coercion. A numeric 1 and a string "1" can still collide later.
            let mut previous: Vec<NodeId> = Vec::new();
            for pair in frame.children.chunks_exact(2) {
                let (key, alias) = pair[0];
                if !alias {
                    let duplicate = previous.iter().any(|old| {
                        match (&self.doc.nodes[*old], &self.doc.nodes[key]) {
                            (Node::Null, Node::Null) | (Node::Merge, Node::Merge) => true,
                            (Node::Bool(a), Node::Bool(b)) => a == b,
                            (Node::String(a), Node::String(b)) => a == b,
                            (Node::Wtf16String(a), Node::Wtf16String(b)) => a == b,
                            (Node::Number(a), Node::Number(b)) => a.as_f64() == b.as_f64(),
                            _ => false,
                        }
                    });
                    if duplicate {
                        let start = self.marks.get(&key).copied().unwrap_or(frame.mark);
                        let mut error = self.diag(
                            start,
                            start,
                            "YAMLParseError",
                            "DUPLICATE_KEY",
                            "Map keys must be unique",
                        );
                        error.pos.as_mut().unwrap()[1] += 1;
                        error.line_pos.as_mut().unwrap()[1].col += 1;
                        return Err(error);
                    }
                    previous.push(key);
                }
            }
            if standard.as_deref() == Some("set") {
                if entries
                    .iter()
                    .any(|(_, v)| !matches!(self.doc.nodes[*v], Node::Null))
                {
                    return Err(self.tag_error(frame.mark, "Set items must all have null values"));
                }
                return Ok(Node::Set(entries.into_iter().map(|(k, _)| k).collect()));
            }
            let mut fields: Vec<(ObjectKey, NodeId)> = Vec::new();
            for (entry_index, (key, value)) in entries.into_iter().enumerate() {
                if matches!(self.doc.nodes[key], Node::Merge) {
                    let sources = match &self.doc.nodes[value] {
                        Node::Array(values) => values.clone(),
                        _ => vec![value],
                    };
                    for source in sources {
                        if let Some(entries) = self.doc.fields_units(source) {
                            for (key, val) in entries {
                                let key = ObjectKey::from_units(key);
                                if !fields.iter().any(|(k, _)| k == &key) {
                                    fields.push((key, val));
                                }
                            }
                        } else {
                            return Err(Diagnostic::exception(
                                "Error",
                                "Merge sources must be maps or map aliases",
                            ));
                        }
                    }
                    continue;
                }
                let object_key = matches!(
                    self.doc.nodes[key],
                    Node::Array(_)
                        | Node::Object(_)
                        | Node::ObjectWithUtf16Keys(_)
                        | Node::Date(_)
                        | Node::Binary(_)
                        | Node::Map(_)
                        | Node::Set(_)
                );
                // Lf resolves the value first. Primitive alias values use String(value);
                // object values instead use the original Alias AST's literal spelling.
                let key_units = if let Some(alias) =
                    frame.key_aliases.get(&entry_index).filter(|_| object_key)
                {
                    alias.clone()
                } else {
                    self.doc.key_units(key)?
                };
                let key = ObjectKey::from_units(key_units);
                if let Some((_, existing)) = fields.iter_mut().find(|(k, _)| k == &key) {
                    *existing = value;
                } else {
                    fields.push((key, value));
                }
            }
            sort_unit_fields(&mut fields);
            if fields.iter().all(|(key, _)| key.text().is_some()) {
                Ok(Node::Object(
                    fields
                        .into_iter()
                        .map(|(key, id)| (key.text().unwrap().to_owned(), id))
                        .collect(),
                ))
            } else {
                Ok(Node::ObjectWithUtf16Keys(fields))
            }
        } else {
            let values: Vec<_> = frame.children.iter().map(|(id, _)| *id).collect();
            match standard.as_deref() {
                Some("omap") | Some("pairs") => {
                    let mut entries = Vec::new();
                    for id in &values {
                        let Some(raw) = self.edges.get(id) else {
                            return Err(self.tag_error(
                                frame.mark,
                                "Each pair must have its own sequence indicator",
                            ));
                        };
                        if raw.len() != 2 {
                            return Err(self.tag_error(
                                frame.mark,
                                "Each pair must have its own sequence indicator",
                            ));
                        }
                        entries.push((raw[0].0, raw[1].0));
                    }
                    if standard.as_deref() == Some("omap") {
                        let mut keys = BTreeSet::new();
                        for (key, _) in &entries {
                            let key = self.doc.key_units(*key)?;
                            if !keys.insert(key.clone()) {
                                let prefix = "Ordered maps must not include duplicate keys: ";
                                let mut error = self.tag_error(
                                    frame.mark,
                                    &format!("{prefix}{}", String::from_utf16_lossy(&key)),
                                );
                                if String::from_utf16(&key).is_err() {
                                    let mut units: Vec<u16> = prefix.encode_utf16().collect();
                                    units.extend(&key);
                                    let suffix = &error.message
                                        [prefix.len() + String::from_utf16_lossy(&key).len()..];
                                    units.extend(suffix.encode_utf16());
                                    error.message_units = Some(units);
                                }
                                return Err(error);
                            }
                        }
                        Ok(Node::Map(entries))
                    } else {
                        Ok(Node::Array(values))
                    }
                }
                Some("seq") | None => Ok(Node::Array(values)),
                _ => Ok(Node::Array(values)),
            }
        }
    }
}
fn sort_unit_fields(fields: &mut [(ObjectKey, NodeId)]) {
    let index = |key: &ObjectKey| {
        key.text().and_then(|s| {
            s.parse::<u32>()
                .ok()
                .filter(|n| *n < u32::MAX && n.to_string() == s)
        })
    };
    fields.sort_by(|(a, _), (b, _)| match (index(a), index(b)) {
        (Some(a), Some(b)) => a.cmp(&b),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        _ => std::cmp::Ordering::Equal,
    });
}
fn sort_fields(fields: &mut [(String, NodeId)]) {
    let index = |s: &str| {
        s.parse::<u32>()
            .ok()
            .filter(|n| *n < u32::MAX && n.to_string() == s)
    };
    fields.sort_by(|(a, _), (b, _)| match (index(a), index(b)) {
        (Some(a), Some(b)) => a.cmp(&b),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        _ => std::cmp::Ordering::Equal,
    });
}
fn scalar_pattern(index: usize, text: &str) -> bool {
    // Shipped core and YAML1.1 regex tables (rv / Bv), in resolver order.
    static PATTERNS: std::sync::OnceLock<Vec<regress::Regex>> = std::sync::OnceLock::new();
    let patterns = PATTERNS.get_or_init(|| {
        [
            r"^0o[0-7]+$",
            r"^[-+]?[0-9]+$",
            r"^0x[0-9a-fA-F]+$",
            r"^[-+]?(?:\.[0-9]+|[0-9]+(?:\.[0-9]*)?)[eE][-+]?[0-9]+$",
            r"^[-+]?(?:\.[0-9]+|[0-9]+\.[0-9]*)$",
            r"^[-+]?0b[0-1_]+$",
            r"^[-+]?0[0-7_]+$",
            r"^[-+]?[0-9][0-9_]*$",
            r"^[-+]?0x[0-9a-fA-F_]+$",
            r"^[-+]?(?:[0-9][0-9_]*)?(?:\.[0-9_]*)?[eE][-+]?[0-9]+$",
            r"^[-+]?(?:[0-9][0-9_]*)?\.[0-9_]*$",
            r"^[-+]?[0-9][0-9_]*(?::[0-5]?[0-9])+$",
            r"^[-+]?[0-9][0-9_]*(?::[0-5]?[0-9])+\.[0-9_]*$",
        ]
        .into_iter()
        .map(|pattern| regress::Regex::new(pattern).unwrap())
        .collect()
    });
    patterns[index].find(text).is_some()
}
fn resolve_scalar(text: &str, schema11: bool) -> Node {
    if matches!(text, "" | "~" | "null" | "Null" | "NULL") {
        return Node::Null;
    }
    if matches!(text, "true" | "True" | "TRUE")
        || schema11 && matches!(text, "yes" | "Yes" | "YES" | "on" | "On" | "ON" | "y" | "Y")
    {
        return Node::Bool(true);
    }
    if matches!(text, "false" | "False" | "FALSE")
        || schema11 && matches!(text, "no" | "No" | "NO" | "off" | "Off" | "OFF" | "n" | "N")
    {
        return Node::Bool(false);
    }
    if schema11 {
        if let Some(ms) = timestamp(text) {
            return Node::Date(ms);
        }
    }
    let matches = |index: usize| scalar_pattern(index, text);
    let number = match text {
        ".inf" | ".Inf" | ".INF" | "+.inf" | "+.Inf" | "+.INF" => Some(f64::INFINITY.into()),
        "-.inf" | "-.Inf" | "-.INF" => Some(f64::NEG_INFINITY.into()),
        ".nan" | ".NaN" | ".NAN" => Some(f64::NAN.into()),
        _ => None,
    };
    if let Some(number) = number {
        return Node::Number(number);
    }
    let radix = if schema11 {
        if matches(5) {
            Some((2, 2))
        } else if matches(6) {
            Some((8, 1))
        } else if matches(7) {
            Some((10, 0))
        } else if matches(8) {
            Some((16, 2))
        } else {
            None
        }
    } else if matches(0) {
        Some((8, 2))
    } else if matches(1) {
        Some((10, 0))
    } else if matches(2) {
        Some((16, 2))
    } else {
        None
    };
    if let Some((radix, prefix)) = radix {
        let negative = text.starts_with('-');
        let unsigned = text.strip_prefix(['+', '-']).unwrap_or(text);
        let digits = unsigned[prefix..].replace('_', "");
        let signed = format!("{}{digits}", if negative { "-" } else { "" });
        let number = if radix == 10 {
            signed
                .parse::<i64>()
                .ok()
                .map(serde_yaml::Number::from)
                .or_else(|| signed.parse::<u64>().ok().map(serde_yaml::Number::from))
                .unwrap_or_else(|| signed.parse::<f64>().unwrap_or(f64::NAN).into())
        } else if let Ok(value) = u64::from_str_radix(&digits, radix) {
            if negative {
                if value <= i64::MAX as u64 {
                    (-(value as i64)).into()
                } else {
                    (-(value as f64)).into()
                }
            } else {
                value.into()
            }
        } else {
            let value = power_of_two_integer(&digits, radix);
            (if negative { -value } else { value }).into()
        };
        return Node::Number(if negative && number.as_f64() == Some(0.0) {
            (-0.0f64).into()
        } else {
            number
        });
    }
    if schema11 && (matches(11) || matches(12)) {
        let negative = text.starts_with('-');
        let unsigned = text
            .strip_prefix(['+', '-'])
            .unwrap_or(text)
            .replace('_', "");
        let value = unsigned.split(':').fold(0.0, |number, part| {
            number * 60.0 + part.parse::<f64>().unwrap()
        });
        return Node::Number((if negative { -value } else { value }).into());
    }
    if if schema11 {
        matches(9) || matches(10)
    } else {
        matches(3) || matches(4)
    } {
        let value = text.replace('_', "").parse::<f64>().unwrap_or(f64::NAN);
        return Node::Number(value.into());
    }
    Node::String(text.into())
}

fn ordinary_number(value: &Value) -> f64 {
    match value {
        Value::Null => return 0.0,
        Value::Bool(value) => return if *value { 1.0 } else { 0.0 },
        Value::Number(value) => return value.as_f64().unwrap(),
        _ => {}
    }
    let text = crate::yaml::javascript_string(value);
    let text = text.trim_matches(crate::vault_search::js_whitespace);
    if text.is_empty() {
        return 0.0;
    }
    if matches!(text, "Infinity" | "+Infinity") {
        return f64::INFINITY;
    }
    if text == "-Infinity" {
        return f64::NEG_INFINITY;
    }
    for (prefix, radix) in [
        ("0x", 16),
        ("0X", 16),
        ("0o", 8),
        ("0O", 8),
        ("0b", 2),
        ("0B", 2),
    ] {
        if let Some(digits) = text.strip_prefix(prefix) {
            if digits.is_empty() || !digits.chars().all(|c| c.is_digit(radix)) {
                return f64::NAN;
            }
            return power_of_two_integer(digits, radix);
        }
    }
    if !text
        .chars()
        .all(|c| c.is_ascii_digit() || matches!(c, '+' | '-' | '.' | 'e' | 'E'))
    {
        return f64::NAN;
    }
    text.parse::<f64>().unwrap_or(f64::NAN)
}

fn power_of_two_integer(digits: &str, radix: u32) -> f64 {
    let width = radix.ilog2();
    let mut significant = 0u64;
    let mut count = 0usize;
    let mut round = false;
    let mut sticky = false;
    for digit in digits.chars().map(|c| c.to_digit(radix).unwrap()) {
        for shift in (0..width).rev() {
            let bit = digit >> shift & 1;
            if count == 0 && bit == 0 {
                continue;
            }
            count += 1;
            if count <= 53 {
                significant = significant << 1 | bit as u64;
            } else if count == 54 {
                round = bit != 0;
            } else {
                sticky |= bit != 0;
            }
        }
    }
    if count <= 53 {
        return significant as f64;
    }
    if round && (sticky || significant & 1 != 0) {
        significant += 1;
    }
    significant as f64 * 2f64.powi((count - 53).min(1024) as i32)
}

fn timestamp(text: &str) -> Option<i64> {
    let regex=regress::Regex::new(r"^(\d{4})-(\d{1,2})-(\d{1,2})(?:(?:t|T|[ \t]+)(\d{1,2}):(\d{1,2}):(\d{1,2})(\.\d+)?(?:[ \t]*(Z|[-+][012]?[0-9](?::[0-9]{2})?))?)?$").unwrap();
    let m = regex.find(text)?;
    let value = |n| m.group(n).map(|r| &text[r]);
    let number = |n| value(n).unwrap_or("0").parse::<i64>().ok();
    let mut year = number(1)?;
    if year < 100 {
        year += 1900;
    }
    let month = number(2)? - 1;
    year += month.div_euclid(12);
    let month = time::Month::try_from((month.rem_euclid(12) + 1) as u8).ok()?;
    let date = time::Date::from_calendar_date(year as i32, month, 1).ok()?;
    let mut ms = date.midnight().assume_utc().unix_timestamp() * 1000
        + (number(3)? - 1) * 86_400_000
        + number(4)? * 3_600_000
        + number(5)? * 60_000
        + number(6)? * 1000;
    if let Some(fraction) = value(7) {
        let digits = format!("{}00", &fraction[1..]);
        ms += digits[..3].parse::<i64>().ok()?;
    }
    if let Some(zone) = value(8).filter(|s| *s != "Z") {
        let parts: Vec<_> = zone[1..].split(':').collect();
        let mut minutes = parts.iter().try_fold(0i64, |number, part| {
            part.parse::<i64>().ok().map(|part| number * 60 + part)
        })?;
        if minutes < 30 {
            minutes *= 60;
        }
        ms -= if zone.starts_with('-') {
            -minutes
        } else {
            minutes
        } * 60_000;
    }
    Some(ms)
}

impl Document {
    pub fn key_units(&self, id: NodeId) -> Result<Vec<u16>, Diagnostic> {
        match &self.nodes[id] {
            Node::Null => Ok(Vec::new()),
            Node::Wtf16String(units) => Ok(units.clone()),
            Node::Array(_)
            | Node::Object(_)
            | Node::ObjectWithUtf16Keys(_)
            | Node::Map(_)
            | Node::Set(_) => self.flow_units(id),
            _ => Ok(self.key_string(id)?.encode_utf16().collect()),
        }
    }
    pub fn fields_units(&self, id: NodeId) -> Option<Vec<(Vec<u16>, NodeId)>> {
        match &self.nodes[id] {
            Node::Object(fields) => Some(
                fields
                    .iter()
                    .map(|(key, id)| (key.encode_utf16().collect(), *id))
                    .collect(),
            ),
            Node::ObjectWithUtf16Keys(fields) => {
                Some(fields.iter().map(|(key, id)| (key.units(), *id)).collect())
            }
            Node::Array(values) => {
                let mut fields: Vec<_> = values
                    .iter()
                    .enumerate()
                    .map(|(index, id)| (index.to_string().encode_utf16().collect(), *id))
                    .collect();
                if let Some(named) = self.own_utf16_properties.get(&id) {
                    fields.extend(named.iter().map(|(key, id)| (key.units(), *id)));
                } else if let Some(named) = self.own_properties.get(&id) {
                    fields.extend(
                        named
                            .iter()
                            .map(|(key, id)| (key.encode_utf16().collect(), *id)),
                    )
                }
                Some(fields)
            }
            _ if self.own_utf16_properties.contains_key(&id) => Some(
                self.own_utf16_properties[&id]
                    .iter()
                    .map(|(key, id)| (key.units(), *id))
                    .collect(),
            ),
            _ => self.own_properties.get(&id).map(|fields| {
                fields
                    .iter()
                    .map(|(key, id)| (key.encode_utf16().collect(), *id))
                    .collect()
            }),
        }
    }
    pub fn own_fields_units(&self) -> Vec<(Vec<u16>, NodeId)> {
        self.fields_units(self.root).unwrap_or_default()
    }
    pub fn own_keys_units(&self) -> Vec<Vec<u16>> {
        self.own_fields_units()
            .into_iter()
            .map(|(key, _)| key)
            .collect()
    }
    pub fn own_property_units(&self, name: &[u16]) -> Option<NodeId> {
        self.own_fields_units()
            .into_iter()
            .find(|(key, _)| key == name)
            .map(|(_, id)| id)
    }
    pub fn key_string(&self, id: NodeId) -> Result<String, Diagnostic> {
        match &self.nodes[id] {
            Node::Null => Ok(String::new()),
            Node::Array(_)
            | Node::Object(_)
            | Node::ObjectWithUtf16Keys(_)
            | Node::Map(_)
            | Node::Set(_) => self.flow_string(id),
            _ => self.javascript_string(id),
        }
    }
    fn flow_child_units(
        &self,
        parent: NodeId,
        index: usize,
        id: NodeId,
    ) -> Result<Vec<u16>, Diagnostic> {
        match self
            .flow_aliases
            .get(&parent)
            .and_then(|aliases| aliases.get(&index))
        {
            Some(alias) => Ok(alias.clone()),
            None => self.flow_units(id),
        }
    }
    fn flow_string(&self, id: NodeId) -> Result<String, Diagnostic> {
        String::from_utf16(&self.flow_units(id)?).map_err(|_| {
            Diagnostic::exception("TypeError", "Unpaired UTF16 key requires key_units")
        })
    }
    fn flow_units(&self, id: NodeId) -> Result<Vec<u16>, Diagnostic> {
        let text = |s: String| s.encode_utf16().collect::<Vec<_>>();
        let join = |open: &str, close: &str, items: Vec<Vec<u16>>| {
            if items.is_empty() {
                return format!("{}{}", open.trim_end(), close.trim_start())
                    .encode_utf16()
                    .collect::<Vec<_>>();
            }
            let mut units = open.encode_utf16().collect::<Vec<_>>();
            for (i, item) in items.into_iter().enumerate() {
                if i > 0 {
                    units.extend(", ".encode_utf16());
                }
                units.extend(item);
            }
            units.extend(close.encode_utf16());
            units
        };
        // Unsafe Unicode in a scalar always selects the double-quoted native
        // stringifier, independently of its original single/plain quote style.
        if let Node::Wtf16String(units) = &self.nodes[id] {
            return Ok(text(crate::yaml::quote_yaml_units(units)));
        }
        if self.flow_single_quoted.contains(&id) && matches!(self.nodes[id], Node::String(_)) {
            return Ok(text(format!(
                "'{}'",
                self.javascript_string(id)?.replace('\'', "''")
            )));
        }
        if self.flow_quoted.contains(&id) && matches!(self.nodes[id], Node::String(_)) {
            return Ok(text(crate::yaml::quote_yaml_units(
                &self.javascript_units(id)?,
            )));
        }
        Ok(match &self.nodes[id] {
            Node::Array(ids) => join(
                "[ ",
                " ]",
                ids.iter()
                    .enumerate()
                    .map(|(index, child)| self.flow_child_units(id, index, *child))
                    .collect::<Result<_, Diagnostic>>()?,
            ),
            Node::Object(_) | Node::ObjectWithUtf16Keys(_) => {
                let pairs = if let Some(children) = self.flow_children.get(&id) {
                    children
                        .chunks_exact(2)
                        .enumerate()
                        .map(|(index, pair)| {
                            let mut units = self.flow_child_units(id, index * 2, pair[0])?;
                            units.extend(": ".encode_utf16());
                            units.extend(self.flow_child_units(id, index * 2 + 1, pair[1])?);
                            Ok(units)
                        })
                        .collect::<Result<Vec<_>, Diagnostic>>()?
                } else {
                    self.fields_units(id)
                        .unwrap_or_default()
                        .iter()
                        .map(|(key, value)| {
                            let mut units = text(crate::yaml_value::quote_units(key));
                            units.extend(": ".encode_utf16());
                            units.extend(self.flow_units(*value)?);
                            Ok(units)
                        })
                        .collect::<Result<Vec<_>, Diagnostic>>()?
                };
                join("{ ", " }", pairs)
            }
            Node::Set(values) => join(
                "{ ",
                " }",
                values
                    .iter()
                    .enumerate()
                    .map(|(index, child)| self.flow_child_units(id, index * 2, *child))
                    .collect::<Result<_, Diagnostic>>()?,
            ),
            Node::Map(entries) => {
                let items = entries
                    .iter()
                    .enumerate()
                    .map(|(index, (key, value))| {
                        let pair = self
                            .flow_children
                            .get(&id)
                            .and_then(|children| children.get(index));
                        let mut units = if let Some(pair) = pair {
                            self.flow_child_units(*pair, 0, *key)?
                        } else {
                            self.flow_units(*key)?
                        };
                        units.extend(": ".encode_utf16());
                        units.extend(if let Some(pair) = pair {
                            self.flow_child_units(*pair, 1, *value)?
                        } else {
                            self.flow_units(*value)?
                        });
                        Ok(units)
                    })
                    .collect::<Result<_, Diagnostic>>()?;
                join("[ ", " ]", items)
            }
            Node::Date(_) | Node::Binary(_) => self.javascript_units(id)?,
            Node::Null => text("null".into()),
            _ => text(
                crate::yaml::stringify(&self.project(id, false, &mut BTreeSet::new())?)
                    .trim_end_matches('\n')
                    .to_owned(),
            ),
        })
    }
    /// Native YAML output units; no UTF8 loss conversion is used.
    pub fn stringify_units(&self) -> Result<Vec<u16>, Diagnostic> {
        Ok(self.stringify()?.encode_utf16().collect())
    }
    /// Lossless JavaScript String(value), including lone UTF16 surrogates.
    pub fn javascript_units(&self, id: NodeId) -> Result<Vec<u16>, Diagnostic> {
        self.string_units(id, &mut BTreeSet::new())
    }
    /// Convenience for callers whose contract requires a valid Unicode string.
    pub fn javascript_string(&self, id: NodeId) -> Result<String, Diagnostic> {
        String::from_utf16(&self.javascript_units(id)?).map_err(|_| {
            Diagnostic::exception(
                "TypeError",
                "Unpaired UTF16 surrogate requires javascript_units",
            )
        })
    }
    /// Explicit Node/CLI UTF8 output boundary; internal values remain lossless.
    pub fn javascript_string_lossy(&self, id: NodeId) -> Result<String, Diagnostic> {
        Ok(String::from_utf16_lossy(&self.javascript_units(id)?))
    }
    fn string_units(
        &self,
        id: NodeId,
        seen: &mut BTreeSet<NodeId>,
    ) -> Result<Vec<u16>, Diagnostic> {
        if let Node::Wtf16String(units) = &self.nodes[id] {
            return Ok(units.clone());
        }
        if let Node::Array(ids) = &self.nodes[id] {
            if !seen.insert(id) {
                return Ok(Vec::new());
            }
            let mut result = Vec::new();
            for (index, id) in ids.iter().enumerate() {
                if index > 0 {
                    result.push(',' as u16);
                }
                if matches!(self.nodes[*id], Node::Merge) {
                    return Err(Diagnostic::exception(
                        "TypeError",
                        "Cannot convert a Symbol value to a string",
                    ));
                }
                if !matches!(self.nodes[*id], Node::Null) {
                    result.extend(self.string_units(*id, seen)?);
                }
            }
            seen.remove(&id);
            return Ok(result);
        }
        let value = match &self.nodes[id] {
            Node::Null => "null".into(),
            Node::Bool(value) => value.to_string(),
            Node::Number(value) => crate::vault_search::ecmascript_number(value.as_f64().unwrap()),
            Node::String(value) => value.clone(),
            Node::Object(_) | Node::ObjectWithUtf16Keys(_) => "[object Object]".into(),
            Node::Map(_) => "[object Map]".into(),
            Node::Set(_) => "[object Set]".into(),
            Node::Date(value) => date_string(*value),
            Node::Binary(bytes) => bytes
                .iter()
                .map(u8::to_string)
                .collect::<Vec<_>>()
                .join(","),
            Node::Merge => "Symbol(<<)".into(),
            Node::Array(_) | Node::Wtf16String(_) => unreachable!(),
        };
        Ok(value.encode_utf16().collect())
    }
    pub fn stringify(&self) -> Result<String, Diagnostic> {
        let value = self.project(self.root, false, &mut BTreeSet::new())?;
        Ok(crate::yaml::stringify(&value))
    }
    pub fn json(&self, pretty: bool) -> Result<String, Diagnostic> {
        self.json_output(pretty)?
            .ok_or_else(|| Diagnostic::exception("TypeError", "JSON value is undefined"))
    }
    /// None represents JSON.stringify's undefined result for a root Symbol.
    pub fn json_output(&self, pretty: bool) -> Result<Option<String>, Diagnostic> {
        if matches!(self.nodes[self.root], Node::Merge) {
            return Ok(None);
        }
        Ok(Some(self.write_json(
            self.root,
            pretty,
            0,
            &mut BTreeSet::new(),
        )?))
    }
    fn write_json(
        &self,
        id: NodeId,
        pretty: bool,
        indent: usize,
        seen: &mut BTreeSet<NodeId>,
    ) -> Result<String, Diagnostic> {
        use crate::yaml_value::quote_units;
        if !seen.insert(id) {
            return Err(self.circular_error(id));
        }
        let fields = self.fields_units(id);
        let result = match &self.nodes[id] {
            Node::Null | Node::Merge => "null".into(),
            Node::Bool(value) => value.to_string(),
            Node::Number(value) => {
                let value = value.as_f64().unwrap();
                if value.is_finite() {
                    crate::vault_search::ecmascript_number(value)
                } else {
                    "null".into()
                }
            }
            Node::String(value) => quote_units(&value.encode_utf16().collect::<Vec<_>>()),
            Node::Wtf16String(units) => quote_units(units),
            Node::Date(value)
                if !fields.is_some_and(|fields| {
                    fields
                        .iter()
                        .any(|(key, _)| key.iter().copied().eq("toJSON".encode_utf16()))
                }) =>
            {
                quote_units(&iso_date(*value).encode_utf16().collect::<Vec<_>>())
            }
            Node::Array(values) => {
                let values = values
                    .iter()
                    .map(|id| self.write_json(*id, pretty, indent + 2, seen))
                    .collect::<Result<Vec<_>, _>>()?;
                json_collection("[", "]", values, pretty, indent)
            }
            _ => {
                let fields = self.fields_units(id).unwrap_or_default();
                let mut rows = Vec::new();
                for (key, id) in fields {
                    if matches!(self.nodes[id], Node::Merge) {
                        continue;
                    }
                    let key = quote_units(&key);
                    rows.push(format!(
                        "{key}:{}{}",
                        if pretty { " " } else { "" },
                        self.write_json(id, pretty, indent + 2, seen)?
                    ));
                }
                json_collection("{", "}", rows, pretty, indent)
            }
        };
        seen.remove(&id);
        Ok(result)
    }
    pub fn json_mapping(&self) -> Result<Option<Mapping>, Diagnostic> {
        match self.project(self.root, true, &mut BTreeSet::new())? {
            Value::Mapping(map) => Ok(Some(map)),
            _ => Ok(None),
        }
    }
    fn project(
        &self,
        id: NodeId,
        json: bool,
        seen: &mut BTreeSet<NodeId>,
    ) -> Result<Value, Diagnostic> {
        if !seen.insert(id) {
            return Err(if json {
                self.circular_error(id)
            } else {
                Diagnostic::exception("RangeError", "Maximum call stack size exceeded")
            });
        }
        let value = match &self.nodes[id] {
            Node::Null => Value::Null,
            Node::Bool(v) => Value::Bool(*v),
            Node::Number(v) => Value::Number(v.clone()),
            Node::String(v) => Value::String(v.clone()),
            Node::Wtf16String(units) if !json => wtf16_value(units),
            Node::Wtf16String(_) => {
                return Err(Diagnostic::exception(
                    "TypeError",
                    "A UTF16 string requires json_output",
                ))
            }
            Node::Date(_)
                if self.fields_units(id).is_some_and(|fields| {
                    fields
                        .iter()
                        .any(|(key, _)| key.iter().copied().eq("toJSON".encode_utf16()))
                }) =>
            {
                Value::Mapping(
                    self.fields_units(id)
                        .unwrap()
                        .iter()
                        .filter(|(_, id)| !json || !matches!(self.nodes[*id], Node::Merge))
                        .map(|(key, id)| {
                            Ok((
                                key_value(&ObjectKey::from_units(key.clone())),
                                self.project(*id, json, seen)?,
                            ))
                        })
                        .collect::<Result<_, Diagnostic>>()?,
                )
            }
            Node::Date(ms) => Value::String(iso_date(*ms)),
            Node::Array(ids) => Value::Sequence(
                ids.iter()
                    .map(|id| self.project(*id, json, seen))
                    .collect::<Result<_, _>>()?,
            ),
            Node::ObjectWithUtf16Keys(fields) => Value::Mapping(
                fields
                    .iter()
                    .filter(|(_, id)| !json || !matches!(self.nodes[*id], Node::Merge))
                    .map(|(key, id)| Ok((key_value(key), self.project(*id, json, seen)?)))
                    .collect::<Result<_, Diagnostic>>()?,
            ),
            Node::Object(fields) => Value::Mapping(
                fields
                    .iter()
                    .filter(|(_, id)| !json || !matches!(self.nodes[*id], Node::Merge))
                    .map(|(key, id)| {
                        Ok((Value::String(key.clone()), self.project(*id, json, seen)?))
                    })
                    .collect::<Result<_, Diagnostic>>()?,
            ),
            Node::Binary(bytes) if json => Value::Mapping(
                bytes
                    .iter()
                    .enumerate()
                    .map(|(i, byte)| (Value::String(i.to_string()), Value::Number((*byte).into())))
                    .collect(),
            ),
            Node::Binary(bytes) => Value::Sequence(
                bytes
                    .iter()
                    .map(|byte| Value::Number((*byte).into()))
                    .collect(),
            ),
            Node::Set(_) | Node::Map(_) if json => Value::Mapping(Mapping::new()),
            Node::Set(ids) => Value::Sequence(
                ids.iter()
                    .map(|id| self.project(*id, json, seen))
                    .collect::<Result<_, _>>()?,
            ),
            Node::Map(fields) => Value::Mapping(
                fields
                    .iter()
                    .map(|(key, val)| {
                        Ok((
                            self.project(*key, json, seen)?,
                            self.project(*val, json, seen)?,
                        ))
                    })
                    .collect::<Result<_, Diagnostic>>()?,
            ),
            Node::Merge if json => Value::Null,
            Node::Merge => {
                return Err(Diagnostic::exception(
                    "Error",
                    "Tag not resolved for Symbol value",
                ))
            }
        };
        seen.remove(&id);
        Ok(value)
    }
    fn circular_error(&self, id: NodeId) -> Diagnostic {
        fn edges(doc: &Document, id: NodeId) -> Vec<(String, NodeId)> {
            match &doc.nodes[id] {
                Node::ObjectWithUtf16Keys(fields) => fields
                    .iter()
                    .map(|(key, id)| {
                        (
                            format!("property '{}'", String::from_utf16_lossy(&key.units())),
                            *id,
                        )
                    })
                    .collect(),
                Node::Object(fields) => fields
                    .iter()
                    .map(|(key, id)| (format!("property '{key}'"), *id))
                    .collect(),
                Node::Array(values) => values
                    .iter()
                    .enumerate()
                    .map(|(index, id)| (format!("index {index}"), *id))
                    .collect(),
                _ => Vec::new(),
            }
        }
        fn path(
            doc: &Document,
            id: NodeId,
            target: NodeId,
            seen: &mut BTreeSet<NodeId>,
        ) -> Option<Vec<(String, NodeId)>> {
            if !seen.insert(id) {
                return None;
            }
            for (label, next) in edges(doc, id) {
                if next == target {
                    return Some(vec![(label, next)]);
                }
                if let Some(mut trail) = path(doc, next, target, seen) {
                    trail.insert(0, (label, next));
                    return Some(trail);
                }
            }
            None
        }
        let constructor = |id| {
            if matches!(self.nodes[id], Node::Array(_)) {
                "Array"
            } else {
                "Object"
            }
        };
        let mut message=format!("Converting circular structure to JSON\n    --> starting at object with constructor '{}'",constructor(id));
        if let Some(trail) = path(self, id, id, &mut BTreeSet::new()) {
            for (index, (label, next)) in trail.iter().enumerate() {
                if index + 1 == trail.len() {
                    message += &format!("\n    --- {label} closes the circle");
                } else {
                    message += &format!(
                        "\n    |     {label} -> object with constructor '{}'",
                        constructor(*next)
                    );
                }
            }
        }
        Diagnostic::exception("TypeError", &message)
    }
    pub fn try_descriptor(&self) -> Result<Json, Diagnostic> {
        fn check(
            document: &Document,
            id: NodeId,
            seen: &mut BTreeSet<NodeId>,
        ) -> Result<(), Diagnostic> {
            if !seen.insert(id) {
                return Ok(());
            }
            match &document.nodes[id] {
                Node::Array(values) => {
                    document.javascript_units(id)?;
                    for id in values {
                        check(document, *id, seen)?;
                    }
                }
                Node::ObjectWithUtf16Keys(fields) => {
                    for (_, id) in fields {
                        check(document, *id, seen)?;
                    }
                }
                Node::Object(fields) => {
                    for (_, id) in fields {
                        check(document, *id, seen)?;
                    }
                }
                Node::Set(values) => {
                    for id in values {
                        check(document, *id, seen)?;
                    }
                }
                Node::Map(fields) => {
                    for (key, value) in fields {
                        check(document, *key, seen)?;
                        check(document, *value, seen)?;
                    }
                }
                _ => {}
            }
            Ok(())
        }
        check(self, self.root, &mut BTreeSet::new())?;
        Ok(self.describe(self.root, &mut BTreeMap::new()))
    }
    pub fn descriptor(&self) -> Json {
        self.describe(self.root, &mut BTreeMap::new())
    }
    fn describe(&self, id: NodeId, seen: &mut BTreeMap<NodeId, usize>) -> Json {
        match &self.nodes[id] {
            Node::Null => return json!({"type":"null"}),
            Node::Bool(v) => return json!({"type":"boolean","value":v}),
            Node::Number(v) => {
                return json!({"type":"number","string":crate::vault_search::ecmascript_number(v.as_f64().unwrap()),"value":serde_json::from_str::<Json>(&crate::vault_search::ecmascript_number(v.as_f64().unwrap())).unwrap_or(Json::Null)})
            }
            Node::String(v) => return json!({"type":"string","value":v}),
            Node::Wtf16String(units) => return json!({"type":"string","value_utf16":units}),
            Node::Merge => return json!({"type":"symbol","string":"Symbol(<<)"}),
            _ => {}
        }
        if let Some(previous) = seen.get(&id) {
            return json!({"ref":previous});
        }
        let visual = seen.len();
        seen.insert(id, visual);
        match &self.nodes[id] {
            Node::ObjectWithUtf16Keys(fields) => {
                json!({"type":"Object","id":visual,"properties_utf16":fields.iter().map(|(k,v)|json!([k.units(),self.describe(*v,seen)])).collect::<Vec<_>>(),"symbols":[]})
            }
            Node::Object(fields) => {
                json!({"type":"Object","id":visual,"properties":fields.iter().map(|(k,v)|json!([k,self.describe(*v,seen)])).collect::<Vec<_>>(),"symbols":[]})
            }
            Node::Array(ids) => {
                let units = self.javascript_units(id).unwrap();
                let mut value = json!({"type":"Array","id":visual,"values":ids.iter().map(|id|self.describe(*id,seen)).collect::<Vec<_>>()});
                if let Ok(text) = String::from_utf16(&units) {
                    value["string"] = json!(text);
                } else {
                    value["string_utf16"] = json!(units);
                }
                value
            }
            Node::Date(ms) => {
                json!({"type":"Date","id":visual,"iso":iso_date(*ms),"string":date_string(*ms),"millis":ms})
            }
            Node::Binary(bytes) => {
                json!({"type":"Uint8Array","id":visual,"values":bytes,"string":self.javascript_string(id).unwrap()})
            }
            Node::Set(ids) => {
                json!({"type":"Set","id":visual,"values":ids.iter().map(|id|self.describe(*id,seen)).collect::<Vec<_>>(),"string":"[object Set]"})
            }
            Node::Map(fields) => {
                json!({"type":"Map","id":visual,"entries":fields.iter().map(|(k,v)|json!([self.describe(*k,seen),self.describe(*v,seen)])).collect::<Vec<_>>(),"string":"[object Map]"})
            }
            _ => unreachable!(),
        }
    }
}
fn iso_date(ms: i64) -> String {
    let date = time::OffsetDateTime::from_unix_timestamp_nanos(ms as i128 * 1_000_000).unwrap();
    format!(
        "{}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
        if (0..=9999).contains(&date.year()) {
            format!("{:04}", date.year())
        } else {
            format!("{:+07}", date.year())
        },
        date.month() as u8,
        date.day(),
        date.hour(),
        date.minute(),
        date.second(),
        date.millisecond()
    )
}
fn date_string(ms: i64) -> String {
    let date = time::OffsetDateTime::from_unix_timestamp_nanos(ms as i128 * 1_000_000).unwrap();
    let day = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"]
        [date.weekday().number_days_from_monday() as usize];
    let month = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ][date.month() as usize - 1];
    format!(
        "{day} {month} {:02} {:04} {:02}:{:02}:{:02} GMT+0000 (Coordinated Universal Time)",
        date.day(),
        date.year(),
        date.hour(),
        date.minute(),
        date.second()
    )
}
fn json_collection(
    start: &str,
    end: &str,
    rows: Vec<String>,
    pretty: bool,
    indent: usize,
) -> String {
    if rows.is_empty() {
        return format!("{start}{end}");
    }
    if pretty {
        format!(
            "{start}\n{}\n{}{end}",
            rows.iter()
                .map(|row| format!("{}{row}", " ".repeat(indent + 2)))
                .collect::<Vec<_>>()
                .join(",\n"),
            " ".repeat(indent)
        )
    } else {
        format!("{start}{}{end}", rows.join(","))
    }
}
fn key_value(key: &ObjectKey) -> Value {
    match key {
        ObjectKey::String(value) => Value::String(value.clone()),
        ObjectKey::Wtf16(units) => wtf16_value(units),
    }
}
fn wtf16_value(units: &[u16]) -> Value {
    Value::Tagged(Box::new(serde_yaml::value::TaggedValue {
        tag: serde_yaml::value::Tag::new("__native_wtf16"),
        value: Value::Sequence(
            units
                .iter()
                .map(|unit| Value::Number((*unit).into()))
                .collect(),
        ),
    }))
}

/// Worker metadata parses a normalized frontmatter token without separator LF.
pub fn parse_frontmatter(source: &str) -> Result<Document, Diagnostic> {
    let normalized = source.replace("\r\n", "\n");
    let mut source = normalized
        .strip_suffix('\n')
        .unwrap_or(&normalized)
        .to_owned();
    if !source.ends_with('\n') {
        source.push('\n');
    }
    parse(&source)
}
impl Document {
    pub fn own_property(&self, name: &str) -> Option<NodeId> {
        self.own_property_units(&name.encode_utf16().collect::<Vec<_>>())
    }
    pub fn root_fields(&self) -> Option<&[(String, NodeId)]> {
        if self.own_utf16_properties.contains_key(&self.root) {
            return None;
        }
        if let Node::Object(fields) = &self.nodes[self.root] {
            Some(fields)
        } else {
            self.own_properties.get(&self.root).map(Vec::as_slice)
        }
    }
    pub fn inferred_widget(&self, id: NodeId) -> &'static str {
        match &self.nodes[id] {
            Node::String(value) => {
                let b = value.as_bytes();
                let date = b.len() >= 10
                    && b[..4].iter().all(u8::is_ascii_digit)
                    && b[4] == b'-'
                    && matches!(b[5], b'0' | b'1')
                    && b[6].is_ascii_digit()
                    && b[7] == b'-'
                    && matches!(b[8], b'0'..=b'3')
                    && b[9].is_ascii_digit();
                if date && b.len() == 10 {
                    "date"
                } else if date
                    && b.len() == 16
                    && b[10] == b'T'
                    && matches!(b[11], b'0'..=b'2')
                    && b[12].is_ascii_digit()
                    && b[13] == b':'
                    && matches!(b[14], b'0'..=b'5')
                    && b[15].is_ascii_digit()
                {
                    "datetime"
                } else {
                    "text"
                }
            }
            Node::Wtf16String(_) => "text",
            Node::Null => "text",
            Node::Bool(_) => "checkbox",
            Node::Number(_) => "number",
            Node::Array(ids)
                if ids.iter().all(|id| {
                    matches!(
                        self.nodes[*id],
                        Node::String(_) | Node::Wtf16String(_) | Node::Null
                    )
                }) =>
            {
                "multitext"
            }
            _ => "unknown",
        }
    }
    /// Legacy metadata tree: Map/Set entries are opaque because frontmatter
    /// link traversal enumerates own properties instead of their contents.
    /// The native sidecar retains their entries, shared identity and cycles.
    pub fn metadata_mapping(&self) -> Result<Option<Mapping>, Diagnostic> {
        fn value(
            document: &Document,
            id: NodeId,
            active: &mut BTreeSet<NodeId>,
        ) -> Result<Value, Diagnostic> {
            let node = &document.nodes[id];
            if matches!(node, Node::Set(_) | Node::Map(_)) {
                let tag = if matches!(node, Node::Set(_)) {
                    "__native_set"
                } else {
                    "__native_map"
                };
                return Ok(Value::Tagged(Box::new(serde_yaml::value::TaggedValue {
                    tag: serde_yaml::value::Tag::new(tag),
                    value: Value::Null,
                })));
            }
            if !active.insert(id) {
                return Err(document.circular_error(id));
            }
            let result = match node {
                Node::Object(fields) => Value::Mapping(
                    fields
                        .iter()
                        .map(|(key, id)| {
                            Ok((Value::String(key.clone()), value(document, *id, active)?))
                        })
                        .collect::<Result<_, Diagnostic>>()?,
                ),
                Node::ObjectWithUtf16Keys(fields) => Value::Mapping(
                    fields
                        .iter()
                        .map(|(key, id)| Ok((key_value(key), value(document, *id, active)?)))
                        .collect::<Result<_, Diagnostic>>()?,
                ),
                Node::Array(values) => Value::Sequence(
                    values
                        .iter()
                        .map(|id| value(document, *id, active))
                        .collect::<Result<_, Diagnostic>>()?,
                ),
                _ => document.internal_value(id, &mut BTreeSet::new())?,
            };
            active.remove(&id);
            Ok(result)
        }
        match value(self, self.root, &mut BTreeSet::new())? {
            Value::Mapping(map) => Ok(Some(map)),
            _ => Ok(None),
        }
    }
    pub fn internal_mapping(&self) -> Result<Option<Mapping>, Diagnostic> {
        match self.internal_value(self.root, &mut BTreeSet::new())? {
            Value::Mapping(map) => Ok(Some(map)),
            _ => Ok(None),
        }
    }
    fn internal_value(&self, id: NodeId, seen: &mut BTreeSet<NodeId>) -> Result<Value, Diagnostic> {
        if !seen.insert(id) {
            return Err(self.circular_error(id));
        }
        let tagged = |name: &str, value| {
            Value::Tagged(Box::new(serde_yaml::value::TaggedValue {
                tag: serde_yaml::value::Tag::new(name),
                value,
            }))
        };
        let value = match &self.nodes[id] {
            Node::Null => Value::Null,
            Node::Bool(v) => Value::Bool(*v),
            Node::Number(v) => Value::Number(v.clone()),
            Node::String(v) => Value::String(v.clone()),
            Node::Wtf16String(units) => wtf16_value(units),
            Node::ObjectWithUtf16Keys(fields) => Value::Mapping(
                fields
                    .iter()
                    .map(|(key, id)| Ok((key_value(key), self.internal_value(*id, seen)?)))
                    .collect::<Result<_, Diagnostic>>()?,
            ),
            Node::Object(fields) => Value::Mapping(
                fields
                    .iter()
                    .map(|(key, id)| {
                        Ok((Value::String(key.clone()), self.internal_value(*id, seen)?))
                    })
                    .collect::<Result<_, Diagnostic>>()?,
            ),
            Node::Array(ids) => Value::Sequence(
                ids.iter()
                    .map(|id| self.internal_value(*id, seen))
                    .collect::<Result<_, _>>()?,
            ),
            Node::Date(ms) => tagged("__native_date", Value::Number((*ms).into())),
            Node::Binary(bytes) => tagged(
                "__native_binary",
                Value::Sequence(bytes.iter().map(|b| Value::Number((*b).into())).collect()),
            ),
            Node::Set(ids) => tagged(
                "__native_set",
                Value::Sequence(
                    ids.iter()
                        .map(|id| self.internal_value(*id, seen))
                        .collect::<Result<_, _>>()?,
                ),
            ),
            Node::Map(entries) => tagged(
                "__native_map",
                Value::Sequence(
                    entries
                        .iter()
                        .map(|(k, v)| {
                            Ok(Value::Sequence(vec![
                                self.internal_value(*k, seen)?,
                                self.internal_value(*v, seen)?,
                            ]))
                        })
                        .collect::<Result<_, Diagnostic>>()?,
                ),
            ),
            Node::Merge => Value::String("<<".into()),
        };
        seen.remove(&id);
        Ok(value)
    }
    /// Preserve typed children during the Array callback's single assignment.
    /// Sparse output beyond the existing dense boundary remains separate work.
    pub fn modify_root_array(
        &mut self,
        name: &str,
        value: Option<Value>,
    ) -> Result<Option<usize>, Diagnostic> {
        let Node::Array(mut values) = self.nodes[self.root].clone() else {
            return Ok(None);
        };
        if name == "__proto__" {
            return Ok(None);
        }
        let mut count = values.len();
        if name == "length" {
            let Some(value) = value else {
                return Err(Diagnostic::exception(
                    "TypeError",
                    "Cannot delete property 'length' of [object Array]",
                ));
            };
            let length = ordinary_number(&value);
            if !length.is_finite()
                || length < 0.0
                || length > u32::MAX as f64
                || length.fract() != 0.0
            {
                return Err(Diagnostic::exception("RangeError", "Invalid array length"));
            }
            if length > 1_000_000.0 {
                return Err(Diagnostic::exception(
                    "Error",
                    "Sparse frontmatter array length compatibility remains pending.",
                ));
            }
            let null = self.import_value(Value::Null);
            values.resize(length as usize, null);
            count = values.len();
        } else if let Some(index) = name
            .parse::<u32>()
            .ok()
            .filter(|index| *index < u32::MAX && index.to_string() == name)
        {
            let index = index as usize;
            if let Some(value) = value {
                if index > 1_000_000 {
                    return Err(Diagnostic::exception(
                        "Error",
                        "Sparse frontmatter array index compatibility remains pending.",
                    ));
                }
                let id = self.import_value(value);
                if index >= values.len() {
                    let null = self.import_value(Value::Null);
                    values.resize(index + 1, null);
                }
                values[index] = id;
                count = values.len();
            } else if index < values.len() {
                values[index] = self.import_value(Value::Null);
                count -= 1;
            }
        } else {
            return self
                .replace_own_property_units(&name.encode_utf16().collect::<Vec<_>>(), value);
        }
        count += self.own_utf16_properties.get(&self.root).map_or_else(
            || self.own_properties.get(&self.root).map_or(0, Vec::len),
            Vec::len,
        );
        self.nodes[self.root] = Node::Array(values);
        Ok(Some(count))
    }

    /// Assign/delete an own Object field. Values supplied by CLI callbacks are
    /// ordinary JavaScript values; existing constructor nodes retain identity.
    pub fn replace_own_property(
        &mut self,
        name: &str,
        value: Option<Value>,
    ) -> Result<Option<usize>, Diagnostic> {
        if matches!(
            self.nodes[self.root],
            Node::Array(_) | Node::ObjectWithUtf16Keys(_)
        ) || self.own_utf16_properties.contains_key(&self.root)
        {
            return self
                .replace_own_property_units(&name.encode_utf16().collect::<Vec<_>>(), value);
        }
        if self.root_fields().is_none() {
            return Ok(None);
        }
        if let Node::Binary(bytes) = &self.nodes[self.root] {
            let number = ordinary_number(&Value::String(name.to_owned()));
            if name == "-0" || crate::vault_search::ecmascript_number(number) == name {
                let count = self.root_fields().unwrap().len();
                if number.is_finite()
                    && number >= 0.0
                    && number.fract() == 0.0
                    && number < bytes.len() as f64
                    && name != "-0"
                {
                    let index = number as usize;
                    let Some(value) = value else {
                        return Err(Diagnostic::exception(
                            "TypeError",
                            &format!("Cannot delete property '{name}' of [object Uint8Array]"),
                        ));
                    };
                    let number = ordinary_number(&value);
                    let byte = if number.is_finite() {
                        number.trunc().rem_euclid(256.0) as u8
                    } else {
                        0
                    };
                    let node = self.import_value(Value::Number(byte.into()));
                    if let Node::Binary(bytes) = &mut self.nodes[self.root] {
                        bytes[index] = byte;
                    }
                    self.own_properties.get_mut(&self.root).unwrap()[index].1 = node;
                }
                return Ok(Some(count));
            }
        }
        let replacement = value.map(|value| self.import_value(value));
        let fields = if let Node::Object(fields) = &mut self.nodes[self.root] {
            fields
        } else {
            self.own_properties.get_mut(&self.root).unwrap()
        };
        if let Some(id) = replacement {
            if let Some((_, old)) = fields.iter_mut().find(|(key, _)| key == name) {
                *old = id;
            } else {
                fields.push((name.into(), id));
                sort_fields(fields);
            }
        } else {
            fields.retain(|(key, _)| key != name);
        }
        Ok(Some(fields.len()))
    }
    pub fn replace_own_property_units(
        &mut self,
        name: &[u16],
        value: Option<Value>,
    ) -> Result<Option<usize>, Diagnostic> {
        let object = matches!(
            self.nodes[self.root],
            Node::Object(_) | Node::ObjectWithUtf16Keys(_)
        );
        let array = matches!(self.nodes[self.root], Node::Array(_));
        let text = String::from_utf16(name).ok();
        if array
            && text.as_ref().is_some_and(|name| {
                name == "length"
                    || name == "__proto__"
                    || name
                        .parse::<u32>()
                        .ok()
                        .is_some_and(|n| n < u32::MAX && n.to_string() == *name)
            })
        {
            return self.modify_root_array(text.as_deref().unwrap(), value);
        }
        if !object
            && !array
            && !matches!(
                self.nodes[self.root],
                Node::Date(_) | Node::Binary(_) | Node::Map(_) | Node::Set(_)
            )
        {
            return Ok(None);
        }
        // Typed-array canonical numeric keys write a byte, never a named field.
        if let (Node::Binary(bytes), Some(text)) = (&self.nodes[self.root], text.as_ref()) {
            let number = ordinary_number(&Value::String(text.clone()));
            if text == "-0" || crate::vault_search::ecmascript_number(number) == *text {
                let count = self.own_fields_units().len();
                if number.is_finite()
                    && number >= 0.0
                    && number.fract() == 0.0
                    && number < bytes.len() as f64
                    && text != "-0"
                {
                    let index = number as usize;
                    let Some(value) = value else {
                        return Err(Diagnostic::exception(
                            "TypeError",
                            &format!("Cannot delete property '{text}' of [object Uint8Array]"),
                        ));
                    };
                    let number = ordinary_number(&value);
                    let byte = if number.is_finite() {
                        number.trunc().rem_euclid(256.0) as u8
                    } else {
                        0
                    };
                    let node = self.import_value(Value::Number(byte.into()));
                    if let Node::Binary(bytes) = &mut self.nodes[self.root] {
                        bytes[index] = byte;
                    }
                    if let Some(fields) = self.own_utf16_properties.get_mut(&self.root) {
                        fields[index].1 = node;
                    } else {
                        self.own_properties.get_mut(&self.root).unwrap()[index].1 = node;
                    }
                }
                return Ok(Some(count));
            }
        }
        let replacement = value.map(|value| self.import_value(value));
        let initial = if array {
            self.own_utf16_properties
                .get(&self.root)
                .map(|v| v.iter().map(|(k, id)| (k.units(), *id)).collect())
                .unwrap_or_else(|| {
                    self.own_properties
                        .get(&self.root)
                        .map(|v| {
                            v.iter()
                                .map(|(k, id)| (k.encode_utf16().collect(), *id))
                                .collect()
                        })
                        .unwrap_or_default()
                })
        } else {
            self.own_fields_units()
        };
        let mut fields: Vec<_> = initial
            .into_iter()
            .map(|(key, id)| (ObjectKey::from_units(key), id))
            .collect();
        if let Some(id) = replacement {
            if let Some((_, old)) = fields.iter_mut().find(|(key, _)| key.units() == name) {
                *old = id;
            } else {
                fields.push((ObjectKey::from_units(name.to_vec()), id));
                sort_unit_fields(&mut fields)
            }
        } else {
            fields.retain(|(key, _)| key.units() != name)
        }
        let count = fields.len()
            + if let Node::Array(values) = &self.nodes[self.root] {
                values.len()
            } else {
                0
            };
        if !object {
            self.own_properties.remove(&self.root);
            self.own_utf16_properties.insert(self.root, fields);
            return Ok(Some(count));
        }
        self.nodes[self.root] = if fields.iter().all(|(key, _)| key.text().is_some()) {
            Node::Object(
                fields
                    .into_iter()
                    .map(|(key, id)| (key.text().unwrap().to_owned(), id))
                    .collect(),
            )
        } else {
            Node::ObjectWithUtf16Keys(fields)
        };
        Ok(Some(count))
    }
    fn import_value(&mut self, value: Value) -> NodeId {
        let node = match value {
            Value::Null => Node::Null,
            Value::Bool(value) => Node::Bool(value),
            Value::Number(value) => Node::Number(value),
            Value::String(value) => Node::String(value),
            Value::Sequence(values) => Node::Array(
                values
                    .into_iter()
                    .map(|value| self.import_value(value))
                    .collect(),
            ),
            Value::Mapping(values) => {
                let mut fields: Vec<_> = values
                    .into_iter()
                    .map(|(key, value)| {
                        (
                            crate::yaml::javascript_string(&key),
                            self.import_value(value),
                        )
                    })
                    .collect();
                sort_fields(&mut fields);
                Node::Object(fields)
            }
            Value::Tagged(value) => return self.import_value(value.value),
        };
        let id = self.nodes.len();
        self.nodes.push(node);
        id
    }
    pub fn update_string(&mut self, key: &str, text: &str) {
        self.update_string_units(&key.encode_utf16().collect::<Vec<_>>(), text)
    }
    pub fn update_string_units(&mut self, key: &[u16], text: &str) {
        let parts: Vec<_> = key.split(|unit| *unit == 46).collect();
        let mut node = self.root;
        for (index, part) in parts.iter().enumerate() {
            let next = self.fields_units(node).and_then(|fields| {
                fields
                    .into_iter()
                    .find(|(key, _)| key == part)
                    .map(|(_, id)| id)
            });
            if index + 1 == parts.len() {
                let id = self.nodes.len();
                self.nodes.push(Node::String(text.into()));
                match &mut self.nodes[node] {
                    Node::Object(fields) => {
                        if let Some((_, value)) = fields
                            .iter_mut()
                            .find(|(key, _)| key.encode_utf16().eq(part.iter().copied()))
                        {
                            *value = id
                        } else if let Ok(key) = String::from_utf16(part) {
                            fields.push((key, id));
                            sort_fields(fields)
                        } else {
                            let mut converted = fields
                                .iter()
                                .map(|(key, id)| (ObjectKey::String(key.clone()), *id))
                                .collect::<Vec<_>>();
                            converted.push((ObjectKey::from_units(part.to_vec()), id));
                            sort_unit_fields(&mut converted);
                            self.nodes[node] = Node::ObjectWithUtf16Keys(converted);
                        }
                    }
                    Node::ObjectWithUtf16Keys(fields) => {
                        if let Some((_, value)) =
                            fields.iter_mut().find(|(key, _)| key.units() == *part)
                        {
                            *value = id
                        } else {
                            fields.push((ObjectKey::from_units(part.to_vec()), id));
                            sort_unit_fields(fields)
                        }
                    }
                    Node::Array(ids) => {
                        if let Ok(index) = String::from_utf16(part)
                            .unwrap_or_default()
                            .parse::<usize>()
                        {
                            if let Some(value) = ids.get_mut(index) {
                                *value = id
                            }
                        }
                    }
                    _ => {}
                }
            } else {
                let Some(next) = next else { return };
                node = next
            }
        }
    }
}
