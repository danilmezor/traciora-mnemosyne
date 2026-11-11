use mnemosyne_runtime::{NodeChild, NodeChildConfig, NodeIdentity};
use std::{
    collections::BTreeMap,
    fs,
    path::PathBuf,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
pub fn launch() -> (NodeChild, u64, PathBuf) {
    let repo = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap();
    let proof = repo.join("target/rea/runtime-eval/electron-node");
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let report_dir = repo.join(format!("target/rea/engine-widgets/rust-{stamp}"));
    fs::create_dir_all(report_dir.join("vault")).unwrap();
    let addon = report_dir.join("addon.node");
    fs::copy(
        repo.join("target/runtime-build/debug/libmnemosyne_runtime_addon.dylib"),
        &addon,
    )
    .unwrap();
    let context = stamp as u64;
    let child = NodeChild::launch(NodeChildConfig {
        executable: proof.join("dist/Electron.app/Contents/MacOS/Electron"),
        framework: proof.join(
            "dist/Electron.app/Contents/Frameworks/Electron Framework.framework/Electron Framework",
        ),
        addon,
        profile: report_dir.join("profile"),
        vault: report_dir.join("vault"),
        identity: NodeIdentity {
            executable_sha256: "1121571fb0838d50519c2f002780b2b69f285d37993cf5622ee4b88c6f4b5ce4"
                .into(),
            framework_sha256: "be1b9531ce2218021027aab470e1c188c975d1525b176992c160499cb860838c"
                .into(),
            versions: BTreeMap::from([
                ("node".into(), "24.21.0".into()),
                ("electron".into(), "43.7.7".into()),
                ("v8".into(), "15.0.245.31-electron.0".into()),
                ("icu".into(), "78.2".into()),
            ]),
        },
        context,
        startup_timeout: Duration::from_secs(15),
    })
    .unwrap();
    eprintln!("widget replay owned runtime PID={}", child.pid());
    (child, context, report_dir)
}
