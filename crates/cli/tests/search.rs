use mnemosyne_core::Note;
use serde::Deserialize;
use std::collections::BTreeMap;

#[derive(Deserialize)]
struct Manifest {
    reference_version: String,
    phases: BTreeMap<String, Phase>,
    cases: Vec<Case>,
}
#[derive(Deserialize)]
struct Phase {
    files: BTreeMap<String, String>,
    order: Vec<String>,
}
#[derive(Deserialize)]
struct Case {
    id: String,
    phase: String,
    args: Vec<String>,
    expected: Expected,
}
#[derive(Deserialize)]
struct Expected {
    stdout: String,
    stderr: String,
    exit_code: i32,
}

#[test]
fn search_matches_captured_reference_queries_and_pipe_bytes() {
    let manifest: Manifest =
        serde_json::from_str(include_str!("../../core/tests/fixtures/search.json")).unwrap();
    assert_eq!(manifest.reference_version, "1.14.4");
    let mut failures = Vec::new();
    for case in &manifest.cases {
        let phase = &manifest.phases[&case.phase];
        let notes: Vec<_> = phase
            .order
            .iter()
            .map(|path| Note::parse(path.into(), phase.files[path].clone()))
            .collect();
        let mut params = mnemosyne::search::Params::new();
        for arg in case.args.iter().skip(1) {
            if let Some((key, value)) = arg.split_once('=') {
                params.insert(key.to_owned(), Some(value.to_owned()));
            } else {
                params.insert(arg.clone(), None);
            }
        }
        assert_eq!(case.expected.stderr, "");
        assert_eq!(case.expected.exit_code, 0);
        let command = mnemosyne::args::prepare_command(&case.args[0], &mut params);
        let actual = mnemosyne::search::response(&notes, &params, command == "search:context");
        let caches: Vec<_> = notes
            .iter()
            .map(mnemosyne_core::cache::CachedMetadata::from_note)
            .collect();
        assert_eq!(
            mnemosyne::search::response_cached(
                notes.iter().zip(&caches),
                &params,
                command == "search:context"
            ),
            actual,
            "{} retained-cache query result",
            case.id
        );
        if actual != case.expected.stdout {
            failures.push(format!(
                "{} {:?}\nexpected: {:?}\n  actual: {:?}",
                case.id, case.args, case.expected.stdout, actual
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "{} of {} captured cases differ:\n{}",
        failures.len(),
        manifest.cases.len(),
        failures.join("\n")
    );
}
