use mnemosyne_core::cache::{CachedMetadata, SourcePositions};

#[test]
fn cache_locations_count_full_source_utf16_units() {
    let source = "\u{feff}---\r\ntitle: 😀 café\r\n---\r\n[[Note]] 😀";
    let index = SourcePositions::new(source);
    let start = source.find("[[Note]]").unwrap();
    let location = index.loc(start);
    assert_eq!(location.line, 3);
    assert_eq!(location.col, 0);
    assert_eq!(location.offset, source[..start].encode_utf16().count() - 1);
    let end = index.loc(source.len());
    assert_eq!(end.col, "[[Note]] 😀".encode_utf16().count());
    assert_eq!(end.offset, source.encode_utf16().count() - 1);
    for (byte, _) in source.char_indices().filter(|(byte, _)| *byte >= 3) {
        assert_eq!(
            index.byte_at_utf16_offset(index.loc(byte).offset),
            Some(byte)
        );
    }
    assert_eq!(index.byte_at_utf16_offset(end.offset), Some(source.len()));
    let emoji = source.find('😀').unwrap();
    assert_eq!(
        index.byte_at_utf16_offset(index.loc(emoji).offset + 1),
        None
    );
}

#[derive(serde::Deserialize)]
struct FixtureManifest {
    reference_version: String,
    reference_app_js_sha256: String,
    cases: Vec<CacheCase>,
}

#[derive(serde::Deserialize)]
struct CacheCase {
    id: String,
    source: String,
    expected: serde_yaml::Value,
}

#[test]
fn cache_matches_captured_metadata() {
    let manifest: FixtureManifest =
        serde_yaml::from_str(include_str!("fixtures/cache.json")).unwrap();
    assert_eq!(manifest.reference_version, "1.14.4");
    assert_eq!(
        manifest.reference_app_js_sha256,
        "958ce57fe22f87fdf121546e46dfccd9c8ee971ce3fd8f801d60d4ece39a2df9"
    );
    assert_eq!(manifest.cases.len(), 12);
    let mut differences = Vec::new();
    for case in manifest.cases {
        let actual = serde_yaml::to_value(CachedMetadata::parse(&case.source)).unwrap();
        if actual != case.expected {
            let keys = actual
                .as_mapping()
                .unwrap()
                .keys()
                .chain(case.expected.as_mapping().unwrap().keys())
                .collect::<std::collections::HashSet<_>>();
            let mismatches = keys
                .into_iter()
                .filter(|key| actual.get(*key) != case.expected.get(*key))
                .collect::<Vec<_>>();
            eprintln!("{} differs in {:?}", case.id, mismatches);
            let destination = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("../../target/rea/cache-probes");
            std::fs::create_dir_all(&destination).unwrap();
            std::fs::write(
                destination.join(format!("{}-actual.yaml", case.id)),
                serde_yaml::to_string(&actual).unwrap(),
            )
            .unwrap();
            differences.push(case.id);
        }
    }
    assert!(differences.is_empty(), "cache mismatches: {differences:?}");
}

#[test]
fn cache_omits_absent_fields() {
    let empty = serde_yaml::to_value(CachedMetadata::parse("")).unwrap();
    assert!(empty.as_mapping().unwrap().is_empty());
    let note = CachedMetadata::parse("# Heading\n");
    assert_eq!(note.headings[0].position.end.col, 9);
    assert_eq!(note.sections[0].position.end.col, 9);
    assert_eq!(note.headings[0].position.end.line, 0);
}

#[test]
fn malformed_uri_images_use_raw_destination_while_links_are_unconverted() {
    let cache = CachedMetadata::parse("[link](bad%zz.md) ![image](bad%zz.png)\n");
    assert!(cache.links.is_empty());
    assert_eq!(cache.embeds.len(), 1);
    assert_eq!(cache.embeds[0].link, "bad%zz.png");
}

#[test]
fn comment_and_footnote_syntax_inside_code_stays_code() {
    let cache = CachedMetadata::parse("`^[inline]`\n\n```md\n%%\n^[inline]\n%%\n```\n");
    assert!(cache.footnotes.is_empty());
    assert_eq!(cache.sections.len(), 2);
    assert_eq!(cache.sections[1].kind, "code");
}
