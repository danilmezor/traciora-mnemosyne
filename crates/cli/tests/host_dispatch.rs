#![cfg(unix)]
//! Composed host-dispatch checks over captured native routing/open contracts.
//! The injected launcher verifies dispatch, not normal OS application launching.
use mnemosyne_core::CONFIG_DIR;
use mnemosyne_engine::{host::VaultHost, server::CliRequest, vault_registry::VaultRegistry};
use serde_json::json;
use std::{
    fs,
    path::{Path, PathBuf},
};
struct Scratch(PathBuf);
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
#[test]
fn existing_paths_dispatch_before_cli_gate_and_keep_vaults_and_external_reads() {
    let root =
        Scratch(std::env::temp_dir().join(format!("mn-host-dispatch-{}", std::process::id())));
    fs::create_dir_all(root.0.join("Alpha/Nested").join(CONFIG_DIR)).unwrap();
    fs::create_dir_all(root.0.join("Alpha").join(CONFIG_DIR)).unwrap();
    fs::write(root.0.join("Alpha/Note.md"), "base\n").unwrap();
    fs::write(root.0.join("Alpha/Nested/Note.md"), "nested\n").unwrap();
    fs::write(root.0.join("Alpha/Plain.txt"), "text").unwrap();
    fs::write(root.0.join("External.md"), "\u{feff}external\n").unwrap();
    let registry = VaultRegistry::from_document(
        json!({"cli":false,"vaults":{
            "base":{"path":root.0.join("Alpha"),"open":true},
            "nested":{"path":root.0.join("Alpha/Nested"),"open":true}
        }}),
        &root.0,
        None,
    )
    .unwrap();
    let mut host = VaultHost::from_registry(registry);
    host.restore_open_vaults();
    let request = |path: &Path| CliRequest {
        argv: vec!["eval".into(), path.to_string_lossy().into_owned()],
        tty: false,
        cwd: root.0.to_string_lossy().into_owned(),
    };
    let nested = root.0.join("Alpha/Nested/Note.md");
    let output =
        mnemosyne::host_dispatch::execute_with_launcher(&mut host, &request(&nested), |_| {
            panic!("Markdown must not use OS launcher")
        });
    assert_eq!(output, format!("Opened {}\n", nested.display()));
    assert_eq!(
        host.engine("nested").unwrap().active_file(),
        Some("Note.md")
    );
    assert_eq!(host.engine("base").unwrap().active_file(), None);
    let plain = root.0.join("Alpha/Plain.txt");
    let mut launches = Vec::new();
    let output =
        mnemosyne::host_dispatch::execute_with_launcher(&mut host, &request(&plain), |path| {
            launches.push(path.to_owned());
            Ok(())
        });
    assert_eq!(output, format!("Opened {}\n", plain.display()));
    assert_eq!(launches, [plain.clone()]);
    assert_eq!(host.engine("base").unwrap().active_file(), None);
    // An OS-side failure does not replace native Pt's acknowledgement bytes.
    let output =
        mnemosyne::host_dispatch::execute_with_launcher(&mut host, &request(&plain), |_| {
            anyhow::bail!("injected launch failure")
        });
    assert_eq!(output, format!("Opened {}\n", plain.display()));
    assert!(host
        .registry()
        .diagnostics
        .iter()
        .any(|s| s.contains("injected launch failure")));
    let external = root.0.join("External.md");
    assert_eq!(
        mnemosyne::host_dispatch::execute_with_launcher(
            &mut host,
            &request(&external),
            |_| panic!("external Markdown must not use OS launcher")
        ),
        format!("Opened {}\n", external.display())
    );
    assert_eq!(
        host.engine("base").unwrap().active_file(),
        Some(format!("file:{}", external.display()).as_str())
    );
    host.set_cli_enabled(true);
    let read = CliRequest {
        argv: vec!["vault=base".into(), "read".into()],
        tty: false,
        cwd: root.0.to_string_lossy().into_owned(),
    };
    assert_eq!(
        mnemosyne::host_dispatch::execute(&mut host, &read),
        "external\n"
    );
    assert_eq!(
        host.engine("nested").unwrap().active_file(),
        Some("Note.md")
    );
}
