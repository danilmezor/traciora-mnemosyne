//! Focused tests of the shipped mB branches; native insertion replay is separate.
use mnemosyne_core::{
    yaml_graph::{parse, Document},
    yaml_value::Node,
};
use serde_json::{json, Value};

fn merged(existing: &str, incoming: &str) -> Document {
    parse(existing)
        .unwrap()
        .merged_template_properties(&parse(incoming).unwrap())
        .unwrap()
}
fn value(document: &Document) -> Value {
    serde_json::from_str(&document.json(false).unwrap()).unwrap()
}

#[test]
fn falsy_values_are_replaced_and_null_preserves_truthy_values() {
    let document = merged(
        "zero: 0\nfalse: false\nempty: ''\nnil: null\ntruthy: keep\nlist: []\nobject: {}\n",
        "zero: 1\nfalse: true\nempty: text\nnil: value\ntruthy: null\nlist: null\nobject: null\nnew: null\n",
    );
    assert_eq!(
        value(&document),
        json!({"zero":1,"false":true,"empty":"text","nil":"value","truthy":"keep","list":[],"object":{},"new":null})
    );
}

#[test]
fn nested_objects_merge_and_scalar_values_replace() {
    let document = merged(
        "nested: {keep: original, change: old, inner: {a: 1}}\nscalar: old\n",
        "nested: {change: new, inner: {b: 2}, add: yes}\nscalar: {new: object}\n",
    );
    assert_eq!(
        value(&document),
        json!({"nested":{"keep":"original","change":"new","inner":{"a":1,"b":2},"add":"yes"},"scalar":{"new":"object"}})
    );
}

#[test]
fn arrays_use_primitive_equality_and_object_identity() {
    let document = merged(
        "items: [a, a, 0, -0, null, {x: 1}]\n",
        "shared: &shared {x: 1}\nitems: [a, b, null, *shared, *shared, {x: 1}]\n",
    );
    let Node::Array(items) = &document.nodes[document.own_property("items").unwrap()] else {
        panic!("array expected")
    };
    // One primitive a/zero/null, existing object, b, shared object, new object.
    assert_eq!(items.len(), 7);
    let shared = document.own_property("shared").unwrap();
    assert_eq!(items.iter().filter(|id| **id == shared).count(), 1);
    assert_eq!(
        items
            .iter()
            .filter(|id| matches!(document.nodes[**id], Node::Object(_)))
            .count(),
        3
    );
}

#[test]
fn recursive_mutation_preserves_existing_alias_identity() {
    let existing = parse("shared: &shared {left: 1}\ncopy: *shared\n").unwrap();
    let original = existing.own_property("shared").unwrap();
    let document = existing
        .merged_template_properties(&parse("copy: {right: 2}\n").unwrap())
        .unwrap();
    assert_eq!(document.own_property("shared"), Some(original));
    assert_eq!(document.own_property("copy"), Some(original));
    assert_eq!(
        value(&document),
        json!({"shared":{"left":1,"right":2},"copy":{"left":1,"right":2}})
    );
    assert_eq!(
        value(&existing),
        json!({"shared":{"left":1},"copy":{"left":1}})
    );
}

#[test]
fn imports_preserve_typed_values_and_utf16_property_names() {
    let document = merged("{}", "date: !!timestamp 2001-01-01\nbinary: !!binary AQI=\nset: !!set {a: null}\n\"\\uD800\": value\n");
    assert!(matches!(
        document.nodes[document.own_property("date").unwrap()],
        Node::Date(_)
    ));
    assert!(matches!(
        document.nodes[document.own_property("binary").unwrap()],
        Node::Binary(_)
    ));
    assert!(matches!(
        document.nodes[document.own_property("set").unwrap()],
        Node::Set(_)
    ));
    let id = document.own_property_units(&[0xd800]).unwrap();
    assert!(matches!(&document.nodes[id], Node::String(text) if text == "value"));
}

#[test]
fn has_own_property_shadow_and_recursive_cycle_report_source_errors() {
    let error = parse("{}")
        .unwrap()
        .merged_template_properties(&parse("hasOwnProperty: value").unwrap())
        .unwrap_err();
    assert_eq!(error.name, "TypeError");
    assert_eq!(error.message, "t.hasOwnProperty is not a function");
    let error = parse("obj: &x {self: *x}")
        .unwrap()
        .merged_template_properties(&parse("obj: &x {self: *x}").unwrap())
        .unwrap_err();
    assert_eq!(error.name, "RangeError");
    assert_eq!(error.message, "Maximum call stack size exceeded");
}

#[test]
fn uncaptured_prototype_mutation_reports_an_explicit_boundary() {
    let error = parse("{}")
        .unwrap()
        .merged_template_properties(&parse("__proto__: {hidden: true}").unwrap())
        .unwrap_err();
    assert_eq!(error.name, "TemplateMergeBoundary");
}
