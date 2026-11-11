//! YAML frontmatter parsing, matching the desktop app's delimiter and tag/alias
//! extraction rules 

use std::collections::BTreeMap;

use serde_yaml::Value;

/// A `[[link]]` declared as a frontmatter property value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrontmatterLink {
    /// Dotted property path, e.g. `related` or `links.0`.
    pub key: String,
    pub key_units: Option<Vec<u16>>,
    /// The link target (path plus optional `#subpath`).
    pub link: String,
    /// The display text (alias, or the target rendered with `>`).
    pub display_text: String,
    /// Original decoded property value, used when updating links on rename.
    pub original: String,
}

impl serde::Serialize for FrontmatterLink {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        let mut map = serializer.serialize_map(Some(3))?;
        if let Some(units) = &self.key_units {
            let raw =
                serde_json::value::RawValue::from_string(crate::yaml_value::quote_units(units))
                    .map_err(serde::ser::Error::custom)?;
            map.serialize_entry("key", &raw)?
        } else {
            map.serialize_entry("key", &self.key)?
        }
        map.serialize_entry("link", &self.link)?;
        map.serialize_entry("display_text", &self.display_text)?;
        map.end()
    }
}

/// Parsed frontmatter for a single note.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct Frontmatter {
    /// The raw YAML text between the delimiters.
    pub raw: String,
    /// The parsed top-level mapping.
    pub data: BTreeMap<String, Value>,
    /// Byte range `[start, end)` of the full block (including delimiters)
    /// within the note source, when present.
    pub span: Option<(usize, usize)>,
}

impl Frontmatter {
    /// Returns all `tags` declared in the frontmatter, each prefixed with `#`
    /// (matching the plugin API's `parseFrontMatterTags`). Supports a sequence
    /// (`tags: [a, b]`) or a scalar string (`tags: a`). Values containing a
    /// literal space are dropped.
    pub fn tags(&self) -> Vec<String> {
        match parse_string_array(&self.data, "tags") {
            Some(list) => list
                .into_iter()
                .filter(|t| !t.is_empty() && !t.contains(' '))
                .map(|t| {
                    if t.starts_with('#') {
                        t
                    } else {
                        format!("#{t}")
                    }
                })
                .collect(),
            None => Vec::new(),
        }
    }

    /// Returns all `aliases` declared in the frontmatter.
    pub fn aliases(&self) -> Vec<String> {
        match parse_string_array(&self.data, "aliases") {
            Some(list) => list
                .into_iter()
                .map(|t| t.trim().to_string())
                .filter(|t| !t.is_empty())
                .collect(),
            None => Vec::new(),
        }
    }

    /// Returns all `[[link]]` values declared as frontmatter properties
    /// (the plugin API's `parseFrontMatterLinks`).
    pub fn links(&self) -> Vec<FrontmatterLink> {
        if let Ok(document) = crate::yaml_graph::parse_frontmatter(&self.raw) {
            return links_from_document(&document);
        }
        let mut out = Vec::new();
        for (k, v) in &self.data {
            collect_links(v, k, &mut out);
        }
        out
    }

    /// Looks up a property by exact key.
    pub fn get_str(&self, key: &str) -> Option<&str> {
        self.data.get(key).and_then(Value::as_str)
    }

    /// Looks up a property as a boolean by exact key.
    pub fn get_bool(&self, key: &str) -> Option<bool> {
        self.data.get(key).and_then(Value::as_bool)
    }

    /// Looks up a property by case-insensitive key (e.g. `tags`, `Tags`).
    pub fn get_str_ci(&self, key: &str) -> Option<&str> {
        find_key_ci(&self.data, key).and_then(Value::as_str)
    }
}

