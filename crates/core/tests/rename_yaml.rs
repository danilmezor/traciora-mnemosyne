//! Expected outputs were captured from the desktop app in an isolated profile.

use mnemosyne_core::CONFIG_DIR;
use mnemosyne_core::Vault;
use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::PathBuf;

#[derive(Deserialize)]
struct Fixtures {
    reference_version: String,
    reference_app_js_sha256: String,
    cases: Vec<Case>,
}

#[derive(Deserialize)]
struct Case {
    id: String,
    old: String,
    new: String,
    format: String,
    files: BTreeMap<String, String>,
    expected: BTreeMap<String, String>,
}

struct Scratch(PathBuf);

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn rename_matches_19_whole_header_yaml_runtime_cases() {
    // JSON is also valid YAML; reuse the core's existing parser dependency.
    let fixtures: Fixtures =
        serde_yaml::from_str(include_str!("fixtures/rename_yaml.json")).unwrap();
    assert_eq!(fixtures.reference_version, "1.14.4");
    assert_eq!(
        fixtures.reference_app_js_sha256,
        "958ce57fe22f87fdf121546e46dfccd9c8ee971ce3fd8f801d60d4ece39a2df9"
    );
    assert_eq!(fixtures.cases.len(), 19);
    let mut failures = Vec::new();
    for case in fixtures.cases {
        let scratch = Scratch(std::env::temp_dir().join(format!(
            "mnemosyne-rename-yaml-{}-{}",
            std::process::id(),
            case.id
        )));
        fs::create_dir_all(&scratch.0).unwrap();
        for (path, content) in &case.files {
            let destination = scratch.0.join(path);
            fs::create_dir_all(destination.parent().unwrap()).unwrap();
            fs::write(destination, content).unwrap();
        }
        fs::create_dir_all(scratch.0.join(CONFIG_DIR)).unwrap();
        fs::write(
            scratch.0.join(CONFIG_DIR).join("app.json"),
            format!(
                "{{\"alwaysUpdateLinks\":true,\"newLinkFormat\":\"{}\"}}",
                case.format
            ),
        )
        .unwrap();
        // The captured reference probe explicitly created this folder before
        // invoking fileManager.renameFile; rename itself does not create it.
        if let Some(parent) = PathBuf::from(&case.new).parent() {
            fs::create_dir_all(scratch.0.join(parent)).unwrap();
        }
        let mut vault = Vault::open(&scratch.0).unwrap();
        let edits = vault.rename_with_edits(&case.old, &case.new).unwrap();
        for record in edits {
            let original = case
                .files
                .get(&record.path)
                .or_else(|| {
                    (record.path == case.new)
                        .then(|| case.files.get(&case.old))
                        .flatten()
                })
                .unwrap();
            let mut reconstructed = original.clone();
            for edit in record.edits.iter().rev() {
                reconstructed.replace_range(edit.start..edit.end, &edit.text);
            }
            assert_eq!(
                reconstructed,
                fs::read_to_string(scratch.0.join(&record.path)).unwrap(),
                "{} source-edit handoff for {}",
                case.id,
                record.path
            );
        }

        let expected_files: BTreeSet<String> = case.expected.keys().cloned().collect();
        assert_eq!(vault.files(), &expected_files, "{}: file index", case.id);
        assert!(!scratch.0.join(&case.old).exists(), "{}: old file", case.id);
        for (path, content) in &case.expected {
            let actual = fs::read_to_string(scratch.0.join(path)).unwrap();
            if actual != *content {
                failures.push(format!(
                    "{} {path}: actual {actual:?}, expected {content:?}",
                    case.id
                ));
            }
            if let Some(note) = vault.get(path) {
                assert_eq!(note.source, actual, "{}: memory content", case.id);
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}
