use mnemosyne::{deletion, registry::frame_output};
use mnemosyne_engine::Engine;
use serde_json::Value;
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};
struct Fixture(PathBuf);
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
fn bytes(value: &Value) -> Vec<u8> {
    if let Some(text) = value.as_str() {
        text.as_bytes().to_vec()
    } else {
        value
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_u64().unwrap() as u8)
            .collect()
    }
}
fn fixture(initial: &Value) -> Fixture {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let root = std::env::temp_dir().join(format!(
        "mn-delete-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir(&root).unwrap();
    for folder in initial["directories"].as_array().into_iter().flatten() {
        std::fs::create_dir_all(root.join(folder.as_str().unwrap())).unwrap();
    }
    for (name, data) in initial["files"].as_object().unwrap() {
        let path = root.join(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, bytes(data)).unwrap();
    }
    Fixture(root)
}
fn tree(root: &Path) -> (BTreeMap<String, Vec<u8>>, BTreeSet<String>) {
    fn walk(
        root: &Path,
        dir: &Path,
        files: &mut BTreeMap<String, Vec<u8>>,
        folders: &mut BTreeSet<String>,
    ) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            let name = path.strip_prefix(root).unwrap().to_str().unwrap();
            if name == ".mnemosyne" || name.starts_with(".mnemosyne/") {
                continue;
            }
            if path.is_dir() {
                folders.insert(name.into());
                walk(root, &path, files, folders)
            } else {
                files.insert(name.into(), std::fs::read(path).unwrap());
            }
        }
    }
    let mut files = BTreeMap::new();
    let mut folders = BTreeSet::new();
    walk(root, root, &mut files, &mut folders);
    (files, folders)
}
#[test]
fn forced_local_fallback_and_permanent_delete_match_ten_native_steps() {
    let manifest: Value =
        serde_json::from_str(include_str!("fixtures/file_operations.json")).unwrap();
    let mut steps = 0;
    for case in manifest["cases"].as_array().unwrap() {
        let fixture = fixture(&case["initial"]);
        for step in case["steps"].as_array().unwrap() {
            // Reopening also proves .trash collision lookup consults disk.
            let mut engine = Engine::open(&fixture.0).unwrap();
            let argv: Vec<String> = serde_json::from_value(step["args"].clone()).unwrap();
            let (_, params) = mnemosyne::args::parse_argv(&argv);
            let mut attempted = false;
            let response = deletion::execute_with_trash(&mut engine, &params, |path| {
                attempted = true;
                assert!(path.starts_with(&fixture.0));
                false
            })
            .unwrap();
            assert_eq!(
                frame_output(&response),
                step["expected"]["stdout"],
                "{}",
                case["id"]
            );
            assert_eq!(attempted, !params.contains_key("permanent"));
            let (files, dirs) = tree(&fixture.0);
            let expected_files = step["after"]["files"]
                .as_object()
                .unwrap()
                .iter()
                .map(|(name, data)| (name.clone(), bytes(data)))
                .collect();
            let expected_dirs = step["after"]["directories"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|v| v.as_str().unwrap().to_owned())
                .collect();
            assert_eq!(files, expected_files, "{} bytes", case["id"]);
            assert_eq!(dirs, expected_dirs, "{} folders", case["id"]);
            steps += 1;
        }
    }
    assert_eq!(steps, 10);
}

/// Explicitly invoked with a proven isolated native session. Every name and
/// byte string is newly authored; cleanup addresses exact owned names only.
#[cfg(target_os = "macos")]
#[test]
#[ignore = "requires assigned native oracle interval and owned trash fixture descriptor"]
fn normal_macos_trash_matches_isolated_native_files() {
    verify_normal_macos_trash(false)
}

#[cfg(target_os = "macos")]
#[test]
#[ignore = "requires assigned native oracle interval and owned trash fixture descriptor"]
fn normal_macos_binary_delete_matches_isolated_native_files() {
    verify_normal_macos_trash(true)
}