/// Parses a leading `---` frontmatter block.
///
/// `FE = /^---(\r?\n)/`, `NE = /---(\r?\n|$)/`. The closing delimiter must be
/// preceded by a newline (i.e. at line start); `...` is not a delimiter.
///
/// Returns the parsed frontmatter (if any) and the byte offset at which the
/// note body begins.
pub fn parse(source: &str) -> (Option<Frontmatter>, usize) {
    let (source, bom) = match source.strip_prefix('\u{feff}') {
        Some(s) => (s, 3),
        None => (source, 0),
    };

    let (open_len, rest) = if let Some(r) = source.strip_prefix("---\n") {
        (4, r)
    } else if let Some(r) = source.strip_prefix("---\r\n") {
        (5, r)
    } else {
        return (None, bom);
    };

    let mut content = String::new();
    let mut pos = 0usize;
    let mut closed = false;

    for line in rest.split_inclusive('\n') {
        let (body, had_newline) = match line.strip_suffix('\n') {
            Some(b) => (b, true),
            None => (line, false),
        };
        let body = body.strip_suffix('\r').unwrap_or(body);
        if body == "---" {
            closed = true;
            pos += line.len();
            break;
        }
        let _ = had_newline;
        content.push_str(line);
        pos += line.len();
    }

    if !closed {
        return (None, bom);
    }

    let raw = content;
    let data = parse_yaml_mapping(&raw);
    let content_start = bom + open_len + pos;

    (
        Some(Frontmatter {
            raw,
            data,
            span: Some((bom, content_start)),
        }),
        content_start,
    )
}

fn parse_yaml_mapping(raw: &str) -> BTreeMap<String, Value> {
    match crate::yaml::parse_metadata_mapping(raw) {
        Ok(Some(map)) => map
            .into_iter()
            .filter_map(|(k, v)| k.as_str().map(|k| (k.to_owned(), v)))
            .collect(),
        _ => BTreeMap::new(),
    }
}

fn find_key_ci<'a>(data: &'a BTreeMap<String, Value>, key: &str) -> Option<&'a Value> {
    data.iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(key))
        .map(|(_, v)| v)
}

fn parse_string_array(data: &BTreeMap<String, Value>, key: &str) -> Option<Vec<String>> {
    match find_key_ci(data, key)? {
        Value::String(s) => Some(vec![s.trim().to_string()]),
        Value::Sequence(seq) => Some(
            seq.iter()
                .filter_map(Value::as_str)
                .map(|s| s.trim().to_string())
                .collect(),
        ),
        _ => None,
    }
}

fn collect_links(value: &Value, prefix: &str, out: &mut Vec<FrontmatterLink>) {
    match value {
        Value::String(s) => {
            let s = s.as_str();
            if let Some(inner) = s.strip_prefix("[[").and_then(|x| x.strip_suffix("]]")) {
                let (href, alias) = match inner.find('|') {
                    Some(p) => (inner[..p].trim(), Some(inner[p + 1..].trim())),
                    None => (inner.trim(), None),
                };
                let display_text = match alias {
                    Some(a) if !a.is_empty() => a.to_string(),
                    _ => href
                        .split('#')
                        .filter(|s| !s.is_empty())
                        .collect::<Vec<_>>()
                        .join(" > "),
                };
                out.push(FrontmatterLink {
                    key: prefix.to_string(),
                    key_units: None,
                    link: href.to_string(),
                    display_text,
                    original: s.to_string(),
                });
            } else if s.starts_with('[') && s.ends_with(')') {
                // Frontmatter also recognizes complete inline Markdown links,
                // but not embeds or links embedded within a longer string.
                let parsed = crate::markdown::parse(s);
                if let Some(link) = parsed.links.first().filter(|link| {
                    link.start == 0
                        && link.end == s.len()
                        && crate::link::is_internal_link(&link.destination)
                }) {
                    out.push(FrontmatterLink {
                        key: prefix.to_string(),
                        key_units: None,
                        link: crate::rewrite::decode_uri(&link.destination)
                            .unwrap_or_else(|| link.destination.clone()),
                        display_text: link.text.clone(),
                        original: s.to_string(),
                    });
                }
            }
        }
        Value::Sequence(seq) => {
            for (i, v) in seq.iter().enumerate() {
                let key = if prefix.is_empty() {
                    i.to_string()
                } else {
                    format!("{prefix}.{i}")
                };
                collect_links(v, &key, out);
            }
        }
        Value::Mapping(map) => {
            for (k, v) in map {
                let name = k.as_str().unwrap_or_default();
                let key = if prefix.is_empty() {
                    name.to_string()
                } else {
                    format!("{prefix}.{name}")
                };
                collect_links(v, &key, out);
            }
        }
        _ => {}
    }
}

