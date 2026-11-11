use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use serde::Deserialize;

#[derive(Deserialize)]
struct Manifest {
    schema_version: u32,
    cases: Vec<Case>,
}

#[derive(Deserialize)]
struct Case {
    id: String,
    initial: Tree,
    steps: Vec<Step>,
}

#[derive(Deserialize)]
struct Step {
    args: Vec<String>,
    expected: ProcessResult,
    after: Tree,
}

#[derive(Deserialize)]
struct ProcessResult {
    stdout: String,
    stderr: String,
    exit_code: i32,
}

#[derive(Default, Deserialize)]
struct Tree {
    #[serde(default)]
    files: BTreeMap<String, Contents>,
    #[serde(default)]
    directories: BTreeSet<String>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum Contents {
    Text(String),
    Bytes(Vec<u8>),
}

impl Contents {
    fn bytes(&self) -> &[u8] {
        match self {
            Self::Text(text) => text.as_bytes(),
            Self::Bytes(bytes) => bytes,
        }
    }
}

struct Sandbox(PathBuf);

impl Sandbox {
    fn new(case: &str) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let sequence = NEXT.fetch_add(1, Ordering::Relaxed);
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "mnemosyne-cli-{case}-{}-{stamp}-{sequence}",
            std::process::id()
        ));
        fs::create_dir(&root).unwrap();
        Self(root)
    }

    fn vault(&self) -> PathBuf {
        self.0.join("vault")
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn relative(root: &Path, path: &str) -> PathBuf {
    let path = Path::new(path);
    assert!(
        path.components()
            .all(|part| matches!(part, std::path::Component::Normal(_))),
        "fixture path must be vault-relative: {path:?}"
    );
    root.join(path)
}

fn populate(root: &Path, tree: &Tree) {
    fs::create_dir(root).unwrap();
    for directory in &tree.directories {
        fs::create_dir_all(relative(root, directory)).unwrap();
    }
    for (path, contents) in &tree.files {
        let path = relative(root, path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents.bytes()).unwrap();
    }
}

fn snapshot(root: &Path) -> (BTreeMap<String, Vec<u8>>, BTreeSet<String>) {
    fn walk(
        root: &Path,
        folder: &Path,
        files: &mut BTreeMap<String, Vec<u8>>,
        directories: &mut BTreeSet<String>,
    ) {
        for entry in fs::read_dir(folder).unwrap() {
            let entry = entry.unwrap();
            let path = entry.path();
            let name = path
                .strip_prefix(root)
                .unwrap()
                .to_str()
                .unwrap()
                .replace('\\', "/");
            let kind = entry.file_type().unwrap();
            if name == ".mnemosyne" || name.starts_with(".mnemosyne/") {
                continue;
            }
            if kind.is_dir() {
                directories.insert(name);
                walk(root, &path, files, directories);
            } else {
                assert!(kind.is_file(), "unexpected fixture entry: {path:?}");
                files.insert(name, fs::read(&path).unwrap());
            }
        }
    }

    let mut files = BTreeMap::new();
    let mut directories = BTreeSet::new();
    walk(root, root, &mut files, &mut directories);
    (files, directories)
}

#[test]
fn forced_local_and_permanent_delete_match_reference_fixtures() {
    let manifest: Manifest =
        serde_json::from_str(include_str!("fixtures/file_operations.json")).unwrap();
    assert_eq!(manifest.schema_version, 1);
    assert!(!manifest.cases.is_empty());
    let mut ids = BTreeSet::new();

    for case in manifest.cases {
        assert!(ids.insert(case.id.clone()), "duplicate case: {}", case.id);
        let sandbox = Sandbox::new(&case.id);
        let vault = sandbox.vault();
        populate(&vault, &case.initial);
        for (number, step) in case.steps.iter().enumerate() {
            let label = format!("{} step {} {:?}", case.id, number + 1, step.args);
            // The reference fixtures explicitly force system-trash failure.
            // Inject the same boundary rather than moving ordinary fixture
            // basenames into the real user's OS trash through a CLI process.
            let mut engine = mnemosyne_engine::Engine::open(&vault).unwrap();
            let (_, params) = mnemosyne::args::parse_argv(&step.args);
            let response =
                mnemosyne::deletion::execute_with_trash(&mut engine, &params, |_| false).unwrap();
            assert_eq!(step.expected.exit_code, 0, "{label}: exit");
            assert_eq!(
                mnemosyne::registry::frame_output(&response).as_bytes(),
                step.expected.stdout.as_bytes(),
                "{label}: stdout"
            );
            assert_eq!(b"", step.expected.stderr.as_bytes(), "{label}: stderr");
            assert!(sandbox.0.is_dir(), "{label}: removed the sandbox parent");
            assert!(vault.is_dir(), "{label}: removed the vault root");

            let (files, directories) = snapshot(&vault);
            let expected_files: BTreeMap<_, _> = step
                .after
                .files
                .iter()
                .map(|(path, contents)| (path.clone(), contents.bytes().to_vec()))
                .collect();
            assert_eq!(files, expected_files, "{label}: file tree and bytes");
            assert_eq!(directories, step.after.directories, "{label}: directories");
        }
    }
}