#[cfg(target_os = "macos")]
fn verify_normal_macos_trash(binary: bool) {
    use objc2_foundation::{NSFileManager, NSSearchPathDirectory, NSSearchPathDomainMask, NSURL};
    use std::process::Command;
    static ORACLE: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _serial = ORACLE.lock().unwrap();
    struct OwnedTrash {
        path: PathBuf,
        bytes: Vec<u8>,
    }
    impl Drop for OwnedTrash {
        fn drop(&mut self) {
            if std::fs::read(&self.path).ok().as_deref() == Some(self.bytes.as_slice()) {
                let _ = std::fs::remove_file(&self.path);
            }
        }
    }
    let descriptor = std::env::var("MNEMOSYNE_TRASH_FIXTURE").unwrap();
    let descriptor: Value = serde_json::from_slice(&std::fs::read(descriptor).unwrap()).unwrap();
    let session = &descriptor["session"];
    let vault = PathBuf::from(session["vault"].as_str().unwrap());
    let boundary = PathBuf::from(descriptor["boundary"].as_str().unwrap());
    assert_eq!(vault.parent().unwrap(), boundary);
    assert!(boundary.starts_with(
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/rea/trash-routing")
            .canonicalize()
            .unwrap()
    ));
    assert_eq!(descriptor["proof"]["vault"], session["vault"]);
    assert_eq!(descriptor["proof"]["use_mock_keychain"], true);
    let uid = descriptor["uid"].as_str().unwrap();
    assert_eq!(uid.len(), 32);
    assert!(uid.chars().all(|c| c.is_ascii_hexdigit()));
    let native = |args: &[String]| {
        let result = Command::new(session["executable"].as_str().unwrap())
            .args(args)
            .env("HOME", session["home"].as_str().unwrap())
            .env("TZ", "UTC")
            .current_dir(&vault)
            .output()
            .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(350));
        result
    };
    let eval = |code: &str| {
        let output = native(&["eval".into(), format!("code={code}")]);
        assert!(output.status.success());
        assert!(output.stderr.is_empty());
        let text = String::from_utf8(output.stdout).unwrap();
        serde_json::from_str::<Value>(text.trim().strip_prefix("=> ").unwrap()).unwrap()
    };
    let rust_root = boundary.join(if binary {
        "rust-binary-vault"
    } else {
        "rust-vault"
    });
    std::fs::create_dir(&rust_root).unwrap();
    let candidate_home = boundary.join("candidate-home");
    if binary {
        std::fs::create_dir(&candidate_home).unwrap();
    }
    let mut records = Vec::new();
    for (index, extension) in [".md", ".png", "", ".tar.gz"].into_iter().enumerate() {
        let mut payload = format!("Owned trash fixture {uid} {index}\n").into_bytes();
        if index > 0 {
            payload.extend([0, 255, 128]);
        }
        let mode = if binary { "binary" } else { "library" };
        let native_name = format!("rea-owned-{uid}-{mode}-native-{index}{extension}");
        let rust_name = format!("rea-owned-{uid}-{mode}-rust-{index}{extension}");
        let native_path = vault.join(&native_name);
        let rust_path = rust_root.join(&rust_name);
        std::fs::write(&native_path, &payload).unwrap();
        std::fs::write(&rust_path, &payload).unwrap();
        let directory = objc2::rc::autoreleasepool(|_| {
            let url = NSURL::from_file_path(&native_path).unwrap();
            NSFileManager::defaultManager()
                .URLForDirectory_inDomain_appropriateForURL_create_error(
                    NSSearchPathDirectory::TrashDirectory,
                    NSSearchPathDomainMask::UserDomainMask,
                    Some(&url),
                    false,
                )
                .unwrap()
                .to_file_path()
                .unwrap()
        });
        let native_trash = directory.join(&native_name);
        let rust_trash = directory.join(&rust_name);
        assert!(!native_trash.exists() && !rust_trash.exists());
        let _native_cleanup = OwnedTrash {
            path: native_trash.clone(),
            bytes: payload.clone(),
        };
        let _rust_cleanup = OwnedTrash {
            path: rust_trash.clone(),
            bytes: payload.clone(),
        };
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        while !eval(&format!(
            "JSON.stringify(!!app.vault.getFileByPath({}))",
            serde_json::to_string(&native_name).unwrap()
        ))
        .as_bool()
        .unwrap()
        {
            assert!(std::time::Instant::now() < deadline);
        }
        let output = native(&["delete".into(), format!("path={native_name}")]);
        let expected = format!("Moved to trash: {native_name}\n");
        assert!(output.status.success());
        assert_eq!(output.stdout, expected.as_bytes());
        assert!(output.stderr.is_empty());
        assert_eq!(std::fs::read(&native_trash).unwrap(), payload);
        assert!(!native_path.exists());
        assert!(!vault.join(".trash").exists());
        let candidate_stdout = if binary {
            let output = Command::new(env!("CARGO_BIN_EXE_mnemosyne"))
                .args(["delete", &format!("path={rust_name}")])
                .env("HOME", &candidate_home)
                .env("MNEMOSYNE_VAULT", &rust_root)
                .env_remove("MNEMOSYNE_SOCKET")
                .current_dir(&rust_root)
                .output()
                .unwrap();
            assert!(output.status.success());
            assert!(output.stderr.is_empty());
            String::from_utf8(output.stdout).unwrap()
        } else {
            let mut engine = Engine::open(&rust_root).unwrap();
            let mut params = mnemosyne::search::Params::new();
            params.insert("path".into(), Some(rust_name.clone()));
            frame_output(&deletion::execute(&mut engine, &params).unwrap())
        };
        assert_eq!(candidate_stdout, format!("Moved to trash: {rust_name}\n"));
        assert_eq!(std::fs::read(&rust_trash).unwrap(), payload);
        assert!(!rust_path.exists());
        assert!(!rust_root.join(".trash").exists());
        // Content equality and the unique descriptor name prove ownership;
        // no directory enumeration or other trash entry is touched.
        std::fs::remove_file(&native_trash).unwrap();
        std::fs::remove_file(&rust_trash).unwrap();
        records.push(serde_json::json!({"id":index,"extension":extension,"native_stdout":expected,"rust_stdout":candidate_stdout,"native_stderr":"","exit_code":0,"bytes":payload,"normal_system_success":true,"local_trash_absent":true,"owned_cleanup_complete":true}));
    }
    let report = serde_json::json!({"reference_version":"1.14.4","rust_mode":if binary {"binary"} else {"library"},"session":session,"records":records});
    std::fs::write(
        boundary.parent().unwrap().join(if binary {
            "normal-binary-runtime.json"
        } else {
            "normal-runtime.json"
        }),
        serde_json::to_vec_pretty(&report).unwrap(),
    )
    .unwrap();
}