/// Raw adapter-text boundaries used by processFrontMatter. Unlike cached
/// metadata parsing, this path deliberately does not remove a leading BOM.
#[derive(Debug, Clone, Copy)]
pub struct ProcessingBounds {
    pub from: usize,
    pub to: usize,
    pub content_start: usize,
}

pub fn processing_bounds(source: &str) -> Option<ProcessingBounds> {
    let from = if source.starts_with("---\n") {
        4
    } else if source.starts_with("---\r\n") {
        5
    } else {
        return None;
    };
    let mut position = from;
    for line in source[from..].split_inclusive('\n') {
        let plain = line
            .strip_suffix('\n')
            .unwrap_or(line)
            .strip_suffix('\r')
            .unwrap_or_else(|| line.strip_suffix('\n').unwrap_or(line));
        if plain == "---" {
            return Some(ProcessingBounds {
                from,
                to: position,
                content_start: position + line.len(),
            });
        }
        position += line.len();
    }
    None
}

/// Set (Some) or delete (None) a property with the shipped frontmatter
/// operation's object/array/scalar handling and delimiter preservation.
pub fn process_property(
    source: &str,
    name: &str,
    value: Option<Value>,
) -> Result<String, crate::yaml::YamlError> {
    let bounds = processing_bounds(source);
    if let Some(bounds) = bounds {
        if let Ok(mut document) = crate::yaml_graph::parse(&source[bounds.from..bounds.to]) {
            if let Some(count) = document
                .modify_root_array(name, value.clone())
                .map_err(crate::yaml::YamlError::from)?
            {
                if count == 0 {
                    return Ok(source[bounds.content_start..].to_owned());
                }
                let serialized = document.stringify().map_err(crate::yaml::YamlError::from)?;
                return Ok(format!(
                    "{}{}{}",
                    &source[..bounds.from],
                    serialized,
                    &source[bounds.to..]
                ));
            }
            // The prototype setter/iterator and Array callback branches below
            // retain their separately observed handling. Object assignment must
            // keep Date/Binary/Map/Set and shared aliases in unrelated fields.
            if document.fields_units(document.root).is_some()
                && (name != "__proto__" || document.own_property(name).is_some())
            {
                let count = document
                    .replace_own_property(name, value.clone())
                    .map_err(crate::yaml::YamlError::from)?
                    .unwrap();
                if count == 0 {
                    return Ok(source[bounds.content_start..].to_owned());
                }
                let serialized = document
                    .stringify()
                    .map_err(|error| crate::yaml::YamlError {
                        exception_name: None,
                        message: error.message,
                    })?;
                return Ok(format!(
                    "{}{}{}",
                    &source[..bounds.from],
                    serialized,
                    &source[bounds.to..]
                ));
            }
        }
    }
    let mut object = match bounds {
        Some(bounds) => crate::yaml::parse_js(&source[bounds.from..bounds.to])?,
        None => Value::Mapping(serde_yaml::Mapping::new()),
    };
    if !matches!(object, Value::Mapping(_) | Value::Sequence(_)) {
        object = Value::Mapping(serde_yaml::Mapping::new());
    }
    let mut enumerable_count = match &object {
        Value::Mapping(map) => map.len(),
        Value::Sequence(values) => values.len(),
        _ => unreachable!(),
    };
    let mut serialized_value = None;
    match &mut object {
        Value::Mapping(map) => {
            let key = Value::String(name.to_owned());
            if name == "__proto__" && !map.contains_key(&key) {
                // Assignment reaches Object.prototype's setter. Primitive
                // values are ignored; an array supplies an inherited iterator
                // used by the pinned YAML createNode/sequence conversion.
                if let Some(Value::Sequence(mut values)) = value {
                    for (index, item) in values.iter_mut().enumerate() {
                        if let Some(own) = map.get(Value::String(index.to_string())) {
                            *item = own.clone();
                        }
                    }
                    serialized_value = Some(Value::Sequence(values));
                }
            } else {
                if let Some(value) = value {
                    map.insert(key, value);
                } else {
                    map.shift_remove(&key);
                }
            }
            enumerable_count = map.len();
        }
        Value::Sequence(values) => {
            if name == "length" {
                if let Some(value) = value {
                    let text = crate::yaml::javascript_string(&value);
                    let number = match value {
                        Value::Null => 0.0,
                        Value::Bool(v) => {
                            if v {
                                1.0
                            } else {
                                0.0
                            }
                        }
                        _ => text.parse::<f64>().unwrap_or(f64::NAN),
                    };
                    if number < 0.0
                        || number > u32::MAX as f64
                        || number.fract() != 0.0
                        || !number.is_finite()
                    {
                        return Err(crate::yaml::YamlError {
                            exception_name: None,
                            message: "Invalid array length".into(),
                        });
                    }
                    let length = number as usize;
                    if length > 1_000_000 {
                        return Err(crate::yaml::YamlError {
                            exception_name: None,
                            message:
                                "Sparse frontmatter array length compatibility remains pending."
                                    .into(),
                        });
                    }
                    values.resize(length, Value::Null);
                    enumerable_count = length;
                }
            } else {
                let index = name
                    .parse::<usize>()
                    .ok()
                    .filter(|index| index.to_string() == name && *index < u32::MAX as usize);
                if let Some(index) = index {
                    if let Some(value) = value {
                        // Large sparse arrays would need a sparse representation;
                        // this dense storage stops at the boundary below.
                        if index > 1_000_000 {
                            return Err(crate::yaml::YamlError {
                                exception_name: None,
                                message:
                                    "Sparse frontmatter array index compatibility remains pending."
                                        .into(),
                            });
                        }
                        if index >= values.len() {
                            values.resize(index + 1, Value::Null);
                        }
                        values[index] = value;
                        enumerable_count = values.len();
                    } else if index < values.len() {
                        values[index] = Value::Null;
                        enumerable_count -= 1;
                    }
                } else if value.is_some() {
                    // Array named properties are enumerable but YAML's sequence
                    // serialization ignores them, matching the native operation.
                    enumerable_count += 1;
                }
            }
        }
        _ => unreachable!(),
    }
    if enumerable_count == 0 {
        return Ok(bounds.map_or_else(
            || source.to_owned(),
            |bounds| source[bounds.content_start..].to_owned(),
        ));
    }
    let serialized = crate::yaml::stringify(serialized_value.as_ref().unwrap_or(&object));
    Ok(match bounds {
        Some(bounds) => format!(
            "{}{}{}",
            &source[..bounds.from],
            serialized,
            &source[bounds.to..]
        ),
        None => format!("---\n{serialized}---\n{source}"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_frontmatter() {
        let (fm, off) = parse("# Hello\n\nbody");
        assert!(fm.is_none());
        assert_eq!(off, 0);
    }

    #[test]
    fn parses_frontmatter() {
        let src = "---\ntitle: Foo\ntags: [a, b]\n---\nBody";
        let (fm, off) = parse(src);
        let fm = fm.unwrap();
        assert_eq!(fm.get_str("title"), Some("Foo"));
        assert_eq!(fm.tags(), vec!["#a", "#b"]);
        assert_eq!(off, src.len() - "Body".len());
    }

    #[test]
    fn crlf_delimiter() {
        let (fm, off) = parse("---\r\ntitle: Foo\r\n---\r\nBody");
        let fm = fm.unwrap();
        assert_eq!(fm.get_str("title"), Some("Foo"));
        assert_eq!(off, "---\r\ntitle: Foo\r\n---\r\n".len());
    }

    #[test]
    fn scalar_tag() {
        let (fm, _) = parse("---\ntags: foo\n---\n");
        assert_eq!(fm.unwrap().tags(), vec!["#foo"]);
    }

    #[test]
    fn tag_already_hashed() {
        let (fm, _) = parse("---\ntags: [\"#a\", b]\n---\n");
        assert_eq!(fm.unwrap().tags(), vec!["#a", "#b"]);
    }

    #[test]
    fn tag_with_space_dropped() {
        let (fm, _) = parse("---\ntags: [good, \"has space\"]\n---\n");
        assert_eq!(fm.unwrap().tags(), vec!["#good"]);
    }

    #[test]
    fn unclosed_is_not_frontmatter() {
        let (fm, off) = parse("---\ntitle: Foo\nBody");
        assert!(fm.is_none());
        assert_eq!(off, 0);
    }

    #[test]
    fn dots_is_not_a_closing_delimiter() {
        // `...` is not accepted as a closing delimiter.
        let (fm, off) = parse("---\ntitle: Foo\n...\nBody");
        assert!(fm.is_none());
        assert_eq!(off, 0);
    }

    #[test]
    fn mid_line_dashes_not_closing() {
        // `---` only closes at line start (preceded by a newline); a `---`
        // inside a YAML value does not.
        let src = "---\ntitle: Foo\ndesc: \"a --- b\"\n---\nBody";
        let (fm, off) = parse(src);
        let fm = fm.unwrap();
        assert_eq!(fm.get_str("title"), Some("Foo"));
        assert_eq!(fm.get_str("desc"), Some("a --- b"));
        assert_eq!(off, src.len() - "Body".len());
    }

    #[test]
    fn aliases() {
        let (fm, _) = parse("---\naliases: [Foo, \"Bar Baz\"]\n---\n");
        assert_eq!(fm.unwrap().aliases(), vec!["Foo", "Bar Baz"]);
    }

    #[test]
    fn property_links_require_whole_value_and_decode_markdown_urls() {
        let (fm, _) = parse("---\nwiki: '[[Target]]'\nmarkdown: '[label](Old%20Name.md)'\npadded: ' [[Target]] '\nembedded: 'prefix [[Target]]'\nexternal: '[web](https://example.com)'\nembed: '![[Target]]'\n---\n");
        let links = fm.unwrap().links();
        assert_eq!(
            links
                .iter()
                .map(|link| (link.key.as_str(), link.link.as_str()))
                .collect::<Vec<_>>(),
            [("wiki", "Target"), ("markdown", "Old Name.md")]
        );
    }
}

/// Native traversal enumerates Object/Array own keys, preserving dotted UTF16 paths.
pub fn links_from_document(document: &crate::yaml_graph::Document) -> Vec<FrontmatterLink> {
    use crate::yaml_value::Node;
    fn walk(
        document: &crate::yaml_graph::Document,
        id: usize,
        key: &[u16],
        active: &mut std::collections::BTreeSet<usize>,
        out: &mut Vec<FrontmatterLink>,
    ) {
        if !active.insert(id) {
            return;
        }
        match &document.nodes[id] {
            Node::String(text) => {
                let mut links = Vec::new();
                collect_links(&Value::String(text.clone()), "", &mut links);
                for mut link in links {
                    link.key = String::from_utf16(key).unwrap_or_default();
                    link.key_units = String::from_utf16(key).is_err().then(|| key.to_vec());
                    out.push(link)
                }
            }
            Node::Object(_) | Node::ObjectWithUtf16Keys(_) => {
                for (field, child) in document.fields_units(id).unwrap() {
                    let mut path = key.to_vec();
                    if !path.is_empty() {
                        path.push(46)
                    }
                    path.extend(field);
                    walk(document, child, &path, active, out)
                }
            }
            Node::Array(values) => {
                for (index, child) in values.iter().enumerate() {
                    let mut path = key.to_vec();
                    if !path.is_empty() {
                        path.push(46)
                    }
                    path.extend(index.to_string().encode_utf16());
                    walk(document, *child, &path, active, out)
                }
            }
            _ => {}
        }
        active.remove(&id);
    }
    let mut out = Vec::new();
    walk(
        document,
        document.root,
        &[],
        &mut Default::default(),
        &mut out,
    );
    out
}

pub(crate) fn rewrite_links_units(raw: &str, updates: &[(Vec<u16>, String)]) -> Option<String> {
    let mut document = crate::yaml_graph::parse(raw).ok()?;
    for (key, text) in updates {
        document.update_string_units(key, text)
    }
    document.stringify().ok()
}
