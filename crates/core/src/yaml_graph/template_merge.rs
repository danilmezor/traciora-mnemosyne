//! Typed counterpart of the shipped mB template-property merge. No parse or
//! JSON projection occurs here: aliases, constructor identities and UTF16 names
//! remain graph nodes. Engine insertion owns parsing, warnings and file effects.
use super::{sort_unit_fields, Document};
use crate::yaml_value::{Diagnostic, Node, NodeId, ObjectKey};
use std::collections::BTreeSet;

impl Document {
    /// ZB.serialize creates a fresh plain editor object before mB(existing,t).
    pub fn merged_template_properties(&self, template: &Document) -> Result<Document, Diagnostic> {
        let mut document = self.clone();
        let incoming = document.import_document(template);
        let fields = enumerable_fields(&mut document, self.root)?;
        let root = document.nodes.len();
        document.nodes.push(Node::Object(Vec::new()));
        for (key, value) in fields {
            set_property(&mut document, root, &key, value)?;
        }
        document.root = root;
        merge(&mut document, root, incoming)?;
        Ok(document)
    }
    fn import_document(&mut self, source: &Document) -> NodeId {
        let offset = self.nodes.len();
        for node in &source.nodes {
            self.nodes.push(match node {
                Node::Array(values) => Node::Array(values.iter().map(|id| id + offset).collect()),
                Node::Object(fields) => Node::Object(
                    fields
                        .iter()
                        .map(|(key, id)| (key.clone(), id + offset))
                        .collect(),
                ),
                Node::ObjectWithUtf16Keys(fields) => Node::ObjectWithUtf16Keys(
                    fields
                        .iter()
                        .map(|(key, id)| (key.clone(), id + offset))
                        .collect(),
                ),
                Node::Set(values) => Node::Set(values.iter().map(|id| id + offset).collect()),
                Node::Map(values) => Node::Map(
                    values
                        .iter()
                        .map(|(key, value)| (key + offset, value + offset))
                        .collect(),
                ),
                value => value.clone(),
            });
        }
        self.own_properties
            .extend(source.own_properties.iter().map(|(id, fields)| {
                (
                    id + offset,
                    fields
                        .iter()
                        .map(|(key, value)| (key.clone(), value + offset))
                        .collect(),
                )
            }));
        self.own_utf16_properties
            .extend(source.own_utf16_properties.iter().map(|(id, fields)| {
                (
                    id + offset,
                    fields
                        .iter()
                        .map(|(key, value)| (key.clone(), value + offset))
                        .collect(),
                )
            }));
        self.flow_children.extend(
            source
                .flow_children
                .iter()
                .map(|(id, values)| (id + offset, values.iter().map(|id| id + offset).collect())),
        );
        self.flow_aliases.extend(
            source
                .flow_aliases
                .iter()
                .map(|(id, values)| (id + offset, values.clone())),
        );
        self.flow_quoted
            .extend(source.flow_quoted.iter().map(|id| id + offset));
        self.flow_single_quoted
            .extend(source.flow_single_quoted.iter().map(|id| id + offset));
        self.warnings.extend(source.warnings.clone());
        self.console_warnings
            .extend(source.console_warnings.clone());
        source.root + offset
    }
}
fn is_object(node: &Node) -> bool {
    matches!(
        node,
        Node::Object(_)
            | Node::ObjectWithUtf16Keys(_)
            | Node::Array(_)
            | Node::Date(_)
            | Node::Binary(_)
            | Node::Set(_)
            | Node::Map(_)
    )
}
fn truthy(document: &Document, id: NodeId) -> bool {
    match &document.nodes[id] {
        Node::Null => false,
        Node::Bool(value) => *value,
        Node::Number(value) => value
            .as_f64()
            .is_some_and(|value| value != 0.0 && !value.is_nan()),
        Node::String(value) => !value.is_empty(),
        Node::Wtf16String(value) => !value.is_empty(),
        _ => true,
    }
}
fn enumerable_fields(
    document: &mut Document,
    id: NodeId,
) -> Result<Vec<(Vec<u16>, NodeId)>, Diagnostic> {
    if matches!(document.nodes[id], Node::Null) {
        return Err(Diagnostic::exception(
            "TypeError",
            "Cannot convert undefined or null to object",
        ));
    }
    let text = match &document.nodes[id] {
        Node::String(value) => Some(value.encode_utf16().collect::<Vec<_>>()),
        Node::Wtf16String(units) => Some(units.clone()),
        _ => None,
    };
    if let Some(text) = text {
        return Ok(text
            .into_iter()
            .enumerate()
            .map(|(index, unit)| {
                let id = document.nodes.len();
                document.nodes.push(match String::from_utf16(&[unit]) {
                    Ok(text) => Node::String(text),
                    Err(_) => Node::Wtf16String(vec![unit]),
                });
                (index.to_string().encode_utf16().collect(), id)
            })
            .collect());
    }
    Ok(document.fields_units(id).unwrap_or_default())
}
fn own_property(document: &mut Document, id: NodeId, key: &[u16]) -> Option<NodeId> {
    enumerable_fields(document, id)
        .ok()?
        .into_iter()
        .find(|(name, _)| name == key)
        .map(|(_, id)| id)
}
fn method_exists(document: &mut Document, id: NodeId) -> bool {
    own_property(
        document,
        id,
        &"hasOwnProperty".encode_utf16().collect::<Vec<_>>(),
    )
    .is_none()
}
fn array_index(key: &[u16]) -> Option<usize> {
    let text = String::from_utf16(key).ok()?;
    let index = text.parse::<u32>().ok()?;
    (index < u32::MAX && index.to_string() == text).then_some(index as usize)
}
fn same_value_zero(document: &Document, left: NodeId, right: NodeId) -> bool {
    if left == right {
        return true;
    }
    match (&document.nodes[left], &document.nodes[right]) {
        (Node::Null, Node::Null) => true,
        (Node::Bool(a), Node::Bool(b)) => a == b,
        (Node::Number(a), Node::Number(b)) => {
            let (a, b) = (a.as_f64().unwrap(), b.as_f64().unwrap());
            a == b || (a.is_nan() && b.is_nan())
        }
        (Node::String(a), Node::String(b)) => a == b,
        (Node::Wtf16String(a), Node::Wtf16String(b)) => a == b,
        (Node::String(a), Node::Wtf16String(b)) | (Node::Wtf16String(b), Node::String(a)) => {
            a.encode_utf16().eq(b.iter().copied())
        }
        (Node::Merge, Node::Merge) => true,
        _ => false,
    }
}
// Prototype mutation and sparse/typed-array writes have no captured insertion
// contract yet. Keep that limitation visible instead of synthesizing JS objects.
fn boundary(message: &str) -> Diagnostic {
    Diagnostic::exception("TemplateMergeBoundary", message)
}
fn set_property(
    document: &mut Document,
    id: NodeId,
    key: &[u16],
    value: NodeId,
) -> Result<(), Diagnostic> {
    if key == "__proto__".encode_utf16().collect::<Vec<_>>() {
        return Err(boundary("Template prototype mutation is not supported"));
    }
    if let Node::Array(values) = &mut document.nodes[id] {
        if key == "length".encode_utf16().collect::<Vec<_>>() {
            return Err(boundary("Template array length mutation is not supported"));
        }
        if let Some(index) = array_index(key) {
            if index < values.len() {
                values[index] = value;
            } else if index == values.len() {
                values.push(value);
            } else {
                return Err(boundary("Template sparse array mutation is not supported"));
            }
            return Ok(());
        }
    }
    if matches!(document.nodes[id], Node::Binary(_)) && array_index(key).is_some() {
        return Err(boundary("Template typed-array mutation is not supported"));
    }
    set_named_property(document, id, key, value)
}
fn set_named_property(
    document: &mut Document,
    id: NodeId,
    key: &[u16],
    value: NodeId,
) -> Result<(), Diagnostic> {
    let object = matches!(
        document.nodes[id],
        Node::Object(_) | Node::ObjectWithUtf16Keys(_)
    );
    let mut fields: Vec<(ObjectKey, NodeId)> = if object {
        document
            .fields_units(id)
            .unwrap_or_default()
            .into_iter()
            .map(|(key, id)| (ObjectKey::from_units(key), id))
            .collect()
    } else {
        document
            .own_utf16_properties
            .get(&id)
            .cloned()
            .unwrap_or_else(|| {
                document
                    .own_properties
                    .get(&id)
                    .map(|fields| {
                        fields
                            .iter()
                            .map(|(key, id)| (ObjectKey::String(key.clone()), *id))
                            .collect()
                    })
                    .unwrap_or_default()
            })
    };
    if let Some((_, old)) = fields.iter_mut().find(|(name, _)| name.units() == key) {
        *old = value
    } else {
        fields.push((ObjectKey::from_units(key.to_vec()), value));
        sort_unit_fields(&mut fields);
    }
    if object {
        document.nodes[id] = if fields.iter().all(|(key, _)| key.text().is_some()) {
            Node::Object(
                fields
                    .into_iter()
                    .map(|(key, id)| (key.text().unwrap().to_owned(), id))
                    .collect(),
            )
        } else {
            Node::ObjectWithUtf16Keys(fields)
        }
    } else {
        document.own_properties.remove(&id);
        document.own_utf16_properties.insert(id, fields);
    }
    Ok(())
}
struct Frame {
    existing: NodeId,
    incoming: NodeId,
    keys: Vec<Vec<u16>>,
    next: usize,
}
fn frame(
    document: &mut Document,
    existing: NodeId,
    incoming: NodeId,
) -> Result<Option<Frame>, Diagnostic> {
    let keys = enumerable_fields(document, incoming)?
        .into_iter()
        .map(|(key, _)| key)
        .collect::<Vec<_>>();
    Ok((!keys.is_empty()).then_some(Frame {
        existing,
        incoming,
        keys,
        next: 0,
    }))
}
fn merge(document: &mut Document, existing: NodeId, incoming: NodeId) -> Result<(), Diagnostic> {
    let Some(first) = frame(document, existing, incoming)? else {
        return Ok(());
    };
    let mut active = BTreeSet::from([(existing, incoming)]);
    let mut frames = vec![first];
    while let Some(current) = frames.last_mut() {
        if current.next == current.keys.len() {
            let finished = frames.pop().unwrap();
            active.remove(&(finished.existing, finished.incoming));
            continue;
        }
        let key = current.keys[current.next].clone();
        current.next += 1;
        let (existing, incoming) = (current.existing, current.incoming);
        if !method_exists(document, incoming) {
            return Err(Diagnostic::exception(
                "TypeError",
                "t.hasOwnProperty is not a function",
            ));
        }
        let Some(value) = own_property(document, incoming, &key) else {
            continue;
        };
        if !method_exists(document, existing) {
            return Err(Diagnostic::exception(
                "TypeError",
                "e.hasOwnProperty is not a function",
            ));
        }
        let old = own_property(document, existing, &key);
        if let Some(old) = old.filter(|old| truthy(document, *old)) {
            if let (Node::Array(left), Node::Array(right)) =
                (&document.nodes[old], &document.nodes[value])
            {
                let mut unique = Vec::new();
                for value in left.iter().chain(right).copied() {
                    if !unique
                        .iter()
                        .any(|old| same_value_zero(document, *old, value))
                    {
                        unique.push(value);
                    }
                }
                let merged = document.nodes.len();
                document.nodes.push(Node::Array(unique));
                set_property(document, existing, &key, merged)?;
            } else if is_object(&document.nodes[old]) && is_object(&document.nodes[value]) {
                if let Some(next) = frame(document, old, value)? {
                    if !active.insert((old, value)) {
                        return Err(Diagnostic::exception(
                            "RangeError",
                            "Maximum call stack size exceeded",
                        ));
                    }
                    frames.push(next);
                }
            } else if !matches!(document.nodes[value], Node::Null) {
                set_property(document, existing, &key, value)?;
            }
        } else {
            set_property(document, existing, &key, value)?;
        }
    }
    Ok(())
}
