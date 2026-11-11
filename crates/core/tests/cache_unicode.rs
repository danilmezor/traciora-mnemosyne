use mnemosyne_core::{cache::CachedMetadata, graph_refs, Note};
use serde_json::Value;

#[test]
fn cache_reference_normalization_matches_four_native_captures() {
    let fixture: Value = serde_json::from_str(include_str!("fixtures/cache_unicode.json")).unwrap();
    let mut differences = Vec::new();
    for case in fixture["cases"].as_array().unwrap() {
        let cache = CachedMetadata::parse(case["source"].as_str().unwrap());
        let actual = serde_json::to_value(cache).unwrap();
        for key in [
            "links",
            "embeds",
            "frontmatter",
            "frontmatterLinks",
            "referenceLinks",
        ] {
            if actual[key] != case["cache"][key] {
                differences.push(format!(
                    "{} {key}\nexpected {}\nactual {}",
                    case["id"], case["cache"][key], actual[key]
                ));
            }
        }
    }
    assert!(differences.is_empty(), "{}", differences.join("\n\n"));
}

#[test]
fn graph_projection_uses_captured_normalized_hrefs_without_changing_raw_spans() {
    let fixture: Value = serde_json::from_str(include_str!("fixtures/cache_unicode.json")).unwrap();
    for case in fixture["cases"].as_array().unwrap() {
        let source = case["source"].as_str().unwrap();
        let note = Note::parse(case["id"].as_str().unwrap().into(), source.into());
        let expected: Vec<_> = ["frontmatterLinks", "links", "embeds"]
            .into_iter()
            .flat_map(|key| case["cache"][key].as_array().into_iter().flatten())
            .map(|link| graph_refs::link_path(link["link"].as_str().unwrap()).to_owned())
            .collect();
        assert_eq!(graph_refs::link_targets(&note), expected, "{}", case["id"]);
        for reference in note.references() {
            assert_eq!(
                &note.body[reference.start..reference.end],
                reference.original
            );
        }
        assert_eq!(note.source, source);
    }
}
